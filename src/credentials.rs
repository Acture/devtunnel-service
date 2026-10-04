//! Credential sources. Hosting, checks and reconnection live in one place
//! and take their credentials from a [`Credentials`] implementation.
//!
//! The `cli` source delegates everything to the devtunnel CLI's existing
//! login: it never logs in, selects an identity or stores a token.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;
use tunnels::management::Authorization;

use crate::error::{Error, Result, exit_code};

/// How long a minted token serves reads (tunnel checks, standby polls,
/// unregistering) before reads mint a new one. CLI host tokens live 24 h.
pub(crate) const READ_TOKEN_REUSE: Duration = Duration::from_secs(60 * 60);

pub(crate) const UNRECOGNIZED_TOKEN_OUTPUT: &str =
	"Unrecognized devtunnel token output; refusing to host";

/// A bearer token held only in process memory: never logged, written to
/// disk or passed in argv. `Debug` is redacted and there is no `Display`.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
	/// Accepts a token with surrounding whitespace (such as the CLI's trailing
	/// newline) removed; rejects an empty value or one containing anything
	/// but visible ASCII, which could not travel in an HTTP header.
	pub(crate) fn new(value: &str) -> Result<Self> {
		let value = value.trim();
		if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
			return Err(Error::value(UNRECOGNIZED_TOKEN_OUTPUT));
		}
		Ok(Self(value.to_owned()))
	}

	pub(crate) fn expose(&self) -> &str {
		&self.0
	}
}

impl fmt::Debug for Secret {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Secret(..)")
	}
}

/// A credential source.
pub(crate) trait Credentials {
	/// Mints a host token for the next relay connection. Called before every
	/// connection attempt.
	async fn host_token(&self) -> Result<Secret>;

	/// Authorization for reading the tunnel with its ports and access
	/// control, and for the relay's own management calls.
	async fn read_auth(&self) -> Result<Authorization>;

	/// Extends the tunnel lease to 30 days.
	async fn renew(&self) -> Result<()>;
}

/// The `cli` source: the devtunnel CLI's existing login.
///
/// - `host_token`: `BINARY token TUNNEL_ID --scopes host --json`; the token
///   is the string field `token` of the JSON object on stdout, passed
///   through [`Secret::new`]. Anything else is a `ValueError`
///   ([`UNRECOGNIZED_TOKEN_OUTPUT`]). The token is cached for `read_auth`.
/// - `read_auth`: `Authorization::Tunnel` with the cached token while it is
///   younger than [`READ_TOKEN_REUSE`], otherwise a fresh `host_token`.
/// - `renew`: `BINARY update TUNNEL_ID --expiration 30d`, judged by exit
///   status only; its output is not parsed.
///
/// Every command runs through [`crate::process::capture`] with
/// [`crate::process::CLI_TIMEOUT`]. A nonzero exit is a `RuntimeError` that
/// never includes CLI output, exactly as in Python:
/// `devtunnel {subcommand} failed with exit {code}; check the CLI login and
/// tunnel permissions under the service's Unix user`.
pub(crate) struct Cli {
	binary: PathBuf,
	tunnel_id: String,
	timeout: Duration,
	/// How long a minted token serves `read_auth` ([`READ_TOKEN_REUSE`]).
	reuse: Duration,
	cache: Mutex<Option<(Secret, Instant)>>,
}

impl Cli {
	pub(crate) fn new(binary: impl Into<PathBuf>, tunnel_id: impl Into<String>) -> Self {
		Self {
			binary: binary.into(),
			tunnel_id: tunnel_id.into(),
			timeout: crate::process::CLI_TIMEOUT,
			reuse: READ_TOKEN_REUSE,
			cache: Mutex::new(None),
		}
	}

	/// Runs `BINARY SUBCOMMAND TUNNEL_ID ARGS...` and returns its stdout, or a
	/// `RuntimeError` naming only the subcommand and exit code.
	async fn run(&self, subcommand: &str, args: &[&str]) -> Result<Vec<u8>> {
		let mut argv: Vec<OsString> = vec![
			self.binary.clone().into(),
			subcommand.into(),
			self.tunnel_id.clone().into(),
		];
		argv.extend(args.iter().map(OsString::from));
		let captured = crate::process::capture(&argv, self.timeout).await?;
		if !captured.status.success() {
			// Never forward arbitrary CLI output into service logs.
			return Err(Error::runtime(format!(
				"devtunnel {subcommand} failed with exit {}; check the CLI login and tunnel \
				 permissions under the service's Unix user",
				exit_code(captured.status)
			)));
		}
		Ok(captured.stdout)
	}
}

