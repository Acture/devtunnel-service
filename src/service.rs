//! Dev Tunnels SDK adapter: tunnel reads and the relay host.
//!
//! The SDK's error type is private, so SDK failures are classified here and
//! reported through their `Display` text only. `Debug` output and request
//! options must never be logged: they can carry the token.

use std::error::Error as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::Value;
use tunnels::connections::{RelayHandle, RelayTunnelHost};
use tunnels::contracts::{PORT_TOKEN, Tunnel, TunnelPort};
use tunnels::management::{
	Authorization, AuthorizationProvider, BoxFuture, HttpError, TunnelLocator,
	TunnelManagementClient, TunnelRequestOptions,
};

use crate::credentials::Secret;
use crate::error::{Error, Result};

pub(crate) const UNKNOWN_CLUSTER: &str =
	"Cannot determine the tunnel's cluster; configure the full tunnel ID (ID.CLUSTER)";
pub(crate) const UNRECOGNIZED_SCHEMA: &str = "Unrecognized tunnel schema; refusing to host";

/// Limit for one management request. The SDK's HTTP client has none.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Limit for registering the endpoint and opening the relay connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `request` within `limit`; running out of time is transient.
async fn bounded<T>(
	limit: Duration,
	what: &str,
	request: impl Future<Output = Result<T>>,
) -> Result<T> {
	tokio::time::timeout(limit, request)
		.await
		.unwrap_or_else(|_| {
			Err(Error::transient(format!(
				"{what} did not finish within {}s",
				limit.as_secs()
			)))
		})
}

/// Reads of the tunnel.
pub(crate) trait Api {
	/// The tunnel with its ports, tunnel- and port-level access control,
	/// endpoints and status (`include_ports`, `include_access_control`).
	async fn tunnel(&self, auth: &Authorization) -> Result<Tunnel>;
}

/// A relay host. One instance serves the whole process, so its host ID and
/// endpoint stay the same across reconnections.
pub(crate) trait Relay {
	/// Resolves when the relay connection ends, for whatever reason.
	type Connection: Future<Output = ()> + Unpin;

	/// Forwards `port` to localhost on every connection. The SDK registers
	/// the port with the service and creates it if it is missing, so callers
	/// pass only ports that already exist remotely.
	async fn add_port(&mut self, port: u16, auth: &Authorization) -> Result<()>;

	/// Registers this host's endpoint and connects to the relay. Failures are
	/// transient.
	async fn connect(&mut self, token: &Secret) -> Result<Self::Connection>;

	/// Disconnects from the relay.
	async fn close(&mut self, connection: Self::Connection);

	/// Removes this host's endpoint from the tunnel.
	async fn unregister(&mut self, auth: &Authorization) -> Result<()>;

	/// This host's ID, known once it has connected.
	fn host_id(&self) -> Option<&str>;
}

/// The SDK's management client authorization: the most recent token the
/// hosting loop handed to [`Relay::add_port`] or [`Relay::unregister`].
#[derive(Clone)]
struct AuthCell(Arc<Mutex<Authorization>>);

impl AuthCell {
	fn new(auth: Authorization) -> Self {
		Self(Arc::new(Mutex::new(auth)))
	}

	fn set(&self, auth: &Authorization) {
		*self.0.lock().expect("authorization lock") = auth.clone();
	}
}

impl AuthorizationProvider for AuthCell {
	fn get_authorization(&self) -> BoxFuture<'_, Result<Authorization, HttpError>> {
		let auth = self.0.lock().expect("authorization lock").clone();
		Box::pin(async move { Ok(auth) })
	}
}

/// The tunnel service for one tunnel.
pub(crate) struct Service {
	mgmt: TunnelManagementClient,
	locator: TunnelLocator,
	auth: AuthCell,
}

impl Service {
	/// A client for `tunnel_id` (see [`locator`]) whose management calls
	/// default to `token`. The user agent is `devtunnel-service/VERSION`.
	pub(crate) fn new(tunnel_id: &str, token: &Secret) -> Result<Self> {
		let locator = locator(tunnel_id, token)?;
		let auth = AuthCell::new(Authorization::Tunnel(token.expose().to_owned()));
		let mut builder = tunnels::management::new_tunnel_management(&format!(
			"{}/{}",
			crate::PROGRAM,
			crate::VERSION
		));
		builder.authorization_provider(auth.clone());
		Ok(Self {
			mgmt: builder.into(),
			locator,
			auth,
		})
	}

