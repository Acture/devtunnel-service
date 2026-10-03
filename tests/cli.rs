//! The command line as a black box, on paths that reach neither the devtunnel
//! CLI nor the tunnel service.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const PROGRAM: &str = env!("CARGO_BIN_EXE_devtunnel-service");

/// Runs the program without a usable `PATH` or home directory.
fn run(args: &[&str]) -> Output {
	Command::new(PROGRAM)
		.args(args)
		.env("PATH", "")
		.env("HOME", "/nonexistent")
		.env_remove("JOURNAL_STREAM")
		.stdin(Stdio::null())
		.output()
		.unwrap()
}

fn stderr(output: &Output) -> &str {
	std::str::from_utf8(&output.stderr).unwrap()
}

/// A temporary directory, removed on drop.
struct Folder(PathBuf);

impl Folder {
	fn new(name: &str) -> Self {
		let path = std::env::temp_dir().join(format!(
			"devtunnel-service-cli-{}-{name}",
			std::process::id()
		));
		std::fs::create_dir_all(&path).unwrap();
		Self(path)
	}

	/// Writes `config.json` and returns its path.
	fn config(&self, contents: &str) -> String {
		let path = self.0.join("config.json");
		std::fs::write(&path, contents).unwrap();
		path.to_str().unwrap().to_owned()
	}
}

impl Drop for Folder {
	fn drop(&mut self) {
		std::fs::remove_dir_all(&self.0).unwrap();
	}
}

/// Asserts a failure reported as the single stderr line `line`.
fn assert_fails_with(output: &Output, line: &str) {
	assert_eq!(output.status.code(), Some(1));
	assert_eq!(stderr(output), format!("{line}\n"));
	assert!(output.stdout.is_empty());
}

#[test]
fn version() {
	let output = run(&["--version"]);
	assert!(output.status.success());
	assert_eq!(
		output.stdout,
		concat!("devtunnel-service ", env!("CARGO_PKG_VERSION"), "\n").as_bytes()
	);
}

#[test]
fn help_needs_no_system_access() {
	for args in [&["--help"][..], &["deploy", "--help"], &["host", "--help"]] {
		let output = run(args);
		assert!(output.status.success(), "{args:?}");
		let stdout = String::from_utf8(output.stdout).unwrap();
		assert!(stdout.contains("devtunnel-service"), "{args:?}");
		if args[0] == "deploy" {
			assert!(stdout.contains("--allow-anonymous"));
		}
	}
}

#[test]
fn command_is_required() {
	assert_eq!(run(&[]).status.code(), Some(2));
}

#[test]
fn missing_config_reads_like_python() {
	assert_fails_with(
		&run(&["doctor", "--config", "/missing"]),
		"FileNotFoundError: [Errno 2] No such file or directory: '/missing'",
	);
}

#[test]
fn malformed_config_is_one_line() {
	let folder = Folder::new("malformed");
	let output = run(&["doctor", "--config", &folder.config("{")]);
	assert_eq!(output.status.code(), Some(1));
	let stderr = stderr(&output);
	assert!(stderr.starts_with("JSONDecodeError: "), "{stderr}");
	assert_eq!(stderr.lines().count(), 1);
}

#[test]
fn invalid_config_is_a_value_error() {
	let folder = Folder::new("invalid");
	let config = folder.config(
		r#"{"tunnel_id": "example-api.usw2", "binary": "/usr/bin/devtunnel", "ports": [4000], "allow_anonymous": "x"}"#,
	);
	assert_fails_with(
		&run(&["renew", "--config", &config]),
		"ValueError: allow_anonymous must be true or false",
	);
}

#[test]
#[ignore = "exercises credentials::Cli::host_token, which is implemented separately"]
fn missing_devtunnel_reads_like_python() {
	let folder = Folder::new("binary");
	let config = folder.config(
		r#"{"tunnel_id": "example-api.usw2", "binary": "/nonexistent/devtunnel", "ports": [4000]}"#,
	);
	assert_fails_with(
		&run(&["host", "--config", &config]),
		"FileNotFoundError: [Errno 2] No such file or directory: '/nonexistent/devtunnel'",
	);
}