/// The string field `token` of the JSON object in `stdout`.
/// The token in `devtunnel token --json` output: the JSON object that starts
/// a line, after any notice the CLI prints first (it shows a welcome banner
/// once, on some runs).
fn parse_token(stdout: &[u8]) -> Result<Secret> {
	let start = stdout
		.split_inclusive(|byte| *byte == b'\n')
		.scan(0, |offset, line| {
			let at = *offset;
			*offset += line.len();
			Some((at, line))
		})
		.find(|(_, line)| line.first() == Some(&b'{'))
		.map_or(0, |(at, _)| at);
	let value: Value = serde_json::from_slice(&stdout[start..])
		.map_err(|_| Error::value(UNRECOGNIZED_TOKEN_OUTPUT))?;
	let token = value
		.as_object()
		.and_then(|object| object.get("token"))
		.and_then(Value::as_str)
		.ok_or_else(|| Error::value(UNRECOGNIZED_TOKEN_OUTPUT))?;
	Secret::new(token)
}

impl Credentials for Cli {
	async fn host_token(&self) -> Result<Secret> {
		let stdout = self.run("token", &["--scopes", "host", "--json"]).await?;
		let token = parse_token(&stdout)?;
		*self.cache.lock().expect("token cache lock") = Some((token.clone(), Instant::now()));
		Ok(token)
	}

	async fn read_auth(&self) -> Result<Authorization> {
		let cached: Option<Secret> = self
			.cache
			.lock()
			.expect("token cache lock")
			.as_ref()
			.filter(|(_, minted)| minted.elapsed() < self.reuse)
			.map(|(token, _)| token.clone());
		let token = match cached {
			Some(token) => token,
			None => self.host_token().await?,
		};
		Ok(Authorization::Tunnel(token.expose().to_owned()))
	}