	/// The relay host for this process.
	pub(crate) fn relay(&self) -> SdkRelay {
		SdkRelay {
			host: RelayTunnelHost::new(self.locator.clone(), self.mgmt.clone()),
			auth: self.auth.clone(),
			host_id: None,
			ports: Vec::new(),
		}
	}
}

impl Api for Service {
	/// HTTP 401/403/404 and other 4xx are `RuntimeError`s naming the status
	/// and pointing at the CLI login and tunnel permissions; connection
	/// errors, 408, 429 and 5xx are transient; a response that does not
	/// deserialize (such as an unknown ACE type) is a `ValueError`,
	/// [`UNRECOGNIZED_SCHEMA`].
	async fn tunnel(&self, auth: &Authorization) -> Result<Tunnel> {
		let options = TunnelRequestOptions {
			authorization: Some(auth.clone()),
			include_ports: true,
			include_access_control: true,
			..Default::default()
		};
		bounded(REQUEST_TIMEOUT, "Reading the tunnel", async {
			self.mgmt
				.get_tunnel(&self.locator, &options)
				.await
				.map_err(|error| read_error(&error))
		})
		.await
	}
}

/// Classifies a failed tunnel read.
fn read_error(error: &HttpError) -> Error {
	match error {
		HttpError::ResponseError(response) => status_error(response.status_code.as_u16()),
		// reqwest reports a body that fails to deserialize, and a body whose
		// transfer fails, both as decode errors; only the former is a schema
		// problem.
		HttpError::ConnectionError(error)
			if error.is_decode()
				&& error
					.source()
					.is_some_and(|source| source.is::<serde_json::Error>()) =>
		{
			Error::value(UNRECOGNIZED_SCHEMA)
		}
		HttpError::ConnectionError(error) => Error::transient(format!(
			"Cannot reach the tunnel service: {}",
			sdk_text(error)
		)),
		HttpError::AuthorizationError(message) => Error::runtime(format!(
			"Cannot authorize the tunnel service request: {message}"
		)),
	}
}

/// The error for an unsuccessful HTTP status of a tunnel read.
fn status_error(code: u16) -> Error {
	match code {
		401 | 403 => Error::runtime(format!(
			"Tunnel service refused the request (HTTP {code}); check the CLI login and tunnel \
			 permissions under the service's Unix user"
		)),
		404 => Error::runtime("Tunnel not found (HTTP 404); check the tunnel ID"),
		408 | 429 | 500..=599 => {
			Error::transient(format!("Tunnel service unavailable (HTTP {code})"))
		}
		_ => Error::runtime(format!("Tunnel service rejected the request (HTTP {code})")),
	}
}

/// The locator for `tunnel_id`: `ID.CLUSTER` splits at its first dot;
/// a bare ID takes its cluster from the `clusterId` claim of `token` (a JWT
/// whose payload is decoded, never verified or logged). Only a host-name
/// label (ASCII letters, digits and `-`) is a cluster: the SDK panics on one
/// that cannot be part of a host name. Otherwise: `ValueError`
/// [`UNKNOWN_CLUSTER`].
pub(crate) fn locator(tunnel_id: &str, token: &Secret) -> Result<TunnelLocator> {
	let (id, cluster) = match tunnel_id.split_once('.') {
		Some((id, cluster)) => (id, Some(cluster.to_owned())),
		None => (tunnel_id, cluster_claim(token.expose())),
	};
	match cluster {
		Some(cluster) if !id.is_empty() && is_label(&cluster) => Ok(TunnelLocator::ID {
			cluster,
			id: id.to_owned(),
		}),
		_ => Err(Error::value(UNKNOWN_CLUSTER)),
	}
}

fn is_label(text: &str) -> bool {
	!text.is_empty()
		&& text
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
		// The `??--` form is reserved for IDNA (RFC 5890); invalid punycode
		// such as `xn--a` makes the URL parser, and so the SDK, fail.
		&& text.get(2..4) != Some("--")
}

