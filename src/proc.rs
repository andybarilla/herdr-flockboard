//! Bounded external command execution. `Command::output` waits forever, so
//! every subprocess the dashboard spawns (`gh`, `herdr`, `git`) goes through
//! `run_with_timeout`: a hung command becomes an inline error instead of a
//! frozen board. Arguments are passed straight to exec — no shell is
//! involved, so there is no interpolation or injection surface.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// How often the child is polled while waiting for it to exit.
const POLL: Duration = Duration::from_millis(10);

/// Runs `command` to completion, collecting stdout/stderr, or kills and
/// reaps it after `timeout` and returns an error. Both pipes are drained on
/// their own threads so a chatty child cannot fill a pipe buffer and
/// deadlock against the wait.
pub fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning command")?;
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting on child")? {
            break status;
        }
        if Instant::now() >= deadline {
            // Kill and reap so no orphan or zombie is left behind; the
            // reader threads finish on their own once the pipes close.
            let _ = child.kill();
            let _ = child.wait();
            bail!("command timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(POLL);
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
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
    fn missing_binary_is_a_spawn_error() {
        let mut cmd = Command::new("flockboard-no-such-binary");
        let err = run_with_timeout(&mut cmd, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("spawning"));
    }
}
