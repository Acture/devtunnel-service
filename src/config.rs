//! Instance configuration, written by `deploy` and read by `host`, `renew`
//! and `doctor`.

use std::fmt::Write;
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

fn is_tunnel_id(value: &str) -> bool {
	let mut chars = value.chars();
	chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
		&& chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn ports(value: Option<&Value>) -> Option<Vec<u16>> {
	let items = value?.as_array()?;
	let ports: Vec<u16> = items
		.iter()
		.map(|item| item.as_u64()?.try_into().ok().filter(|port| *port != 0))
		.collect::<Option<_>>()?;
	let mut unique = ports.clone();
	unique.sort_unstable();
	unique.dedup();
	(!ports.is_empty() && unique.len() == ports.len()).then_some(ports)
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
		let object = value
			.as_object()
			.ok_or_else(|| Error::value(NOT_AN_OBJECT))?;
		let tunnel_id = object
			.get("tunnel_id")
			.and_then(Value::as_str)
			.filter(|id| is_tunnel_id(id))
			.ok_or_else(|| Error::value(INVALID_TUNNEL_ID))?;
		let binary = object
			.get("binary")
			.and_then(Value::as_str)
			.filter(|binary| Path::new(binary).is_absolute())
			.ok_or_else(|| Error::value(INVALID_BINARY))?;
		let ports = ports(object.get("ports")).ok_or_else(|| Error::value(INVALID_PORTS))?;
		let allow_anonymous = match object.get("allow_anonymous") {
			None => false,
			Some(value) => value
				.as_bool()
				.ok_or_else(|| Error::value(INVALID_ALLOW_ANONYMOUS))?,
		};
		Ok(Self {
			tunnel_id: tunnel_id.to_owned(),
			binary: binary.to_owned(),
			ports,
			allow_anonymous,
		})
	}

	/// Reads and validates `path`. Read failures are `OSError`s naming the
	/// path (see [`Error::io`]); invalid UTF-8 is a `UnicodeDecodeError`;
	/// malformed JSON is a `JSONDecodeError`.
	pub(crate) fn load(path: &Path) -> Result<Self> {
		let bytes = std::fs::read(path).map_err(|error| Error::io(&error, Some(path)))?;
		let text = std::str::from_utf8(&bytes).map_err(|error| unicode_error(&bytes, error))?;
		let value: Value = serde_json::from_str(text).map_err(|error| Error::json(&error))?;
		Self::from_value(&value)
	}

	/// The file written by `deploy`, byte-identical to Python's
	/// `json.dumps(config, indent=2) + "\n"`: keys in the order `tunnel_id`,
	/// `binary`, `ports`, `allow_anonymous`, and every character outside
	/// printable ASCII escaped as `\uXXXX` (surrogate pairs above U+FFFF), as
	/// `ensure_ascii` does.
	pub(crate) fn to_json(&self) -> String {
		let ports: Vec<String> = self
			.ports
			.iter()
			.map(|port| format!("    {port}"))
			.collect();
		format!(
			"{{\n  \"tunnel_id\": {},\n  \"binary\": {},\n  \"ports\": [\n{}\n  ],\n  \"allow_anonymous\": {}\n}}\n",
			json_string(&self.tunnel_id),
			json_string(&self.binary),
			ports.join(",\n"),
			self.allow_anonymous,
		)
	}
}

/// A JSON string literal escaped like Python's `json.dumps` with
/// `ensure_ascii=True`.
pub(crate) fn json_string(text: &str) -> String {
	let mut out = String::with_capacity(text.len() + 2);
	out.push('"');
	for char in text.chars() {
		match char {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'\u{8}' => out.push_str("\\b"),
			'\u{c}' => out.push_str("\\f"),
			' '..='~' => out.push(char),
			_ => {
				let mut units = [0; 2];
				for unit in char.encode_utf16(&mut units) {
					write!(out, "\\u{unit:04x}").expect("writing to a String");
				}
			}
		}
	}
	out.push('"');
	out
}