/// SDK error text fit for one log line. The SDK's messages embed request
/// URLs, including a proxy URL with its credentials, and raw response
/// bodies: credentials are removed, and the text ends at its first control
/// character, so a body cannot add lines or journal priorities.
fn sdk_text(error: &impl std::fmt::Display) -> String {
	let text = error.to_string();
	let mut rest = text.split(char::is_control).next().unwrap_or_default();
	let mut clean = String::with_capacity(rest.len());
	while let Some(scheme) = rest.find("://") {
		let (head, tail) = rest.split_at(scheme + 3);
		clean.push_str(head);
		let end = tail
			.find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
			.unwrap_or(tail.len());
		let authority = &tail[..end];
		clean.push_str(
			authority
				.rsplit_once('@')
				.map_or(authority, |(_, host)| host),
		);
		rest = &tail[end..];
	}
	clean.push_str(rest);
	clean
}

/// The `clusterId` claim of a JWT.
fn cluster_claim(token: &str) -> Option<String> {
	let payload = base64url_decode(token.split('.').nth(1)?)?;
	let claims: Value = serde_json::from_slice(&payload).ok()?;
	Some(claims.get("clusterId")?.as_str()?.to_owned())
}

/// Decodes base64url (RFC 4648 section 5), with or without padding.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
	let text = text
		.strip_suffix("==")
		.or_else(|| text.strip_suffix('='))
		.unwrap_or(text);
	if text.len() % 4 == 1 {
		return None;
	}
	let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
	let (mut buffer, mut bits) = (0u32, 0u32);
	for byte in text.bytes() {
		let sextet = match byte {
			b'A'..=b'Z' => byte - b'A',
			b'a'..=b'z' => byte - b'a' + 26,
			b'0'..=b'9' => byte - b'0' + 52,
			b'-' => 62,
			b'_' => 63,
			_ => return None,
		};
		buffer = (buffer << 6 | u32::from(sextet)) & 0xfff;
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			bytes.push((buffer >> bits) as u8);
		}
	}
	Some(bytes)
}

/// [`Relay`] over the SDK's `RelayTunnelHost`.
pub(crate) struct SdkRelay {
	host: RelayTunnelHost,
	auth: AuthCell,
	host_id: Option<String>,
	/// Ports added so far, for logging their forwarding URLs.
	ports: Vec<u16>,
}

/// A live relay connection.
pub(crate) struct Connection(RelayHandle);

impl Future for Connection {
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
		Pin::new(&mut self.0).poll(cx).map(|_| ())
	}
}

impl Relay for SdkRelay {
	type Connection = Connection;

	async fn add_port(&mut self, port: u16, auth: &Authorization) -> Result<()> {
		self.auth.set(auth);
		let tunnel_port = TunnelPort {
			port_number: port,
			..Default::default()
		};
		bounded(REQUEST_TIMEOUT, &format!("Forwarding port {port}"), async {
			self.host.add_port(&tunnel_port).await.map_err(|error| {
				Error::runtime(format!("Cannot forward port {port}: {}", sdk_text(&error)))
			})
		})
		.await?;
		self.ports.push(port);
		Ok(())
	}

	/// Logs the forwarding URL of each port from the endpoint's
	/// `port_uri_format`, when the service provides one.
	async fn connect(&mut self, token: &Secret) -> Result<Connection> {
		let handle = bounded(CONNECT_TIMEOUT, "Connecting to the relay", async {
			self.host.connect(token.expose()).await.map_err(|error| {
				Error::transient(format!("Relay connection failed: {}", sdk_text(&error)))
			})
		})
		.await?;
		let endpoint = handle.endpoint();
		self.host_id = Some(endpoint.host_id.clone());
		if let Some(format) = &endpoint.port_uri_format {
			for port in &self.ports {
				let url = format.replace(PORT_TOKEN, &port.to_string());
				crate::log::info(format!("Forwarding port {port}: {url}"));
			}
		}
		Ok(Connection(handle))
	}

	async fn close(&mut self, connection: Connection) {
		// The connection is gone either way; there is nothing to recover.
		let _ = connection.0.close().await;
	}

	async fn unregister(&mut self, auth: &Authorization) -> Result<()> {
		self.auth.set(auth);
		bounded(REQUEST_TIMEOUT, "Unregistering this host", async {
			match self.host.unregister().await {
				Ok(_) => Ok(()),
				Err(error) => Err(Error::transient(format!(
					"Cannot unregister this host: {}",
					sdk_text(&error)
				))),
			}
		})
		.await
	}