	async fn renew(&self) -> Result<()> {
		self.run("update", &["--expiration", "30d"]).await?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn secret_strips_newline_and_rejects_header_breaking_values() {
		assert_eq!(
			Secret::new("eyJ.a-b_c.d\n").unwrap().expose(),
			"eyJ.a-b_c.d"
		);
		for value in ["", " \n", "a b", "a\r\nX-Header: 1", "t\u{e9}"] {
			assert_eq!(
				Secret::new(value).unwrap_err().to_string(),
				format!("ValueError: {UNRECOGNIZED_TOKEN_OUTPUT}")
			);
		}
	}

	#[test]
	fn secret_debug_is_redacted() {
		assert_eq!(format!("{:?}", Secret::new("token").unwrap()), "Secret(..)");
	}

	const TUNNEL_ID: &str = "example-api";
	const TOKEN_OUTPUT: &str = r#"printf '{"tunnelId": "example-api", "token": "eyJ.a-b_c.d"}\n'"#;

	/// A temporary directory holding a fake devtunnel CLI that appends its
	/// arguments to `argv.log` next to itself, then runs `body`. Removed on drop.
	struct FakeCli(PathBuf);

	impl FakeCli {
		fn new(name: &str, body: &str) -> Self {
			use std::os::unix::fs::PermissionsExt;
			let dir = std::env::temp_dir().join(format!(
				"devtunnel-service-credentials-{}-{name}",
				std::process::id()
			));
			std::fs::create_dir(&dir).unwrap();
			let script = dir.join("devtunnel");
			std::fs::write(
				&script,
				format!(
					"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/argv.log\"\n{body}\n"
				),
			)
			.unwrap();
			std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
			Self(dir)
		}

		fn cli(&self) -> Cli {
			self.cli_with(crate::process::CLI_TIMEOUT, READ_TOKEN_REUSE)
		}

		fn cli_with(&self, timeout: Duration, reuse: Duration) -> Cli {
			Cli {
				binary: self.0.join("devtunnel"),
				tunnel_id: TUNNEL_ID.to_owned(),
				timeout,
				reuse,
				cache: Mutex::new(None),
			}
		}

		/// One line per invocation: the arguments after the binary.
		fn calls(&self) -> Vec<String> {
			std::fs::read_to_string(self.0.join("argv.log"))
				.unwrap_or_default()
				.lines()
				.map(str::to_owned)
				.collect()
		}
	}

	impl Drop for FakeCli {
		fn drop(&mut self) {
			// Ignored: a failed removal must not abort a panicking test.
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn tunnel_token(auth: Authorization) -> String {
		let Authorization::Tunnel(token) = auth else {
			panic!("expected tunnel authorization");
		};
		token
	}

	#[tokio::test]
	async fn token_parsed_from_json_with_trailing_newline() {
		let fake = FakeCli::new("token", TOKEN_OUTPUT);
		let token = fake.cli().host_token().await.unwrap();
		assert_eq!(token.expose(), "eyJ.a-b_c.d");
		assert_eq!(fake.calls(), ["token example-api --scopes host --json"]);
	}

	#[tokio::test]
	async fn unrecognized_token_output_is_value_error() {
		let fake = FakeCli::new("unrecognized", r#"cat "$(dirname "$0")/stdout""#);
		let cli = fake.cli();
		let outputs: [&[u8]; 12] = [
			b"not json\n",
			b"{}\n",
			b"{\"token\": 7}\n",
			b"{\"token\": null}\n",
			b"{\"token\": \"\"}\n",
			b"{\"token\": \"a b\"}\n",
			b"[\"eyJ.a-b_c.d\"]\n",
			b"\"eyJ.a-b_c.d\"\n",
			b"{\"token\": \"\xff\"}\n",
			b"",
			b"Welcome to dev tunnels!\n",
			b"{\"token\": \"eyJ.a\"}\ntrailing text\n",
		];
		for output in outputs {
			std::fs::write(fake.0.join("stdout"), output).unwrap();
			let error = cli.host_token().await.unwrap_err();
			assert_eq!(
				error.to_string(),
				format!("ValueError: {UNRECOGNIZED_TOKEN_OUTPUT}"),
				"{}",
				String::from_utf8_lossy(output)
			);
		}
	}

	#[tokio::test]
	async fn failure_never_exposes_cli_output() {
		let fake = FakeCli::new("failure", "echo secret; echo secret >&2; exit 3");
		let cli = fake.cli();
		let advice = "check the CLI login and tunnel permissions under the service's Unix user";
		assert_eq!(
			cli.host_token().await.unwrap_err().to_string(),
			format!("RuntimeError: devtunnel token failed with exit 3; {advice}")
		);
		assert_eq!(
			cli.renew().await.unwrap_err().to_string(),
			format!("RuntimeError: devtunnel update failed with exit 3; {advice}")
		);
	}

	#[tokio::test]
	async fn signal_exit_is_negative() {
		let fake = FakeCli::new("signal", "kill -9 $$");
		let error = fake.cli().renew().await.unwrap_err();
		assert!(
			error
				.to_string()
				.starts_with("RuntimeError: devtunnel update failed with exit -9;")
		);
	}

	#[tokio::test]
	async fn renew_checks_exit_status_only() {
		let fake = FakeCli::new("renew", "echo not json; echo noise >&2");
		fake.cli().renew().await.unwrap();
		assert_eq!(fake.calls(), ["update example-api --expiration 30d"]);
	}

	#[tokio::test]
	async fn slow_cli_times_out() {
		let fake = FakeCli::new("timeout", "exec sleep 5");
		let cli = fake.cli_with(Duration::from_millis(100), READ_TOKEN_REUSE);
		assert_eq!(cli.host_token().await.unwrap_err().name(), "TimeoutExpired");
	}

	#[tokio::test]
	async fn read_auth_reuses_cached_token() {
		let fake = FakeCli::new("reuse", TOKEN_OUTPUT);
		let cli = fake.cli();
		cli.host_token().await.unwrap();
		for _ in 0..2 {
			assert_eq!(tunnel_token(cli.read_auth().await.unwrap()), "eyJ.a-b_c.d");
		}
		assert_eq!(fake.calls().len(), 1);
	}

	#[tokio::test]
	async fn read_auth_mints_without_a_fresh_token() {
		let fake = FakeCli::new("mint", TOKEN_OUTPUT);
		let cli = fake.cli_with(crate::process::CLI_TIMEOUT, Duration::ZERO);
		assert_eq!(tunnel_token(cli.read_auth().await.unwrap()), "eyJ.a-b_c.d");
		cli.host_token().await.unwrap();
		assert_eq!(tunnel_token(cli.read_auth().await.unwrap()), "eyJ.a-b_c.d");
		assert_eq!(fake.calls().len(), 3);
	}

	#[test]
	fn token_follows_a_cli_notice() {
		let output = b"Welcome to dev tunnels!\nCLI version: 1.0.2094\n\n{\"token\": \"eyJ.a-b_c.d\", \"scope\": \"host\"}\n";
		assert_eq!(parse_token(output).unwrap().expose(), "eyJ.a-b_c.d");
	}
}
