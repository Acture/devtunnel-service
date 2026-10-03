//! Instance configuration, written by `deploy` and read by `host`, `renew`
//! and `doctor`.

use std::path::Path;

use serde_json::Value;

use crate::error::{Error, Result};

pub(crate) const NOT_AN_OBJECT: &str = "Configuration must be a JSON object";
pub(crate) const INVALID_TUNNEL_ID: &str = "A valid explicit tunnel ID is required";
pub(crate) const INVALID_BINARY: &str = "devtunnel binary path must be absolute";
pub(crate) const INVALID_PORTS: &str =
	"Expected ports must be a nonempty list of unique port numbers";
pub(crate) const INVALID_ALLOW_ANONYMOUS: &str = "allow_anonymous must be true or false";

/// A validated instance configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Config {
	/// Matches `[A-Za-z0-9][A-Za-z0-9._-]*` in full: never a flag or a shell
	/// word. May carry its cluster as `ID.CLUSTER`.
	pub tunnel_id: String,
	/// Absolute path of the devtunnel CLI (the `cli` credential source).
	pub binary: String,
	/// Nonempty, unique, 1..=65535, in configuration order.
	pub ports: Vec<u16>,
	/// Accept anonymous allow rules on the remote tunnel (with a warning).
	pub allow_anonymous: bool,
}

impl Config {
	/// Validates a parsed document like Python's `validate_config`, in the
	/// same order and with the same messages (all `ValueError`):
	///
	/// 1. not an object: [`NOT_AN_OBJECT`];
	/// 2. `tunnel_id` not a string matching the ID pattern: [`INVALID_TUNNEL_ID`];
	/// 3. `binary` not a string holding an absolute path: [`INVALID_BINARY`];
	/// 4. `ports` not a nonempty array of unique integers in 1..=65535
	///    (`true`, `4000.0` and `"4000"` are not integers): [`INVALID_PORTS`];
	/// 5. `allow_anonymous` present but not a boolean: [`INVALID_ALLOW_ANONYMOUS`].
	///    Absent means `false`.
	///
	/// Unknown fields, including the legacy `identity`, are ignored.
	pub(crate) fn from_value(value: &Value) -> Result<Self> {
		let _ = value;
		todo!("config::Config::from_value")
	}

	/// Reads and validates `path`. Read failures are `OSError`s naming the
	/// path (see [`Error::io`]); invalid UTF-8 is a `UnicodeDecodeError`;
	/// malformed JSON is a `JSONDecodeError`.
	pub(crate) fn load(path: &Path) -> Result<Self> {
		let _ = (path, Error::value(""));
		todo!("config::Config::load")
	}

	/// The file written by `deploy`, byte-identical to Python's
	/// `json.dumps(config, indent=2) + "\n"`: keys in the order `tunnel_id`,
	/// `binary`, `ports`, `allow_anonymous`, and non-ASCII characters escaped
	/// as `\uXXXX` (surrogate pairs above U+FFFF), as `ensure_ascii` does.
	pub(crate) fn to_json(&self) -> String {
		todo!("config::Config::to_json")
	}
}