	fn host_id(&self) -> Option<&str> {
		self.host_id.as_deref()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

	/// Unpadded base64url, for crafting tokens.
	fn encode(bytes: &[u8]) -> String {
		let mut text = String::new();
		for chunk in bytes.chunks(3) {
			let group = chunk
				.iter()
				.fold(0u32, |group, byte| group << 8 | u32::from(*byte))
				<< (8 * (3 - chunk.len()));
			for index in 0..=chunk.len() {
				text.push(char::from(
					ALPHABET[(group >> (18 - 6 * index) & 63) as usize],
				));
			}
		}
		text
	}

	/// An unsigned JWT whose payload segment is `segment`.
	fn token(segment: &str) -> Secret {
		Secret::new(&format!("{}.{segment}.", encode(br#"{"alg":"none"}"#))).unwrap()
	}

	fn claims(payload: &str) -> Secret {
		token(&encode(payload.as_bytes()))
	}

	fn located(tunnel_id: &str, token: &Secret) -> (String, String) {
		match locator(tunnel_id, token).unwrap() {
			TunnelLocator::ID { cluster, id } => (id, cluster),
			TunnelLocator::Name(_) => panic!("expected an ID locator"),
		}
	}

	fn unknown(tunnel_id: &str, token: &Secret) {
		assert_eq!(
			locator(tunnel_id, token).unwrap_err().to_string(),
			format!("ValueError: {UNKNOWN_CLUSTER}"),
			"{tunnel_id}"
		);
	}

	#[test]
	fn base64url_known_vectors() {
		for (text, bytes) in [
			("", &b""[..]),
			("Zg", b"f"),
			("Zm8", b"fo"),
			("Zm9v", b"foo"),
			("Zm9vYg", b"foob"),
			("Zm9vYmE", b"fooba"),
			("Zm9vYmFy", b"foobar"),
			("Zg==", b"f"),
			("Zm8=", b"fo"),
			("-_8", &[0xfb, 0xff]),
		] {
			assert_eq!(base64url_decode(text).as_deref(), Some(bytes), "{text}");
		}
		for text in ["Z", "Zm9vY", "+/8", "Zm9v!", "Zg===", "Z===", "Zm 9v"] {
			assert_eq!(base64url_decode(text), None, "{text}");
		}
		let bytes: Vec<u8> = (0..=255).collect();
		for length in 0..bytes.len() {
			let text = encode(&bytes[..length]);
			assert_eq!(base64url_decode(&text).as_deref(), Some(&bytes[..length]));
		}
	}

	#[test]
	fn explicit_cluster_is_the_label_after_the_first_dot() {
		let opaque = Secret::new("opaque").unwrap();
		assert_eq!(
			located("example-api.usw2", &opaque),
			("example-api".to_owned(), "usw2".to_owned())
		);
		assert_eq!(
			located("example-api.usw2", &claims(r#"{"clusterId":"euw"}"#)).1,
			"usw2"
		);
		let claimed = claims(r#"{"clusterId":"usw2"}"#);
		unknown("example-api.", &claimed);
		unknown(".usw2", &claimed);
		// A cluster must be one host-name label, or the SDK panics building URLs.
		for id in [
			"a.b.c",
			"a..b",
			"a.us_w2",
			"a.xn--a",
			"a.XN--usw2",
			"a.ab--c",
		] {
			unknown(id, &opaque);
		}
	}

	#[test]
	fn bare_id_takes_cluster_claim() {
		// Payload lengths in every residue modulo 3, so the segment needs
		// zero, one or two padding characters; JWTs omit them, but accept them.
		for spaces in ["", " ", "  "] {
			let payload = format!(r#"{{"sub":"x","clusterId":"usw2"{spaces}}}"#);
			let segment = encode(payload.as_bytes());
			let padding = "=".repeat((4 - segment.len() % 4) % 4);
			for segment in [segment.clone(), format!("{segment}{padding}")] {
				assert_eq!(
					located("example-api", &token(&segment)),
					("example-api".to_owned(), "usw2".to_owned()),
					"{segment}"
				);
			}
		}
	}

	#[test]
	fn bare_id_without_usable_claim_is_unknown_cluster() {
		unknown("example-api", &Secret::new("opaque").unwrap());
		unknown("example-api", &Secret::new("only.one").unwrap());
		unknown("example-api", &token("not+base64url"));
		unknown("example-api", &token("Zm9v"));
		for payload in [
			r#"{"sub":"x"}"#,
			r#"{"clusterId":7}"#,
			r#"{"clusterId":null}"#,
			r#"{"clusterId":""}"#,
			r#"{"clusterId":"us/w2"}"#,
			r#"{"clusterId":"usw2.evil"}"#,
			r#"{"clusterId":"xn--a"}"#,
			r#"["usw2"]"#,
		] {
			unknown("example-api", &claims(payload));
		}
	}

	#[test]
	fn status_classification() {
		let refused = "check the CLI login and tunnel permissions under the service's Unix user";
		for (code, transient, message) in [
			(
				401,
				false,
				format!("Tunnel service refused the request (HTTP 401); {refused}"),
			),
			(
				403,
				false,
				format!("Tunnel service refused the request (HTTP 403); {refused}"),
			),
			(
				404,
				false,
				"Tunnel not found (HTTP 404); check the tunnel ID".to_owned(),
			),
			(
				408,
				true,
				"Tunnel service unavailable (HTTP 408)".to_owned(),
			),
			(
				429,
				true,
				"Tunnel service unavailable (HTTP 429)".to_owned(),
			),
			(
				500,
				true,
				"Tunnel service unavailable (HTTP 500)".to_owned(),
			),
			(
				503,
				true,
				"Tunnel service unavailable (HTTP 503)".to_owned(),
			),
			(
				599,
				true,
				"Tunnel service unavailable (HTTP 599)".to_owned(),
			),
			(
				300,
				false,
				"Tunnel service rejected the request (HTTP 300)".to_owned(),
			),
			(
				400,
				false,
				"Tunnel service rejected the request (HTTP 400)".to_owned(),
			),
			(
				409,
				false,
				"Tunnel service rejected the request (HTTP 409)".to_owned(),
			),
		] {
			let error = status_error(code);
			assert_eq!(error.to_string(), format!("RuntimeError: {message}"));
			assert_eq!(error.is_transient(), transient, "{code}");
		}
		let error = read_error(&HttpError::AuthorizationError("denied".to_owned()));
		assert_eq!(error.name(), "RuntimeError");
		assert!(!error.is_transient());
	}

	#[tokio::test]
	async fn auth_cell_serves_latest_authorization() {
		let cell = AuthCell::new(Authorization::Tunnel("first".to_owned()));
		let shared = cell.clone();
		cell.set(&Authorization::Tunnel("second".to_owned()));
		let Authorization::Tunnel(token) = shared.get_authorization().await.unwrap() else {
			panic!("expected tunnel authorization");
		};
		assert_eq!(token, "second");
	}

	#[test]
	fn service_resolves_locator_before_building_the_client() {
		let service = Service::new("example-api.usw2", &Secret::new("opaque").unwrap()).unwrap();
		assert!(matches!(
			service.locator,
			TunnelLocator::ID { ref cluster, ref id } if cluster == "usw2" && id == "example-api"
		));
		assert_eq!(
			Service::new("example-api", &Secret::new("opaque").unwrap())
				.err()
				.map(|error| error.to_string()),
			Some(format!("ValueError: {UNKNOWN_CLUSTER}"))
		);
	}

	#[test]
	fn sdk_text_drops_credentials_and_extra_lines() {
		let text = "error sending tunnel CONNECT request: response error: HTTP status 407 \
			Proxy Authentication Required from http://alice:s3cr3t@proxy.corp:3128/ \
			(request ID <none>): denied\n<0>forged";
		assert_eq!(
			sdk_text(&text),
			"error sending tunnel CONNECT request: response error: HTTP status 407 Proxy \
			 Authentication Required from http://proxy.corp:3128/ (request ID <none>): denied"
		);
		assert_eq!(
			sdk_text(
				&"see https://usw2.rel.tunnels.api.visualstudio.com/tunnels/x?api-version=1 and wss://u@h"
			),
			"see https://usw2.rel.tunnels.api.visualstudio.com/tunnels/x?api-version=1 and wss://h"
		);
	}
}
