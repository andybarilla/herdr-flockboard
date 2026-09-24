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

use anyhow::{bail, Context, Result};

/// How often the child is polled while waiting for it to exit.
const POLL: Duration = Duration::from_millis(10);

/// Runs `command` to completion, collecting stdout/stderr, or kills it and
/// returns an error once `timeout` has elapsed. The single deadline covers
/// child execution, reader-thread drain, and process-tree teardown: the
/// child is spawned as a process-group leader, so timing out SIGKILLs the
/// whole group — a descendant that inherited the pipes cannot keep the
/// reader threads (and therefore this function) blocked after the deadline.
/// Both pipes are drained on their own threads so a chatty child cannot
/// fill a pipe buffer and deadlock against the wait; those threads are
/// always joined before returning, on success and on timeout.
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
            // Kill the whole tree and reap the direct child so no orphan
            // or zombie is left behind. Killing the group closes every
            // inherited pipe, so the reader joins below cannot hang.
            kill_process_group(&child);
            let _ = child.wait();
            join_readers(stdout_reader, stderr_reader);
            bail!("command timed out after {}s", timeout.as_secs());
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
            // A descendant kept a pipe open past the deadline. Killing the
            // group closes the inherited pipes, so these joins are bounded.
            kill_process_group(&child);
            join_readers(stdout_reader, stderr_reader);
            bail!("command timed out after {}s", timeout.as_secs());
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

/// SIGKILLs the child's process group (pgid == child pid, per
/// `process_group(0)` above). ESRCH — the group already exited — is
/// benign, and any other failure is ignored: the direct child is reaped
/// separately and a missed descendant simply loses its pipe audience.
fn kill_process_group(child: &Child) {
    // SAFETY: kill with a signal number and process-group id derived from
    // the live child is always safe to call; errors are reported via the
    // ignored return value.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
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

    #[test]
    fn descendant_holding_pipes_cannot_outlive_timeout() {
        // The direct child (`sh`) exits immediately, but its background
        // `sleep` inherits stdout/stderr. The old implementation joined the
        // reader threads after reaping only the direct child and blocked
        // for the sleep's full 30s. sh records its own pid — also the
        // process-group id, since the runner makes it a group leader.
        let pid_file = std::env::temp_dir().join(format!(
            "flockboard-proc-test-{}-{}",
            std::process::id(),
            "descendant"
        ));
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo $$ > \"$1\"; sleep 30 & exit 0")
            .arg("sh")
            .arg(&pid_file);
        let start = Instant::now();
        let err = run_with_timeout(&mut cmd, Duration::from_millis(500)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
        // Bounded well before the descendant's own 30s lifetime, with a
        // wide margin for slow CI machines.
        assert!(start.elapsed() < Duration::from_secs(10));

        // The group kill happens before the timeout returns, so this is
        // deterministic rather than timing-dependent: no process in the
        // group may still exist.
        let pgid = String::from_utf8(std::fs::read(&pid_file).unwrap())
            .unwrap()
            .trim()
            .to_string();
        let _ = std::fs::remove_file(&pid_file);
        let group_alive = Command::new("kill")
            .arg("-0")
            .arg(format!("-{pgid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !group_alive.success(),
            "process group {pgid} survived the timeout"
        );
    }

    #[test]
    fn missing_binary_is_a_spawn_error() {
        let mut cmd = Command::new("flockboard-no-such-binary");
        let err = run_with_timeout(&mut cmd, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("spawning"));
    }
}
