//! Dev Tunnels SDK adapter: tunnel reads and the relay host.
//!
//! The SDK's error type is private, so SDK failures are classified here and
//! reported through their `Display` text only. `Debug` output and request
//! options must never be logged: they can carry the token.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tunnels::connections::{RelayHandle, RelayTunnelHost};
use tunnels::contracts::Tunnel;
use tunnels::management::{
	Authorization, AuthorizationProvider, BoxFuture, HttpError, TunnelLocator,
	TunnelManagementClient,
};

use crate::credentials::Secret;
use crate::error::{Error, Result};

pub(crate) const UNKNOWN_CLUSTER: &str =
	"Cannot determine the tunnel's cluster; configure the full tunnel ID (ID.CLUSTER)";
pub(crate) const UNRECOGNIZED_SCHEMA: &str = "Unrecognized tunnel schema; refusing to host";

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
		let _ = (tunnel_id, token);
		todo!("service::Service::new")
	}

	/// The relay host for this process.
	pub(crate) fn relay(&self) -> SdkRelay {
		let _ = (&self.mgmt, &self.locator, &self.auth);
		todo!("service::Service::relay")
	}
}

impl Api for Service {
	/// HTTP 401/403/404 and other 4xx are `RuntimeError`s naming the status
	/// and pointing at the CLI login and tunnel permissions; connection
	/// errors, 408, 429 and 5xx are transient; a response that does not
	/// deserialize (such as an unknown ACE type) is a `ValueError`,
	/// [`UNRECOGNIZED_SCHEMA`].
	async fn tunnel(&self, auth: &Authorization) -> Result<Tunnel> {
		let _ = (auth, Error::value(""));
		todo!("service::Service::tunnel")
	}
}

/// The locator for `tunnel_id`: `ID.CLUSTER` splits at its first dot;
/// a bare ID takes its cluster from the `clusterId` claim of `token` (a JWT
/// whose payload is decoded, never verified or logged). Without either:
/// `ValueError` [`UNKNOWN_CLUSTER`].
pub(crate) fn locator(tunnel_id: &str, token: &Secret) -> Result<TunnelLocator> {
	let _ = (tunnel_id, token);
	todo!("service::locator")
}

/// [`Relay`] over the SDK's `RelayTunnelHost`.
pub(crate) struct SdkRelay {
	host: RelayTunnelHost,
	auth: AuthCell,
	host_id: Option<String>,
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
		let _ = (port, auth, &self.host, &self.auth);
		todo!("service::SdkRelay::add_port")
	}

	/// Logs the forwarding URL of each port from the endpoint's
	/// `port_uri_format`, when the service provides one.
	async fn connect(&mut self, token: &Secret) -> Result<Connection> {
		let _ = token;
		todo!("service::SdkRelay::connect")
	}

	async fn close(&mut self, connection: Connection) {
		drop(connection);
		todo!("service::SdkRelay::close")
	}

	async fn unregister(&mut self, auth: &Authorization) -> Result<()> {
		let _ = auth;
		todo!("service::SdkRelay::unregister")
	}

	fn host_id(&self) -> Option<&str> {
		self.host_id.as_deref()
	}
}
