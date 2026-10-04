//! Child processes: bounded, captured runs of the devtunnel CLI and checked
//! runs of systemd tools.

use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::process::Command;

use crate::error::{Error, Result};

/// Limit for every devtunnel CLI command.
pub(crate) const CLI_TIMEOUT: Duration = Duration::from_secs(45);

/// A finished child whose output was captured. Standard error is discarded:
/// arbitrary CLI output never reaches service logs.
#[derive(Debug)]
pub(crate) struct Captured {
	pub status: ExitStatus,
	pub stdout: Vec<u8>,
}

fn command(argv: &[OsString]) -> Command {
	let mut command = Command::new(&argv[0]);
	command.args(&argv[1..]).stdin(Stdio::null());
	command
}

fn spawn_error(argv: &[OsString], error: &std::io::Error) -> Error {
	Error::io(error, Some(Path::new(&argv[0])))
}

/// Runs `argv` with standard input closed and both output streams captured,
/// and kills it once `timeout` elapses.
pub(crate) async fn capture(argv: &[OsString], timeout: Duration) -> Result<Captured> {
	let child = command(argv)
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true)
		.spawn()
		.map_err(|error| spawn_error(argv, &error))?;
	match tokio::time::timeout(timeout, child.wait_with_output()).await {
		Ok(output) => {
			let output = output?;
			Ok(Captured {
				status: output.status,
				stdout: output.stdout,
			})
		}
		// Dropping the future kills the child (kill_on_drop).
		Err(_) => Err(Error::timeout(argv, timeout.as_secs())),
	}
}

/// Runs `argv` with inherited output streams; an unsuccessful exit is an
/// error, like Python's `subprocess.run(..., check=True)`.
pub(crate) async fn check(argv: &[OsString]) -> Result<()> {
	let status = command(argv)
		.status()
		.await
		.map_err(|error| spawn_error(argv, &error))?;
	if status.success() {
		Ok(())
	} else {
		Err(Error::called_process(argv, status))
	}
}

/// An argument array from anything path- or string-like.
#[macro_export]
macro_rules! argv {
	($($arg:expr),* $(,)?) => {
		[$(::std::ffi::OsString::from($arg)),*]
	};
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn capture_returns_status_and_stdout_only() {
		let argv = argv!["/bin/sh", "-c", "echo out; echo err >&2; exit 3"];
		let captured = capture(&argv, CLI_TIMEOUT).await.unwrap();
		assert_eq!(captured.status.code(), Some(3));
		assert_eq!(captured.stdout, b"out\n");
	}

	#[tokio::test]
	async fn capture_kills_after_timeout() {
		let argv = argv!["/bin/sh", "-c", "sleep 30"];
		let error = capture(&argv, Duration::from_millis(100))
			.await
			.unwrap_err();
		assert_eq!(error.name(), "TimeoutExpired");
	}

	#[tokio::test]
	async fn missing_program_is_file_not_found() {
		let error = check(&argv!["/nonexistent/program"]).await.unwrap_err();
		assert_eq!(
			error.to_string(),
			"FileNotFoundError: [Errno 2] No such file or directory: '/nonexistent/program'"
		);
	}

	#[tokio::test]
	async fn check_reports_exit_status() {
		let error = check(&argv!["/bin/sh", "-c", "exit 4"]).await.unwrap_err();
		assert_eq!(error.name(), "CalledProcessError");
		assert!(
			error
				.message()
				.ends_with("returned non-zero exit status 4.")
		);
	}
}
