//! Small synchronous process helpers shared by shell-out adapters.

use std::io;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug)]
pub(crate) enum TimedOutput {
    Completed(Output),
    TimedOut,
}

/// Run a child process until it exits or `timeout` elapses.
///
/// Timed-out children are killed and reaped before this function returns. The
/// caller controls stdio on `command`; captured output is returned unchanged
/// when the child completes.
pub(crate) fn run_with_timeout(mut command: Command, timeout: Duration) -> io::Result<TimedOutput> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait()? {
            Some(_) => return child.wait_with_output().map(TimedOutput::Completed),
            None if Instant::now() >= deadline => {
                match child.kill() {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
                    Err(error) => return Err(error),
                }
                child.wait()?;
                return Ok(TimedOutput::TimedOut);
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[cfg(unix)]
    #[test]
    fn captures_output_when_process_completes() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf completed")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let result = run_with_timeout(command, Duration::from_secs(1)).unwrap();
        let TimedOutput::Completed(output) = result else {
            panic!("short command timed out");
        };
        assert!(output.status.success());
        assert_eq!(output.stdout, b"completed");
    }

    #[cfg(unix)]
    #[test]
    fn kills_and_reaps_process_after_deadline() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 10")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let started = Instant::now();
        let result = run_with_timeout(command, Duration::from_millis(75)).unwrap();
        assert!(matches!(result, TimedOutput::TimedOut));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout helper returned too slowly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn reports_spawn_failure_separately() {
        let command = Command::new("portagenty-command-that-does-not-exist");
        let error = run_with_timeout(command, Duration::from_millis(10)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