/// Python's `UnicodeDecodeError` for UTF-8 text.
fn unicode_error(bytes: &[u8], error: std::str::Utf8Error) -> Error {
	let position = error.valid_up_to();
	let byte = bytes[position];
	let reason = match error.error_len() {
		None => "unexpected end of data",
		Some(_) if matches!(byte, 0x80..=0xc1 | 0xf5..=0xff) => "invalid start byte",
		Some(_) => "invalid continuation byte",
	};
	Error::unicode(format!(
		"'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: {reason}"
	))
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	fn config() -> Value {
		json!({
			"tunnel_id": "example-api.region",
			"binary": "/usr/local/bin/devtunnel",
			"ports": [4000],
			"allow_anonymous": false,
		})
	}

	fn with(key: &str, value: Value) -> Value {
		let mut config = config();
		config[key] = value;
		config
	}

	fn message(value: &Value) -> String {
		Config::from_value(value).unwrap_err().to_string()
	}

	#[test]
	fn valid() {
		assert_eq!(
			Config::from_value(&config()).unwrap(),
			Config {
				tunnel_id: "example-api.region".into(),
				binary: "/usr/local/bin/devtunnel".into(),
				ports: vec![4000],
				allow_anonymous: false,
			}
		);
	}

	#[test]
	fn not_an_object() {
		for value in [json!([]), json!("x"), json!(null)] {
			assert_eq!(message(&value), format!("ValueError: {NOT_AN_OBJECT}"));
		}
	}

	#[test]
	fn tunnel_id_cannot_be_flag_or_shell_command() {
		for value in [
			json!(""),
			json!("--allow-anonymous"),
			json!("a;touch /tmp/a"),
			json!("a\n"),
			json!(null),
			json!(7),
		] {
			assert_eq!(
				message(&with("tunnel_id", value)),
				format!("ValueError: {INVALID_TUNNEL_ID}")
			);
		}
	}

	#[test]
	fn binary_must_be_absolute() {
		for value in [json!("devtunnel"), json!(null), json!(7)] {
			assert_eq!(
				message(&with("binary", value)),
				format!("ValueError: {INVALID_BINARY}")
			);
		}
	}

	#[test]
	fn ports() {
		for value in [
			json!([]),
			json!([0]),
			json!([65536]),
			json!([-1]),
			json!([true]),
			json!(["4000"]),
			json!([4000.0]),
			json!([4000, 4000]),
			json!(null),
		] {
			assert_eq!(
				message(&with("ports", value)),
				format!("ValueError: {INVALID_PORTS}")
			);
		}
		let config = Config::from_value(&with("ports", json!([5000, 22, 65535]))).unwrap();
		assert_eq!(config.ports, [5000, 22, 65535]);
	}

	#[test]
	fn validation_order_matches_python() {
		let value = json!({"tunnel_id": "x", "binary": "relative", "ports": []});
		assert_eq!(message(&value), format!("ValueError: {INVALID_BINARY}"));
	}

	#[test]
	fn legacy_identity_field_is_unused() {
		let value = with("identity", json!({"selector": "unused", "id": "unused"}));
		assert_eq!(
			Config::from_value(&value).unwrap(),
			Config::from_value(&config()).unwrap()
		);
	}

	#[test]
	fn allow_anonymous_is_an_optional_boolean() {
		assert!(
			Config::from_value(&with("allow_anonymous", json!(true)))
				.unwrap()
				.allow_anonymous
		);
		let mut absent = config();
		absent.as_object_mut().unwrap().remove("allow_anonymous");
		assert!(!Config::from_value(&absent).unwrap().allow_anonymous);
		for value in [json!(null), json!("false"), json!(0)] {
			assert_eq!(
				message(&with("allow_anonymous", value)),
				format!("ValueError: {INVALID_ALLOW_ANONYMOUS}")
			);
		}
	}

	fn sample(binary: &str) -> Config {
		Config {
			tunnel_id: "example-api.usw2".into(),
			binary: binary.into(),
			ports: vec![22, 4000],
			allow_anonymous: false,
		}
	}

	#[test]
	fn json_matches_python_dumps() {
		// json.dumps(config, indent=2) + "\n" from Python 3.14.
		let tail =
			"\",\n  \"ports\": [\n    22,\n    4000\n  ],\n  \"allow_anonymous\": false\n}\n";
		let head = "{\n  \"tunnel_id\": \"example-api.usw2\",\n  \"binary\": \"";
		for (binary, escaped) in [
			("/usr/bin/devtunnel", "/usr/bin/devtunnel"),
			("/opt/ü/devtunnel", "/opt/\\u00fc/devtunnel"),
			("/opt/😀/d\"q\\x", "/opt/\\ud83d\\ude00/d\\\"q\\\\x"),
			("/a\u{7f}b", "/a\\u007fb"),
			("/t\u{2028}", "/t\\u2028"),
		] {
			assert_eq!(sample(binary).to_json(), format!("{head}{escaped}{tail}"));
		}
	}

	#[test]
	fn json_round_trips() {
		let config = Config {
			allow_anonymous: true,
			..sample("/opt/ü/devtunnel")
		};
		let value: Value = serde_json::from_str(&config.to_json()).unwrap();
		assert_eq!(Config::from_value(&value).unwrap(), config);
	}

	#[test]
	fn load_errors_read_like_python() {
		let folder = std::env::temp_dir().join(format!("devtunnel-config-{}", std::process::id()));
		std::fs::create_dir_all(&folder).unwrap();
		let missing = folder.join("missing.json");
		assert_eq!(
			Config::load(&missing).unwrap_err().to_string(),
			format!(
				"FileNotFoundError: [Errno 2] No such file or directory: '{}'",
				missing.display()
			)
		);
		assert_eq!(
			Config::load(&folder).unwrap_err().name(),
			"IsADirectoryError"
		);
		let path = folder.join("config.json");
		std::fs::write(&path, b"\xff").unwrap();
		assert_eq!(
			Config::load(&path).unwrap_err().to_string(),
			"UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
		);
		std::fs::write(&path, "").unwrap();
		assert_eq!(Config::load(&path).unwrap_err().name(), "JSONDecodeError");
		std::fs::write(&path, sample("/usr/bin/devtunnel").to_json()).unwrap();
		assert_eq!(Config::load(&path).unwrap(), sample("/usr/bin/devtunnel"));
		std::fs::remove_dir_all(&folder).unwrap();
	}
}
