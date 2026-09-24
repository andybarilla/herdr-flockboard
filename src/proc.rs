//! Bounded external command execution. `Command::output` waits forever, so
//! every subprocess the dashboard spawns (`gh`, `herdr`, `git`) goes through
//! `run_with_timeout`: a hung command becomes an inline error instead of a
//! frozen board. Arguments are passed straight to exec — no shell is
//! involved, so there is no interpolation or injection surface. Stdin is
//! always `/dev/null`, so no child can read the dashboard's raw-mode
//! terminal: UI keypresses stay with the TUI and a command that would
//! prompt sees EOF instead of blocking on interactive input.

use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// How often the child is polled while waiting for it to exit; also the
/// bounded wait inside the pipe-drain loops, so a cancellation request is
/// observed within one tick.
const POLL: Duration = Duration::from_millis(10);

/// How long teardown polls `try_wait` to reap the direct child after
/// signaling it — after a successful group kill and on the direct-child
/// fallback path alike. SIGKILL delivery does not prove the child has
/// exited, and an unbounded `Child::wait` could hang past the timeout
/// contract, so a child still running when the grace expires is reported
/// and left to the OS.
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
/// child of ours outlives the call. Reaping the direct child is bounded
/// too: SIGKILL delivery does not prove the child has exited, so teardown
/// polls `try_wait` under a grace deadline instead of `Child::wait` and
/// reports a child still running when the grace expires. If the group
/// kill itself fails for a non-ESRCH reason (e.g. EPERM from a setuid
/// child), teardown falls back to SIGKILLing the direct child only —
/// never signaling anything outside our own process group — and every
/// teardown failure is chained onto the timeout or wait error that
/// initiated teardown rather than silently dropped. The child's stdin is
/// `/dev/null`, never the inherited TUI terminal, so no subprocess can
/// consume dashboard keypresses or wait for a prompt.
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
        // Never inherit the dashboard's raw-mode terminal: a child
        // reading stdin would consume UI keypresses or block waiting for
        // interactive input.
        .stdin(Stdio::null())
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
                // cancel/join the readers and terminate the tree, bounded
                // as in the timeout path — so nothing outlives the error
                // return, and keep any teardown failure chained onto the
                // wait error instead of silently dropping it.
                return Err(wait_failed(err, &mut child, readers, &kill_group));
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
/// every inherited writer and the direct child is reaped within the
/// grace; any kill or reap failure is reported alongside the timeout, so
/// the caller sees that the tree may still be running or unreaped.
fn teardown_timed_out(
    child: &mut Child,
    readers: Readers,
    kill_group: &impl Fn(&Child) -> std::io::Result<()>,
    timeout: Duration,
) -> anyhow::Error {
    readers.cancel_and_join();
    let failures = terminate_tree(child, kill_group);
    if failures.is_empty() {
        return anyhow!("command timed out after {}s", timeout.as_secs());
    }
    anyhow!(
        "command timed out after {}s and tearing the process tree down \
         failed ({}); the process tree may still be running or unreaped",
        timeout.as_secs(),
        describe_failures(&failures)
    )
}

/// Builds the error for a failed `try_wait`: tears everything down first
/// (cancel/join readers, bounded tree termination, exactly as on the
/// timeout path) and chains any teardown failure onto the wait error, so
/// neither is lost — the caller must see both that the wait failed and
/// that the process tree may still be running or unreaped.
fn wait_failed(
    wait_err: std::io::Error,
    child: &mut Child,
    readers: Readers,
    kill_group: &impl Fn(&Child) -> std::io::Result<()>,
) -> anyhow::Error {
    readers.cancel_and_join();
    let failures = terminate_tree(child, kill_group);
    let err = anyhow::Error::new(wait_err).context("waiting on child");
    if failures.is_empty() {
        return err;
    }
    err.context(format!(
        "tearing the process tree down after the wait failure failed \
         ({}); the process tree may still be running or unreaped",
        describe_failures(&failures)
    ))
}

/// Why the bounded direct-child reap did not complete.
#[derive(Debug)]
enum ReapFailure {
    /// `try_wait` itself errored, so the child's state is unknown.
    Wait(std::io::Error),
    /// The grace deadline elapsed with the child still running.
    GraceExpired,
}

