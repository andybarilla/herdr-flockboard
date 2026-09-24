//! Bounded external command execution. `Command::output` waits forever, so
//! every subprocess the dashboard spawns (`gh`, `herdr`, `git`) goes through
//! `run_with_timeout`: a hung command becomes an inline error instead of a
//! frozen board. Arguments are passed straight to exec — no shell is
//! involved, so there is no interpolation or injection surface.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// How often the child is polled while waiting for it to exit.
const POLL: Duration = Duration::from_millis(10);

/// How long timeout teardown may spend waiting for the child and reader
/// threads when the process-group kill itself failed: a last bounded
/// best-effort before detaching whatever is still stuck and reporting.
const TEARDOWN_GRACE: Duration = Duration::from_secs(1);

/// Runs `command` to completion, collecting stdout/stderr, or kills it and
/// returns an error once `timeout` has elapsed. The single deadline covers
/// child execution, reader-thread drain, and process-tree teardown: the
/// child is spawned as a process-group leader, so timing out SIGKILLs the
/// whole group — a descendant that inherited the pipes cannot keep the
/// reader threads (and therefore this function) blocked after the deadline.
/// Both pipes are drained on their own threads so a chatty child cannot
/// fill a pipe buffer and deadlock against the wait; those threads are
/// always joined before returning, on success and on timeout. If the
/// process-group kill itself fails for a non-ESRCH reason (e.g. EPERM
/// from a setuid child), teardown falls back to a bounded grace period
/// and reports the failure rather than blocking forever on pipes it
/// cannot close.
pub fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        // New process group with pgid == child pid, so the whole tree can
        // be signaled without touching unrelated processes.
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning command")?;
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    // Readers report over channels so the drain below can be bounded by the
    // deadline; `JoinHandle::join` alone has no timed wait.
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting on child")? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(teardown_timed_out(
                &mut child,
                &stdout_rx,
                &stderr_rx,
                stdout_reader,
                stderr_reader,
                timeout,
            ));
        }
        std::thread::sleep(POLL);
    };

    // The direct child exited, but a descendant may still hold the pipes
    // open; wait for the readers only until the same deadline.
    let stdout = recv_bounded(&stdout_rx, deadline);
    let stderr = recv_bounded(&stderr_rx, deadline);
    let (stdout, stderr) = match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        _ => {
            // A descendant kept a pipe open past the deadline.
            return Err(teardown_timed_out(
                &mut child,
                &stdout_rx,
                &stderr_rx,
                stdout_reader,
                stderr_reader,
                timeout,
            ));
        }
    };
    // Both buffers arrived, so the reader threads are done; join to
    // confirm no thread outlives this call.
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Builds the timeout error after tearing the process tree down.
/// Normally the group SIGKILL closes every inherited pipe, so reaping
/// the direct child and joining the reader threads cannot hang. If the
/// kill fails for a non-ESRCH reason the tree may still be running and
/// holding the pipes; unconditional waits would reintroduce the very
/// hang this function exists to prevent, so teardown degrades to one
/// bounded grace period, then detaches whatever is still stuck (those
/// threads only read the pipes and own no shared state, so they finish
/// on their own once the holder exits) and reports both failures.
fn teardown_timed_out(
    child: &mut Child,
    stdout_rx: &mpsc::Receiver<Vec<u8>>,
    stderr_rx: &mpsc::Receiver<Vec<u8>>,
    stdout_reader: std::thread::JoinHandle<()>,
    stderr_reader: std::thread::JoinHandle<()>,
    timeout: Duration,
) -> anyhow::Error {
    match kill_process_group(child) {
        Ok(()) => {
            // Kill the whole tree and reap the direct child so no orphan
            // or zombie of ours is left behind; the reader joins are
            // bounded because the group kill closed every inherited pipe.
            let _ = child.wait();
            join_readers(stdout_reader, stderr_reader);
            anyhow!("command timed out after {}s", timeout.as_secs())
        }
        Err(kill_err) => {
            let _ = child.kill();
            let grace = Instant::now() + TEARDOWN_GRACE;
            while Instant::now() < grace {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(POLL),
                }
            }
            join_reported(stdout_rx, stdout_reader, grace);
            join_reported(stderr_rx, stderr_reader, grace);
            anyhow!(
                "command timed out after {}s and killing its process group \
                 failed ({kill_err}); the process tree may still be running",
                timeout.as_secs()
            )
        }
    }
}

