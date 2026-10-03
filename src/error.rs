//! Operational errors, printed as one line at the console boundary.
//!
//! Each error carries the name of the exception the Python implementation
//! raised for the same failure (`ValueError`, `RuntimeError`, `OSError`
//! subclasses, `CalledProcessError`, ...), so both versions report the same
//! one-line prefix and exit status.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::ExitStatus;

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub(crate) struct Error {
	name: &'static str,
	message: String,
	transient: bool,
}

impl Error {
	fn new(name: &'static str, message: impl Into<String>) -> Self {
		Self {
			name,
			message: message.into(),
			transient: false,
		}
	}

	/// Invalid configuration, arguments or remote state (Python's `ValueError`).
	pub(crate) fn value(message: impl Into<String>) -> Self {
		Self::new("ValueError", message)
	}

	/// A failed external operation, such as a devtunnel CLI command.
	pub(crate) fn runtime(message: impl Into<String>) -> Self {
		Self::new("RuntimeError", message)
	}

	/// A network or service failure that the hosting loop retries with backoff.
	pub(crate) fn transient(message: impl Into<String>) -> Self {
		Self {
			transient: true,
			..Self::runtime(message)
		}
	}

	/// An `OSError` subclass, formatted like Python: `[Errno 2] No such file or
	/// directory: '/missing'`.
	pub(crate) fn io(error: &io::Error, path: Option<&Path>) -> Self {
		let name = match error.kind() {
			io::ErrorKind::NotFound => "FileNotFoundError",
			io::ErrorKind::PermissionDenied => "PermissionError",
			io::ErrorKind::AlreadyExists => "FileExistsError",
			io::ErrorKind::IsADirectory => "IsADirectoryError",
			io::ErrorKind::NotADirectory => "NotADirectoryError",
			io::ErrorKind::TimedOut => "TimeoutError",
			io::ErrorKind::Interrupted => "InterruptedError",
			io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
			io::ErrorKind::ConnectionReset => "ConnectionResetError",
			io::ErrorKind::ConnectionAborted => "ConnectionAbortedError",
			io::ErrorKind::BrokenPipe => "BrokenPipeError",
			io::ErrorKind::WouldBlock => "BlockingIOError",
			_ => "OSError",
		};
		let mut message = match error.raw_os_error() {
			Some(code) => {
				let text = io::Error::from_raw_os_error(code).to_string();
				let suffix = format!(" (os error {code})");
				format!(
					"[Errno {code}] {}",
					text.strip_suffix(&suffix).unwrap_or(&text)
				)
			}
			None => error.to_string(),
		};
		if let Some(path) = path {
			message.push_str(": ");
			message.push_str(&py_repr(&path.to_string_lossy()));
		}
		Self::new(name, message)
	}

	/// Malformed JSON (Python's `json.JSONDecodeError`).
	pub(crate) fn json(error: &serde_json::Error) -> Self {
		Self::new("JSONDecodeError", error.to_string())
	}

	/// A checked child process exited unsuccessfully (Python's
	/// `subprocess.CalledProcessError`).
	pub(crate) fn called_process(argv: &[OsString], status: ExitStatus) -> Self {
		let outcome = match exit_code(status) {
			code if code < 0 => format!("died with signal {}", -code),
			code => format!("returned non-zero exit status {code}"),
		};
		Self::new(
			"CalledProcessError",
			format!("Command '{}' {outcome}.", py_repr_list(argv)),
		)
	}

	/// A child process outlived its time limit (Python's
	/// `subprocess.TimeoutExpired`).
	pub(crate) fn timeout(argv: &[OsString], seconds: u64) -> Self {
		Self::new(
			"TimeoutExpired",
			format!(
				"Command '{}' timed out after {seconds} seconds",
				py_repr_list(argv)
			),
		)
	}

	/// Whether the hosting loop may retry the failed operation.
	pub(crate) fn is_transient(&self) -> bool {
		self.transient
	}

	pub(crate) fn name(&self) -> &'static str {
		self.name
	}

	pub(crate) fn message(&self) -> &str {
		&self.message
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}: {}", self.name, self.message)
	}
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
	fn from(error: io::Error) -> Self {
		Self::io(&error, None)
	}
}

/// Python's exit-status convention: the negated signal number when a signal
/// ended the process.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
	use std::os::unix::process::ExitStatusExt;
	status
		.code()
		.or_else(|| status.signal().map(|signal| -signal))
		.unwrap_or(-1)
}

/// `repr()` of a Python string, for messages that quote paths and commands.
pub(crate) fn py_repr(text: &str) -> String {
	let quote = if text.contains('\'') && !text.contains('"') {
		'"'
	} else {
		'\''
	};
	let mut out = String::with_capacity(text.len() + 2);
	out.push(quote);
	for char in text.chars() {
		match char {
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c if c == quote => {
				out.push('\\');
				out.push(c);
			}
			c if (c as u32) < 0x20 || c as u32 == 0x7f => {
				out.push_str(&format!("\\x{:02x}", c as u32));
			}
			c => out.push(c),
		}
	}
	out.push(quote);
	out
}

/// `repr()` of a Python list of strings, as in `subprocess` error messages.
pub(crate) fn py_repr_list(argv: &[OsString]) -> String {
	let items: Vec<String> = argv
		.iter()
		.map(|arg| py_repr(&arg.to_string_lossy()))
		.collect();
	format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn missing_file_reads_like_python() {
		let error = io::Error::from_raw_os_error(2);
		assert_eq!(
			Error::io(&error, Some(Path::new("/missing"))).to_string(),
			"FileNotFoundError: [Errno 2] No such file or directory: '/missing'"
		);
	}

	#[test]
	fn repr_quotes_like_python() {
		assert_eq!(py_repr("a'b"), "\"a'b\"");
		assert_eq!(py_repr("a'b\""), "'a\\'b\"'");
		assert_eq!(py_repr("a\nb\\"), "'a\\nb\\\\'");
		let argv = [OsString::from("systemctl"), OsString::from("--user")];
		assert_eq!(py_repr_list(&argv), "['systemctl', '--user']");
	}

	#[test]
	fn called_process_reads_like_python() {
		use std::os::unix::process::ExitStatusExt;
		let argv = [OsString::from("systemctl"), OsString::from("daemon-reload")];
		assert_eq!(
			Error::called_process(&argv, ExitStatus::from_raw(1 << 8)).to_string(),
			"CalledProcessError: Command '['systemctl', 'daemon-reload']' returned non-zero exit status 1."
		);
	}
}