/// One thing that went wrong while tearing the process tree down. The
/// failures are collected in the order they happened so callers can
/// report every one of them, not just the first.
#[derive(Debug)]
enum TeardownFailure {
    /// The process-group SIGKILL failed for a non-ESRCH reason (e.g.
    /// EPERM); the whole tree may still be running.
    GroupKill(std::io::Error),
    /// The fallback SIGKILL of the direct child failed.
    ChildKill(std::io::Error),
    /// The bounded direct-child reap did not complete.
    Reap(ReapFailure),
}

impl std::fmt::Display for TeardownFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GroupKill(err) => write!(f, "killing its process group failed ({err})"),
            Self::ChildKill(err) => write!(
                f,
                "killing the direct child after the group-kill failure failed ({err})"
            ),
            Self::Reap(ReapFailure::Wait(err)) => {
                write!(f, "reaping the direct child failed ({err})")
            }
            Self::Reap(ReapFailure::GraceExpired) => write!(
                f,
                "the direct child was still running when the teardown grace expired"
            ),
        }
    }
}

/// Renders teardown failures for an error message, in order.
fn describe_failures(failures: &[TeardownFailure]) -> String {
    failures
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Polls `try_wait` until the direct child is reaped or `grace` elapses,
/// whichever comes first. Unlike `Child::wait` this can never hang: a
/// child still running past the grace (e.g. signaled but slow to die, or
/// unreapable) is reported instead of awaited forever, and a `try_wait`
/// error is surfaced rather than silently treated as reaped. The closure
/// is injectable so tests can drive grace expiry and wait errors without
/// OS-level tricks.
fn reap_bounded(
    mut try_wait: impl FnMut() -> std::io::Result<Option<ExitStatus>>,
    grace: Duration,
) -> Result<(), ReapFailure> {
    let deadline = Instant::now() + grace;
    loop {
        match try_wait() {
            Ok(Some(_)) => return Ok(()),
            Err(err) => return Err(ReapFailure::Wait(err)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(ReapFailure::GraceExpired);
                }
                std::thread::sleep(POLL);
            }
        }
    }
}

