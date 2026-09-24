//! Bounded external command execution. `Command::output` waits forever, so
//! every subprocess the dashboard spawns (`gh`, `herdr`, `git`) goes through
//! `run_with_timeout`: a hung command becomes an inline error instead of a
//! frozen board. Arguments are passed straight to exec — no shell is
//! involved, so there is no interpolation or injection surface.

use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// How often the child is polled while waiting for it to exit; also the
/// bounded wait inside the pipe-drain loops, so a cancellation request is
/// observed within one tick.
const POLL: Duration = Duration::from_millis(10);

/// How long the fallback teardown waits to reap the direct child when the
/// process-group kill itself failed: a last bounded best-effort before the
/// child is left to the OS and the failure is reported.
const TEARDOWN_GRACE: Duration = Duration::from_secs(1);

/// Runs `command` to completion, collecting stdout/stderr, or kills it and
/// returns an error once `timeout` has elapsed. The single deadline covers
/// child execution, reader-thread drain, and process-tree teardown. The
/// child is spawned as a process-group leader, so timing out SIGKILLs the
/// whole group; each pipe is drained on its own thread so a chatty child
/// cannot fill a pipe buffer and deadlock against the wait. The drain
/// loops are cooperatively cancelable — they `poll` with a short bounded
/// wait and check a shared flag — so joining them never depends on the
/// kill succeeding or on a descendant closing an inherited writer. Every
/// exit path after spawn (success, timeout, or error) cancels and joins
/// both readers before returning, and no helper thread or unreaped direct
/// child of ours outlives the call. If the group kill itself fails for a
/// non-ESRCH reason (e.g. EPERM from a setuid child), teardown falls back
/// to a bounded best-effort kill/reap of the direct child only — never
/// signaling anything outside our own process group — and reports both
/// failures.
pub fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    run_with(command, timeout, kill_process_group)
}

/// `run_with_timeout` with the process-group kill operation injectable, so
/// tests can simulate a non-ESRCH kill failure without unsafe OS tricks.
fn run_with(
    command: &mut Command,
    timeout: Duration,
    kill_group: impl Fn(&Child) -> std::io::Result<()>,
) -> Result<Output> {
    let mut child = command
        // New process group with pgid == child pid, so the whole tree can
        // be signaled without touching unrelated processes.
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning command")?;
    let readers = Readers::spawn(&mut child);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                // The wait itself failed: still tear everything down —
                // cancel/join the readers and best-effort-terminate the
                // child, bounded as in the timeout path — so nothing
                // outlives the error return.
                readers.cancel_and_join();
                terminate_tree(&mut child, &kill_group);
                return Err(err).context("waiting on child");
            }
        }
        if Instant::now() >= deadline {
            return Err(teardown_timed_out(
                &mut child,
                readers,
                &kill_group,
                timeout,
            ));
        }
        std::thread::sleep(POLL);
    };

    // The direct child exited, but a descendant may still hold the pipes
    // open; wait for the readers only until the same deadline.
    let stdout = recv_bounded(&readers.stdout_rx, deadline);
    let stderr = recv_bounded(&readers.stderr_rx, deadline);
    let (stdout, stderr) = match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        // A descendant kept a pipe open past the deadline.
        _ => {
            return Err(teardown_timed_out(
                &mut child,
                readers,
                &kill_group,
                timeout,
            ));
        }
    };
    // Both buffers arrived, so the reader threads are done; join to
    // confirm no thread outlives this call.
    readers.join();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// The stdout/stderr drain threads plus the cancellation flag they share.
