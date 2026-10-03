//! The `host`, `renew` and `doctor` actions.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::credentials::Credentials;
use crate::error::Result;
use crate::service::{Api, Relay};

/// Interval of standby polls while another host serves the tunnel.
pub(crate) const STANDBY_POLL: Duration = Duration::from_secs(60);
/// First reconnection delay; doubles after each failed attempt.
pub(crate) const BACKOFF_INITIAL: Duration = Duration::from_secs(2);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A connection that lasted this long resets the backoff.
pub(crate) const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Bound on closing and unregistering after SIGTERM/SIGINT; systemd stops
/// the unit after 30 s.
pub(crate) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
	Host,
	Renew,
	Doctor,
}

/// Runs `action` for the instance configured at `config_path` with the `cli`
/// credential source: load the configuration, mint the first token through
/// the CLI, build the [`crate::service::Service`], then [`doctor`],
/// [`renew`] or [`host`] (until SIGTERM or SIGINT). `host` and `renew` log
/// [`crate::remote::ANONYMOUS_RISK`] as a warning when `allow_anonymous` is
/// enabled; `doctor` prints its line on stdout.
pub(crate) async fn run(action: Action, config_path: &Path) -> Result<()> {
	let _ = (action, config_path);
	todo!("host::run")
}

/// Reads and validates the tunnel; returns the line `doctor` prints, like
/// Python's `json.dumps`:
/// `{"status": "ready", "tunnel_id": "ID", "ports": [4000, 5000], "anonymous_access": false}`
/// where `anonymous_access` reports what the check observed.
pub(crate) async fn doctor(
	config: &Config,
	credentials: &impl Credentials,
	api: &impl Api,
) -> Result<String> {
	let _ = (config, credentials, api);
	todo!("host::doctor")
}

/// Validates the tunnel, then renews its lease through `credentials`.
/// Prints `Tunnel lease renewed; host connection not restarted.`
pub(crate) async fn renew(
	config: &Config,
	credentials: &impl Credentials,
	api: &impl Api,
) -> Result<()> {
	let _ = (config, credentials, api);
	todo!("host::renew")
}

/// Hosts the tunnel until `shutdown` resolves.
///
/// 1. Read and validate the tunnel (fatal on failure), then `add_port` each
///    configured port once.
/// 2. Never displace a live host: while [`crate::remote::other_host`]
///    holds, stand by, polling every [`STANDBY_POLL`].
/// 3. Mint a fresh host token and connect. Transient connection failures
///    retry with backoff from [`BACKOFF_INITIAL`] up to [`BACKOFF_MAX`].
/// 4. When the connection ends, read and validate the tunnel again. If
///    another host is connected, the host was displaced: log it, unregister
///    this host's endpoint (so no other instance waits on it; a failure is
///    logged and ignored), stand by as in 2, and log the takeover. Otherwise
///    reconnect with backoff; a connection that lasted
///    [`STABLE_CONNECTION`] resets the backoff.
///
/// Transient errors retry; other errors return, so the process exits and
/// systemd restarts it. While standing by, every error except a failed
/// validation retries at the poll interval, so that a restart can never
/// displace the healthy host. On shutdown, close the connection and
/// unregister within [`SHUTDOWN_TIMEOUT`], then return `Ok`.
pub(crate) async fn host<R: Relay>(
	config: &Config,
	credentials: &impl Credentials,
	api: &impl Api,
	relay: &mut R,
	shutdown: impl Future<Output = ()>,
) -> Result<()> {
	let _ = (config, credentials, api, relay, shutdown);
	todo!("host::host")
}
