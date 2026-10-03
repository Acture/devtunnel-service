//! Credential sources. Hosting, checks and reconnection live in one place
//! and take their credentials from a [`Credentials`] implementation.
//!
//! The `cli` source delegates everything to the devtunnel CLI's existing
//! login: it never logs in, selects an identity or stores a token.

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tunnels::management::Authorization;

use crate::error::{Error, Result};

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
	cache: Mutex<Option<(Secret, Instant)>>,
}

impl Cli {
	pub(crate) fn new(binary: impl Into<PathBuf>, tunnel_id: impl Into<String>) -> Self {
		Self {
			binary: binary.into(),
			tunnel_id: tunnel_id.into(),
			timeout: crate::process::CLI_TIMEOUT,
			cache: Mutex::new(None),
		}
	}
}

impl Credentials for Cli {
	async fn host_token(&self) -> Result<Secret> {
		let _ = (&self.binary, &self.tunnel_id, self.timeout, &self.cache);
		todo!("credentials::Cli::host_token")
	}

	async fn read_auth(&self) -> Result<Authorization> {
		todo!("credentials::Cli::read_auth")
	}

	async fn renew(&self) -> Result<()> {
		todo!("credentials::Cli::renew")
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
}