/// SIGKILLs the child's process group (pgid == child pid, per
/// `process_group(0)` above, so only our own tree is signaled — never
/// unrelated processes). ESRCH — the group already exited — is benign
/// and reported as success; any other error (e.g. EPERM) is returned so
/// the caller does not mistake a live tree for a dead one.
fn kill_process_group(child: &Child) -> std::io::Result<()> {
    // SAFETY: kill with a valid signal number and the process-group id
    // derived from our own child is always safe to call; failure is
    // reported through the return value.
    let rc = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(err),
    }
}

/// Receives a reader thread's buffer, bounded by `deadline`. `None` means
/// the pipe was still open (a descendant is holding it) at the deadline;
/// a disconnected channel means the thread panicked and is treated as an
/// empty buffer, matching the old `unwrap_or_default` join behavior.
fn recv_bounded(rx: &mpsc::Receiver<Vec<u8>>, deadline: Instant) -> Option<Vec<u8>> {
    // recv_deadline is unstable; derive the remaining budget instead.
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(buf) => Some(buf),
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Vec::new()),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
    }
}

/// Joins a reader thread only if it reports its buffer by `deadline`.
/// A reported thread is already at its exit, so the join returns
/// immediately; a thread still stuck on a pipe is detached instead of
/// being joined forever. Used only when the group kill failed.
fn join_reported(
    rx: &mpsc::Receiver<Vec<u8>>,
    reader: std::thread::JoinHandle<()>,
    deadline: Instant,
) {
    if recv_bounded(rx, deadline).is_some() {
        let _ = reader.join();
    }
}

/// Joins both reader threads after the process group has been SIGKILLed:
/// every pipe write end is closed, so both threads finish promptly and
/// nothing outlives the timeout path.
fn join_readers(
    stdout_reader: std::thread::JoinHandle<()>,
    stderr_reader: std::thread::JoinHandle<()>,
) {
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_command_captures_output() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let out = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "hello");
    }

    #[test]
    fn failing_command_captures_stderr_and_status() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo oops >&2; exit 3");
        let out = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8(out.stderr).unwrap().trim(), "oops");
    }

    #[test]
    fn slow_command_is_killed_and_reported() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let err = run_with_timeout(&mut cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
        // The point of the timeout: we get the error promptly, not after
        // the child's own lifetime.
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Removes a test's temporary directory even when an assertion
    /// panics, so reruns never trip over leftover pid files.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn descendant_holding_pipes_cannot_outlive_timeout() {
        // The direct child (`sh`) exits immediately, but its background
        // `sleep` inherits stdout/stderr. The old implementation joined the
        // reader threads after reaping only the direct child and blocked
        // for the sleep's full 30s. sh records the background pid so the
        // test can check the descendant itself, not just elapsed time.
        let dir = std::env::temp_dir().join(format!(
            "flockboard-proc-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = TempDir(dir.clone());
        let pid_file = dir.join("descendant.pid");

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo $! > \"$1\"")
            .arg("sh")
            .arg(&pid_file);
        let start = Instant::now();
        let err = run_with_timeout(&mut cmd, Duration::from_millis(500)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
        // Bounded well before the descendant's own 30s lifetime, with a
        // wide margin for slow CI machines.
        assert!(start.elapsed() < Duration::from_secs(10));

        let pid = String::from_utf8(std::fs::read(&pid_file).unwrap())
            .unwrap()
            .trim()
            .to_string();

        // Prove the pipe-holding descendant cannot still be running,
        // portably. `kill -0` (on the pid or the group) is not usable
        // here: it succeeds for a zombie, and the orphaned descendant is
        // reaped by init whenever init gets to it — promptly on macOS,
        // possibly never in a Docker container whose PID 1 does not reap
        // — so an existence check conflates a benign zombie with a live
        // process and made this test fail on Ubuntu CI. Instead poll
        // `ps` for the descendant's state: gone (empty output) or zombie
        // (`Z...`) both mean the SIGKILL landed and it can never run
        // again; any other state means it is still alive. The bounded
        // poll absorbs the instant between kill(2) returning and the
        // process leaving the run queue.
        let ps_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = Command::new("ps")
                .arg("-o")
                .arg("stat=")
                .arg("-p")
                .arg(&pid)
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stat.is_empty() || stat.starts_with('Z') {
                return;
            }
            assert!(
                Instant::now() < ps_deadline,
                "descendant {pid} is still live (state {stat}) after the timeout"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn missing_binary_is_a_spawn_error() {
        let mut cmd = Command::new("flockboard-no-such-binary");
        let err = run_with_timeout(&mut cmd, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("spawning"));
    }
}