/// Terminates the child's process tree without ever waiting indefinitely
/// and reports everything that went wrong, as one list entry per failure
/// (empty means the tree is dead and the direct child reaped). The group
/// SIGKILL reaches every descendant of our own process group and no one
/// else; on a non-ESRCH failure (e.g. EPERM from a setuid child) the tree
/// may still be running, so the fallback SIGKILLs only the direct child —
/// never anything outside our own process group. Either way the direct
/// child is then reaped by `reap_bounded`, because a successful signal
/// does not prove the child exited: ESRCH is benign for group signaling
/// only, and an unbounded `Child::wait` could hang past the timeout
/// contract.
fn terminate_tree(
    child: &mut Child,
    kill_group: &impl Fn(&Child) -> std::io::Result<()>,
) -> Vec<TeardownFailure> {
    let mut failures = Vec::new();
    if let Err(kill_err) = kill_group(child) {
        failures.push(TeardownFailure::GroupKill(kill_err));
        if let Err(kill_err) = child.kill() {
            failures.push(TeardownFailure::ChildKill(kill_err));
        }
    }
    if let Err(reap) = reap_bounded(|| child.try_wait(), TEARDOWN_GRACE) {
        failures.push(TeardownFailure::Reap(reap));
    }
    failures
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
    fn child_stdin_is_dev_null_never_inherited() {
        // Regression for the TUI review finding: a child inheriting fd 0
        // could consume dashboard keypresses or block on interactive
        // input. The sentinel file is wired as this command's stdin — if
        // `run_with_timeout` ever failed to override it with
        // `Stdio::null()`, the child would see that sentinel instead:
        // the `-ef` check distinguishes it from /dev/null and the `cat`
        // check reads the sentinel bytes, while a null stdin gives
        // immediate EOF. This is per-command state only — no
        // process-global fd or env mutation — so it cannot race with
        // other tests running in parallel, and it is deterministic on
        // ANY harness stdin, including a live terminal this test never
        // reads: even a regressed child reads a regular file and exits
        // promptly, so the test can never block.
        let dir = std::env::temp_dir().join(format!(
            "flockboard-proc-stdin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = TempDir(dir.clone());
        let sentinel = dir.join("sentinel-stdin");
        std::fs::write(&sentinel, "sentinel-from-parent-stdin\n").unwrap();
        let file = std::fs::File::open(&sentinel).unwrap();

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("[ /dev/stdin -ef /dev/null ] || exit 42; [ -z \"$(cat)\" ] || exit 43")
            // Would be the child's stdin if the runner did not override
            // it; the runner must replace this with /dev/null.
            .stdin(Stdio::from(file));
        let start = Instant::now();
        let out = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        assert!(
            out.status.success(),
            "child stdin was not overridden with /dev/null: {out:?}"
        );
        // EOF is immediate; nowhere near the 5s timeout a blocked read
        // would hit.
        assert!(start.elapsed() < Duration::from_secs(2));
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
    fn reap_bounded_reports_grace_expiry_instead_of_hanging() {
        // A child that never exits (injected `try_wait` always reports
        // running): the bounded reap must give up and report, not hang
        // like the old unbounded `Child::wait`.
        let start = Instant::now();
        let result = reap_bounded(|| Ok(None), Duration::from_millis(100));
        assert!(
            matches!(result, Err(ReapFailure::GraceExpired)),
            "expected grace expiry, got {result:?}"
        );
        // ~100ms of polling, nowhere near an unbounded wait.
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn reap_bounded_reports_wait_errors() {
        // A failing `try_wait` is surfaced as a reap failure, never
        // silently treated as reaped.
        let result = reap_bounded(
            || Err(std::io::Error::from_raw_os_error(libc::EIO)),
            Duration::from_millis(100),
        );
        match result {
            Err(ReapFailure::Wait(err)) => {
                assert_eq!(err.raw_os_error(), Some(libc::EIO))
            }
            other => panic!("expected wait failure, got {other:?}"),
        }
    }

    #[test]
    fn reap_bounded_reaps_a_killed_child() {
        // The normal teardown case: a SIGKILLed child is reaped well
        // within the grace.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let mut child = cmd.spawn().unwrap();
        child.kill().unwrap();
        reap_bounded(|| child.try_wait(), TEARDOWN_GRACE).unwrap();
    }

    #[test]
    fn terminate_tree_after_successful_group_kill_is_clean() {
        // Real group kill on our own process-group-leading child: the
        // whole group is SIGKILLed and the direct child reaped within
        // the grace, so the outcome is empty.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        cmd.process_group(0);
        let mut child = cmd.spawn().unwrap();
        let failures = terminate_tree(&mut child, &kill_process_group);
        assert!(failures.is_empty(), "unexpected failures: {failures:?}");
    }

    #[test]
    fn terminate_tree_reports_group_kill_failure_and_reaps_direct_child() {
        // Injected EPERM on the group kill: the outcome names the group
        // kill failure, and the fallback still SIGKILLs and reaps the
        // direct child (no zombie, no live `sleep 30`).
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let mut child = cmd.spawn().unwrap();
        let failures = terminate_tree(&mut child, &|_| {
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        });
        assert_eq!(failures.len(), 1, "unexpected failures: {failures:?}");
        assert!(matches!(failures[0], TeardownFailure::GroupKill(_)));
        assert!(child.try_wait().unwrap().is_some(), "child not reaped");
    }

    #[test]
    fn wait_error_path_retains_both_wait_and_teardown_failures() {
        // A real `try_wait` failure is impractical to reproduce at the OS
        // level, so the error-path teardown lives in `wait_failed` and is
        // exercised here with a fabricated wait error plus an injected
        // group-kill failure: the returned error must carry both, and
        // teardown must still run (readers joined, child reaped).
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let readers = Readers::spawn(&mut child);
        let err = wait_failed(
            std::io::Error::from_raw_os_error(libc::EIO),
            &mut child,
            readers,
            &|_| Err(std::io::Error::from_raw_os_error(libc::EPERM)),
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("waiting on child"), "wait error lost: {msg}");
        assert!(
            msg.contains("killing its process group failed"),
            "teardown failure lost: {msg}"
        );
        // Teardown still happened: the fallback killed and reaped the
        // child, and `wait_failed` joined the reader threads (they owned
        // the taken pipes, so no thread outlives the call).
        assert!(child.try_wait().unwrap().is_some(), "child not reaped");
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