/// Every method that consumes or joins is bounded by the drain loops'
/// poll cadence, never by a pipe a descendant may still hold open.
struct Readers {
    cancel: Arc<AtomicBool>,
    stdout_rx: mpsc::Receiver<Vec<u8>>,
    stderr_rx: mpsc::Receiver<Vec<u8>>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl Readers {
    /// Takes the piped stdout/stderr from `child` and spawns one drain
    /// thread per pipe. Readers report over channels so the main thread
    /// can wait for them with a deadline; `JoinHandle::join` alone has no
    /// timed wait.
    fn spawn(child: &mut Child) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        Self {
            cancel: cancel.clone(),
            stdout_rx,
            stderr_rx,
            stdout_thread: Some(spawn_reader(stdout_pipe, cancel.clone(), stdout_tx)),
            stderr_thread: Some(spawn_reader(stderr_pipe, cancel, stderr_tx)),
        }
    }

    /// Joins both reader threads after both buffers were received: each
    /// thread is already past its send, so the joins return immediately.
    fn join(mut self) {
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }

    /// Cancels both drain loops and joins both threads. Each loop polls
    /// with a bounded wait, so it observes the flag within one tick, drops
    /// its read end, and returns — this never blocks on a pipe, no matter
    /// what still holds the writers open.
    fn cancel_and_join(mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

/// Spawns a thread that drains `pipe` to EOF (or cancellation) and sends
/// the collected bytes over `tx`.
fn spawn_reader(
    pipe: impl Read + AsRawFd + Send + 'static,
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let buf = drain_pipe(pipe, &cancel);
        let _ = tx.send(buf);
    })
}

/// Drains `pipe` into a buffer until EOF, a read error, or cancellation.
/// The fd is polled with a bounded wait instead of blocking in `read`, so
/// a cancellation request is honored within one poll tick even while a
/// descendant keeps the write end open forever; on cancellation the loop
/// returns what it has and the caller drops the read end.
fn drain_pipe(mut pipe: impl Read + AsRawFd, cancel: &AtomicBool) -> Vec<u8> {
    let fd = pipe.as_raw_fd();
    set_nonblocking(fd);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid struct and `fd` is owned by `pipe`,
        // which outlives this call.
        let rc = unsafe { libc::poll(&mut pfd, 1, POLL.as_millis() as i32) };
        if rc < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                // A poll error we cannot retry (e.g. EBADF): stop draining
                // rather than spin.
                _ => break,
            }
        }
        if rc == 0 {
            // Poll tick elapsed with nothing to read; re-check the flag.
            continue;
        }
        // Readable, hangup, or error: exactly one read per poll event.
        // Only this thread reads the fd, so data (or EOF) reported by
        // poll cannot be consumed between the two calls; even without
        // O_NONBLOCK this read cannot block.
        match pipe.read(&mut chunk) {
            Ok(0) => break, // EOF: every writer closed its end
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    buf
}

/// Best-effort O_NONBLOCK on `fd`. The drain loop stays correct without
/// it (poll gates every read), so a failed fcntl is not fatal.
fn set_nonblocking(fd: std::os::unix::io::RawFd) {
    // SAFETY: fcntl on an open fd with F_GETFL/F_SETFL is always safe;
    // failure is reported through the return value, which we intentionally
    // ignore per the comment above.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Builds the timeout error after tearing the process tree down. The
/// readers are cancelled and joined first, so the rest never waits on a
/// pipe a descendant may still hold. Normally the group SIGKILL closes
/// every inherited writer and reaping the direct child cannot hang; if
/// the kill fails for a non-ESRCH reason, teardown degrades to a bounded
/// best-effort fallback on the direct child and reports both failures.
fn teardown_timed_out(
    child: &mut Child,
    readers: Readers,
    kill_group: &impl Fn(&Child) -> std::io::Result<()>,
    timeout: Duration,
) -> anyhow::Error {
    readers.cancel_and_join();
    match terminate_tree(child, kill_group) {
        None => anyhow!("command timed out after {}s", timeout.as_secs()),
        Some(kill_err) => anyhow!(
            "command timed out after {}s and killing its process group \
             failed ({kill_err}); the process tree may still be running",
            timeout.as_secs()
        ),
    }
}

/// Terminates the child's process tree without ever waiting indefinitely,
/// returning the group-kill error if that step failed. The group SIGKILL
/// reaches every descendant of our own process group and no one else; on
/// a non-ESRCH failure (e.g. EPERM from a setuid child) the tree may
/// still be running, so the fallback SIGKILLs only the direct child and
/// gives it one bounded grace period to be reaped — if it still has not
/// exited it is left to the OS and the caller reports the failure.
fn terminate_tree(
    child: &mut Child,
    kill_group: &impl Fn(&Child) -> std::io::Result<()>,
) -> Option<std::io::Error> {
    match kill_group(child) {
        Ok(()) => {
            // The direct child is in the killed group (or already exited
            // and its status cached); reap it so no zombie of ours is
            // left behind.
            let _ = child.wait();
            None
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
            Some(kill_err)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    #[test]
    fn reader_cancels_and_joins_with_writer_held_open() {
        // Cancellation must not depend on EOF: keep the write end open,
        // prove the drain has not finished, then cancel and prove the
        // thread reports its partial buffer and is joined — promptly and
        // deterministically, with no process kill involved.
        let (read_end, mut write_end) = std::io::pipe().unwrap();
        write_end.write_all(b"partial").unwrap();
        write_end.flush().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let reader = spawn_reader(read_end, cancel.clone(), tx);

        // The writer stays open, so there is no EOF: the drain must still
        // be running well after the bytes arrived.
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        cancel.store(true, Ordering::Relaxed);
        let start = Instant::now();
        // The next poll tick observes the flag; a wide bound absorbs slow
        // CI scheduling without hiding a real hang (an uncancelable
        // reader would never send).
        let buf = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The send already happened, so the thread is exiting; join must
        // return immediately.
        reader.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(buf, b"partial");
        drop(write_end);
    }

    #[test]
    fn group_kill_failure_falls_back_and_reports() {
        // `sh` waits on a background sleep that inherits the pipes; the
        // injected group kill fails with EPERM, exercising the fallback:
        // cancel/join the readers, SIGKILL the direct child only, bounded
        // reap, and an error that reports the kill failure. The 5s
        // descendant outlives the test briefly and exits on its own, so
        // nothing stray survives.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5 & wait");
        let start = Instant::now();
        let err = run_with(&mut cmd, Duration::from_millis(300), |_child| {
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
        assert!(
            msg.contains("killing its process group failed"),
            "kill failure not reported: {msg}"
        );
        // Bounded by reader cancellation and the direct-child fallback,
        // well before the descendant's own 5s lifetime — proof the
        // readers did not wait on inherited writers.
        assert!(start.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn group_kill_failure_with_live_child_reaps_via_fallback() {
        // Same injected EPERM, but the direct child itself is still
        // running at the deadline: the fallback must SIGKILL and reap it
        // (no zombie, no runaway `sleep 30`), still bounded, still
        // reporting the kill failure.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let err = run_with(&mut cmd, Duration::from_millis(300), |_child| {
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        })
        .unwrap_err();
        assert!(err.to_string().contains("killing its process group failed"));
        // 300ms timeout + at most the 1s reap grace; nowhere near the
        // child's own 30s lifetime.
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bursty_output_on_both_pipes_is_captured_fully() {
        // ~130KB per stream — far beyond the 64KiB pipe buffer — written
        // in a tight interleaved loop, so the nonblocking drains must
        // keep up with a writer that never pauses, without truncation or
        // deadlock.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            "i=1; while [ $i -le 20000 ]; do echo \"out-$i\"; echo \"err-$i\" >&2; i=$((i+1)); done",
        );
        let out = run_with_timeout(&mut cmd, Duration::from_secs(30)).unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8(out.stdout).unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(stdout.lines().count(), 20000);
        assert_eq!(stderr.lines().count(), 20000);
        assert!(stdout.starts_with("out-1\n"));
        assert!(stdout.trim_end().ends_with("out-20000"));
        assert!(stderr.starts_with("err-1\n"));
        assert!(stderr.trim_end().ends_with("err-20000"));
    }
}
