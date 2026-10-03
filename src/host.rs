//! The `host`, `renew` and `doctor` actions.

use std::convert::Infallible;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;
use tunnels::contracts::Tunnel;

use crate::config::{Config, json_string};
use crate::credentials::{Cli, Credentials};
use crate::error::{Error, Result};
use crate::log::{info, warning};
use crate::remote::{self, Checked};
use crate::service::{Api, Relay, Service};

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
	let config = Config::load(config_path)?;
	if config.allow_anonymous && action != Action::Doctor {
		warning(remote::ANONYMOUS_RISK);
	}
	let credentials = Cli::new(&config.binary, &config.tunnel_id);
	let token = credentials.host_token().await?;
	let service = Service::new(&config.tunnel_id, &token)?;
	match action {
		Action::Doctor => println!("{}", doctor(&config, &credentials, &service).await?),
		Action::Renew => renew(&config, &credentials, &service).await?,
		Action::Host => {
			let mut terminate = signal(SignalKind::terminate())?;
			let mut interrupt = signal(SignalKind::interrupt())?;
			let shutdown = async move {
				let name = tokio::select! {
					_ = terminate.recv() => "SIGTERM",
					_ = interrupt.recv() => "SIGINT",
				};
				info(format!("Received {name}"));
			};
			let mut relay = service.relay();
			host(&config, &credentials, &service, &mut relay, shutdown).await?;
		}
	}
	Ok(())
}

/// Reads the tunnel with the current read authorization.
async fn fetch(credentials: &impl Credentials, api: &impl Api) -> Result<Tunnel> {
	let auth = credentials.read_auth().await?;
	api.tunnel(&auth).await
}

/// Reads and validates the tunnel.
async fn check(config: &Config, credentials: &impl Credentials, api: &impl Api) -> Result<Checked> {
	let tunnel = fetch(credentials, api).await?;
	remote::validate(&tunnel, &config.ports, config.allow_anonymous)
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
	let checked = check(config, credentials, api).await?;
	let ports: Vec<String> = config.ports.iter().map(u16::to_string).collect();
	Ok(format!(
		"{{\"status\": \"ready\", \"tunnel_id\": {}, \"ports\": [{}], \"anonymous_access\": {}}}",
		json_string(&config.tunnel_id),
		ports.join(", "),
		checked.anonymous_access,
	))
}

/// Validates the tunnel, then renews its lease through `credentials`.
/// Prints `Tunnel lease renewed; host connection not restarted.`
pub(crate) async fn renew(
	config: &Config,
	credentials: &impl Credentials,
	api: &impl Api,
) -> Result<()> {
	check(config, credentials, api).await?;
	credentials.renew().await?;
	println!("Tunnel lease renewed; host connection not restarted.");
	Ok(())
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
	let mut shutdown = std::pin::pin!(shutdown);
	// Outside the serving future, so that shutdown can still close it.
	let mut live: Option<R::Connection> = None;
	// Dropping the serving future at shutdown interrupts whatever it awaits.
	let served = tokio::select! {
		biased;
		() = &mut shutdown => None,
		result = serve(config, credentials, api, relay, &mut live) => Some(result),
	};
	if let Some(result) = served {
		let Err(error) = result;
		return Err(error);
	}
	info(format!("Disconnecting from tunnel {}", config.tunnel_id));
	let disconnect = async {
		if let Some(connection) = live.take() {
			relay.close(connection).await;
		}
		unregister(config, credentials, relay).await;
	};
	if tokio::time::timeout(SHUTDOWN_TIMEOUT, disconnect)
		.await
		.is_err()
	{
		warning(format!(
			"Disconnecting from tunnel {} timed out after {}s",
			config.tunnel_id,
			SHUTDOWN_TIMEOUT.as_secs()
		));
	}
	Ok(())
}

/// Steps 1 to 4 of [`host`]; returns only on a fatal error. The current
/// connection lives in `live`.
async fn serve<R: Relay>(
	config: &Config,
	credentials: &impl Credentials,
	api: &impl Api,
	relay: &mut R,
	live: &mut Option<R::Connection>,
) -> Result<Infallible> {
	let id = &config.tunnel_id;
	let auth = credentials.read_auth().await?;
	let tunnel = api.tunnel(&auth).await?;
	remote::validate(&tunnel, &config.ports, config.allow_anonymous)?;
	for &port in &config.ports {
		relay.add_port(port, &auth).await?;
	}
	if remote::other_host(&tunnel, relay.host_id()) {
		info(format!("Another host is serving tunnel {id}; standing by"));
		standby(config, credentials, api, relay.host_id()).await?;
		info(format!(
			"No other host is connected to tunnel {id}; taking over"
		));
	}
	let mut backoff = BACKOFF_INITIAL;
	loop {
		let connection = match connect(credentials, relay).await {
			Ok(connection) => connection,
			Err(error) if error.is_transient() => {
				retry(&mut backoff, &error).await;
				continue;
			}
			Err(error) => return Err(error),
		};
		info(format!("Hosting tunnel {id}"));
		let connected = Instant::now();
		live.insert(connection).await;
		*live = None;
		if connected.elapsed() >= STABLE_CONNECTION {
			backoff = BACKOFF_INITIAL;
		}
		let tunnel = loop {
			match fetch(credentials, api).await {
				Ok(tunnel) => break tunnel,
				Err(error) if error.is_transient() => retry(&mut backoff, &error).await,
				Err(error) => return Err(error),
			}
		};
		remote::validate(&tunnel, &config.ports, config.allow_anonymous)?;
		if remote::other_host(&tunnel, relay.host_id()) {
			info(format!(
				"Another host connected to tunnel {id}; standing by"
			));
			unregister(config, credentials, relay).await;
			standby(config, credentials, api, relay.host_id()).await?;
			info(format!(
				"No other host is connected to tunnel {id}; taking over"
			));
		} else {
			info(format!(
				"Relay connection closed; reconnecting in {}s",
				backoff.as_secs()
			));
			pause(&mut backoff).await;
		}
	}
}

/// Mints a fresh host token and connects with it.
async fn connect<R: Relay>(credentials: &impl Credentials, relay: &mut R) -> Result<R::Connection> {
	let token = credentials.host_token().await?;
	relay.connect(&token).await
}

/// Polls every [`STANDBY_POLL`] until no other host serves the tunnel. Only a
/// failed validation ends the standby early.
async fn standby(
	config: &Config,
	credentials: &impl Credentials,
	api: &impl Api,
	own_host_id: Option<&str>,
) -> Result<()> {
	loop {
		tokio::time::sleep(STANDBY_POLL).await;
		match fetch(credentials, api).await {
			Ok(tunnel) => {
				remote::validate(&tunnel, &config.ports, config.allow_anonymous)?;
				if !remote::other_host(&tunnel, own_host_id) {
					return Ok(());
				}
			}
			Err(error) => warning(format!(
				"{error}; polling again in {}s",
				STANDBY_POLL.as_secs()
			)),
		}
	}
}

/// Removes this host's endpoint from the tunnel; a failure is logged.
async fn unregister<R: Relay>(config: &Config, credentials: &impl Credentials, relay: &mut R) {
	let result = match credentials.read_auth().await {
		Ok(auth) => relay.unregister(&auth).await,
		Err(error) => Err(error),
	};
	if let Err(error) = result {
		warning(format!(
			"Could not unregister from tunnel {}: {error}",
			config.tunnel_id
		));
	}
}

/// Logs a transient failure, then waits out the backoff.
async fn retry(backoff: &mut Duration, error: &Error) {
	warning(format!("{error}; retrying in {}s", backoff.as_secs()));
	pause(backoff).await;
}

/// Waits `backoff`, then doubles it up to [`BACKOFF_MAX`].
async fn pause(backoff: &mut Duration) {
	tokio::time::sleep(*backoff).await;
	*backoff = (*backoff * 2).min(BACKOFF_MAX);
}

#[cfg(test)]
mod tests {
	use std::cell::{Cell, RefCell};
	use std::collections::{HashMap, VecDeque};
	use std::pin::Pin;

	use serde_json::{Value, json};
	use tokio::time::Sleep;
	use tunnels::management::Authorization;

	use super::*;
	use crate::credentials::Secret;

	const DOCTOR: &str = r#"{"status": "ready", "tunnel_id": "example-api.usw2", "ports": [4000, 5000], "anonymous_access": false}"#;

	fn config(allow_anonymous: bool) -> Config {
		Config {
			tunnel_id: "example-api.usw2".into(),
			binary: "/usr/bin/devtunnel".into(),
			ports: vec![4000, 5000],
			allow_anonymous,
		}
	}

	fn port(number: u16) -> Value {
		json!({"portNumber": number, "accessControl": {"entries": []}})
	}

	/// A private tunnel with the configured ports, served by `hosts`.
	fn tunnel(hosts: &[&str]) -> Result<Tunnel> {
		let endpoints: Vec<Value> = hosts
			.iter()
			.map(|host| json!({"connectionMode": "TunnelRelay", "hostId": host}))
			.collect();
		Ok(serde_json::from_value(json!({
			"accessControl": {"entries": []},
			"ports": [port(4000), port(5000)],
			"endpoints": endpoints,
			"status": {"hostConnectionCount": hosts.len()},
		}))
		.unwrap())
	}

	/// A tunnel that grants anonymous access.
	fn anonymous() -> Result<Tunnel> {
		let mut tunnel = tunnel(&[])?;
		tunnel.access_control = Some(
			serde_json::from_value(json!({
				"entries": [{"type": "Anonymous", "subjects": [], "scopes": ["connect"]}],
			}))
			.unwrap(),
		);
		Ok(tunnel)
	}

	fn token(auth: &Authorization) -> String {
		match auth {
			Authorization::Tunnel(token) => token.clone(),
			_ => panic!("unexpected authorization scheme"),
		}
	}

	/// Tokens `host-N` and authorizations `read-N`, where N counts the calls;
	/// the calls listed in `fail_*` fail instead.
	#[derive(Default)]
	struct FakeCredentials {
		tokens: Cell<usize>,
		reads: Cell<usize>,
		renewals: Cell<usize>,
		fail_token: RefCell<HashMap<usize, Error>>,
		fail_read: RefCell<HashMap<usize, Error>>,
	}

	fn count(cell: &Cell<usize>) -> usize {
		cell.set(cell.get() + 1);
		cell.get()
	}

	impl Credentials for FakeCredentials {
		async fn host_token(&self) -> Result<Secret> {
			let call = count(&self.tokens);
			match self.fail_token.borrow_mut().remove(&call) {
				Some(error) => Err(error),
				None => Secret::new(&format!("host-{call}")),
			}
		}

		async fn read_auth(&self) -> Result<Authorization> {
			let call = count(&self.reads);
			match self.fail_read.borrow_mut().remove(&call) {
				Some(error) => Err(error),
				None => Ok(Authorization::Tunnel(format!("read-{call}"))),
			}
		}

		async fn renew(&self) -> Result<()> {
			count(&self.renewals);
			Ok(())
		}
	}

	/// Answers tunnel reads from a script; reading past its end fails the test.
	struct FakeApi {
		script: RefCell<VecDeque<Result<Tunnel>>>,
		reads: Cell<usize>,
	}

	impl FakeApi {
		fn new(script: impl IntoIterator<Item = Result<Tunnel>>) -> Self {
			Self {
				script: RefCell::new(script.into_iter().collect()),
				reads: Cell::new(0),
			}
		}
	}

	impl Api for FakeApi {
		async fn tunnel(&self, auth: &Authorization) -> Result<Tunnel> {
			assert!(token(auth).starts_with("read-"));
			count(&self.reads);
			let next = self.script.borrow_mut().pop_front();
			next.expect("unscripted tunnel read")
		}
	}

	#[derive(Debug, PartialEq, Eq)]
	enum Event {
		AddPort(u16, String),
		Connect(String),
		Close,
		Unregister(String),
	}

	/// Records calls with the second they happened at. `connects` scripts
	/// each connection's lifetime or failure.
	struct FakeRelay {
		start: Instant,
		events: Vec<(u64, Event)>,
		connects: VecDeque<Result<Duration>>,
		host_id: Option<String>,
		unregister_fails: bool,
		unregister_hangs: bool,
	}

	impl FakeRelay {
		fn new(connects: impl IntoIterator<Item = Result<Duration>>) -> Self {
			Self {
				start: Instant::now(),
				events: Vec::new(),
				connects: connects.into_iter().collect(),
				host_id: None,
				unregister_fails: false,
				unregister_hangs: false,
			}
		}

		fn record(&mut self, event: Event) {
			self.events.push((self.start.elapsed().as_secs(), event));
		}
	}

	impl Relay for FakeRelay {
		type Connection = Pin<Box<Sleep>>;

		async fn add_port(&mut self, port: u16, auth: &Authorization) -> Result<()> {
			self.record(Event::AddPort(port, token(auth)));
			Ok(())
		}

		async fn connect(&mut self, token: &Secret) -> Result<Self::Connection> {
			self.record(Event::Connect(token.expose().to_owned()));
			let lifetime = self.connects.pop_front().expect("unscripted connect")?;
			self.host_id = Some("me".into());
			Ok(Box::pin(tokio::time::sleep(lifetime)))
		}

		async fn close(&mut self, _connection: Self::Connection) {
			self.record(Event::Close);
		}

		async fn unregister(&mut self, auth: &Authorization) -> Result<()> {
			self.record(Event::Unregister(token(auth)));
			if self.unregister_hangs {
				std::future::pending::<()>().await;
			}
			if self.unregister_fails {
				return Err(Error::transient("unregister failed"));
			}
			Ok(())
		}

		fn host_id(&self) -> Option<&str> {
			self.host_id.as_deref()
		}
	}

	const FOREVER: Duration = Duration::from_secs(24 * 60 * 60);

	fn lasting(seconds: u64) -> Result<Duration> {
		Ok(Duration::from_secs(seconds))
	}

	fn transient() -> Error {
		Error::transient("HTTP 503 Service Unavailable")
	}

	fn add_ports() -> [(u64, Event); 2] {
		[
			(0, Event::AddPort(4000, "read-1".into())),
			(0, Event::AddPort(5000, "read-1".into())),
		]
	}

	fn connect_at(second: u64, token: usize) -> (u64, Event) {
		(second, Event::Connect(format!("host-{token}")))
	}

	fn unregister_at(second: u64, read: usize) -> (u64, Event) {
		(second, Event::Unregister(format!("read-{read}")))
	}

	/// Hosts until the fakes fail or `stop_after` elapses.
	async fn serve(
		credentials: &FakeCredentials,
		api: &FakeApi,
		relay: &mut FakeRelay,
		stop_after: Duration,
	) -> Result<()> {
		host(
			&config(false),
			credentials,
			api,
			relay,
			tokio::time::sleep(stop_after),
		)
		.await
	}

	#[tokio::test(start_paused = true)]
	async fn startup_validation_failure_adds_no_ports() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([anonymous()]);
		let mut relay = FakeRelay::new([]);
		let error = serve(&credentials, &api, &mut relay, FOREVER)
			.await
			.unwrap_err();
		assert_eq!(
			error.to_string(),
			format!("ValueError: {}", remote::ANONYMOUS_REFUSED)
		);
		assert!(relay.events.is_empty());
		assert_eq!(credentials.tokens.get(), 0);
	}

	#[tokio::test(start_paused = true)]
	async fn reconnects_with_doubling_backoff_and_fresh_tokens() {
		let credentials = FakeCredentials::default();
		// Startup, then one read after each of the eight ended connections.
		let api = FakeApi::new(
			[tunnel(&[])]
				.into_iter()
				.chain((0..8).map(|_| tunnel(&["me"]))),
		);
		let mut relay = FakeRelay::new(
			(0..7)
				.map(|_| lasting(1))
				.chain([Ok(STABLE_CONNECTION), Ok(FOREVER)]),
		);
		serve(&credentials, &api, &mut relay, Duration::from_secs(400))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		// Waits of 2, 4, 8, 16, 32, 60 and 60 s after 1 s connections; the
		// stable connection (189 to 249 s) resets the wait to 2 s.
		for (token, second) in [0, 3, 8, 17, 34, 67, 128, 189, 251].into_iter().enumerate() {
			expected.push(connect_at(second, token + 1));
		}
		expected.push((400, Event::Close));
		expected.push(unregister_at(400, 10));
		assert_eq!(relay.events, expected);
		assert_eq!(api.reads.get(), 9);
	}

	#[tokio::test(start_paused = true)]
	async fn displaced_host_unregisters_stands_by_and_takes_over() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([
			tunnel(&[]),
			tunnel(&["me", "them"]),
			tunnel(&["them"]),
			tunnel(&["them"]),
			tunnel(&[]),
		]);
		let mut relay = FakeRelay::new([lasting(10), Ok(FOREVER)]);
		serve(&credentials, &api, &mut relay, Duration::from_secs(250))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		expected.extend([
			connect_at(0, 1),
			unregister_at(10, 3),
			// Standby polls at 70, 130 and 190 s; the last finds no host.
			connect_at(190, 2),
			(250, Event::Close),
			unregister_at(250, 7),
		]);
		assert_eq!(relay.events, expected);
		assert_eq!(api.reads.get(), 5);
	}

	#[tokio::test(start_paused = true)]
	async fn displacement_survives_a_failed_unregister() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&[]), tunnel(&["me", "them"]), tunnel(&["me"])]);
		let mut relay = FakeRelay::new([lasting(10), Ok(FOREVER)]);
		relay.unregister_fails = true;
		serve(&credentials, &api, &mut relay, Duration::from_secs(100))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		expected.extend([
			connect_at(0, 1),
			unregister_at(10, 3),
			connect_at(70, 2),
			(100, Event::Close),
			unregister_at(100, 5),
		]);
		assert_eq!(relay.events, expected);
	}

	#[tokio::test(start_paused = true)]
	async fn waits_for_another_host_at_startup() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&["them"]), tunnel(&["them"]), tunnel(&[])]);
		let mut relay = FakeRelay::new([Ok(FOREVER)]);
		serve(&credentials, &api, &mut relay, Duration::from_secs(150))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		expected.extend([
			connect_at(120, 1),
			(150, Event::Close),
			unregister_at(150, 4),
		]);
		assert_eq!(relay.events, expected);
	}

	#[tokio::test(start_paused = true)]
	async fn transient_connect_token_and_read_errors_retry() {
		let credentials = FakeCredentials::default();
		credentials.fail_token.borrow_mut().insert(2, transient());
		let api = FakeApi::new([tunnel(&[]), Err(transient()), tunnel(&["me"])]);
		let mut relay = FakeRelay::new([Err(transient()), lasting(1), Ok(FOREVER)]);
		serve(&credentials, &api, &mut relay, Duration::from_secs(100))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		expected.extend([
			// Connect fails: wait 2 s. Token 2 fails: wait 4 s.
			connect_at(0, 1),
			connect_at(6, 3),
			// The 1 s connection ends at 7 s; the read fails: wait 8 s, then
			// reconnect after 16 s.
			connect_at(31, 4),
			(100, Event::Close),
			unregister_at(100, 4),
		]);
		assert_eq!(relay.events, expected);
	}

	#[tokio::test(start_paused = true)]
	async fn persistent_error_after_disconnect_returns() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&[]), Err(Error::runtime("HTTP 401 Unauthorized"))]);
		let mut relay = FakeRelay::new([lasting(5)]);
		let error = serve(&credentials, &api, &mut relay, FOREVER)
			.await
			.unwrap_err();
		assert_eq!(error.to_string(), "RuntimeError: HTTP 401 Unauthorized");
		let mut expected = Vec::from(add_ports());
		expected.push(connect_at(0, 1));
		assert_eq!(relay.events, expected);
	}

	#[tokio::test(start_paused = true)]
	async fn anonymous_access_at_reconnect_returns() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&[]), anonymous()]);
		let mut relay = FakeRelay::new([lasting(5)]);
		let error = serve(&credentials, &api, &mut relay, FOREVER)
			.await
			.unwrap_err();
		assert_eq!(
			error.to_string(),
			format!("ValueError: {}", remote::ANONYMOUS_REFUSED)
		);
		assert_eq!(credentials.tokens.get(), 1);
	}

	#[tokio::test(start_paused = true)]
	async fn standby_tolerates_errors_until_validation_fails() {
		let credentials = FakeCredentials::default();
		// Read 2 is the first poll: a failed credential command.
		credentials
			.fail_read
			.borrow_mut()
			.insert(2, Error::runtime("devtunnel token failed with exit 1"));
		let api = FakeApi::new([
			tunnel(&["them"]),
			Err(transient()),
			Err(Error::runtime("HTTP 403 Forbidden")),
			anonymous(),
		]);
		let mut relay = FakeRelay::new([]);
		let error = serve(&credentials, &api, &mut relay, FOREVER)
			.await
			.unwrap_err();
		assert_eq!(
			error.to_string(),
			format!("ValueError: {}", remote::ANONYMOUS_REFUSED)
		);
		assert_eq!(relay.start.elapsed(), STANDBY_POLL * 4);
		assert_eq!(api.reads.get(), 4);
		assert_eq!(relay.events, add_ports());
	}

	#[tokio::test(start_paused = true)]
	async fn shutdown_during_backoff_unregisters() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&[])]);
		let mut relay = FakeRelay::new([Err(transient())]);
		serve(&credentials, &api, &mut relay, Duration::from_secs(1))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		expected.extend([connect_at(0, 1), unregister_at(1, 2)]);
		assert_eq!(relay.events, expected);
	}

	#[tokio::test(start_paused = true)]
	async fn shutdown_during_standby_unregisters() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&["them"])]);
		let mut relay = FakeRelay::new([]);
		relay.unregister_fails = true;
		serve(&credentials, &api, &mut relay, Duration::from_secs(30))
			.await
			.unwrap();
		let mut expected = Vec::from(add_ports());
		expected.push(unregister_at(30, 2));
		assert_eq!(relay.events, expected);
	}

	#[tokio::test(start_paused = true)]
	async fn shutdown_is_bounded() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&[])]);
		let mut relay = FakeRelay::new([Ok(FOREVER)]);
		relay.unregister_hangs = true;
		serve(&credentials, &api, &mut relay, Duration::from_secs(30))
			.await
			.unwrap();
		assert_eq!(
			relay.start.elapsed(),
			Duration::from_secs(30) + SHUTDOWN_TIMEOUT
		);
		assert_eq!(
			relay.events[3..],
			[(30, Event::Close), unregister_at(30, 2)]
		);
	}

	#[tokio::test]
	async fn doctor_line_matches_python() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([tunnel(&[])]);
		assert_eq!(
			doctor(&config(false), &credentials, &api).await.unwrap(),
			DOCTOR
		);
		let api = FakeApi::new([anonymous()]);
		assert_eq!(
			doctor(&config(true), &credentials, &api).await.unwrap(),
			DOCTOR.replace("false", "true")
		);
		let api = FakeApi::new([anonymous()]);
		assert_eq!(
			doctor(&config(false), &credentials, &api)
				.await
				.unwrap_err()
				.to_string(),
			format!("ValueError: {}", remote::ANONYMOUS_REFUSED)
		);
		assert_eq!(credentials.tokens.get(), 0);
		assert_eq!(credentials.renewals.get(), 0);
	}

	#[tokio::test]
	async fn renew_validates_first() {
		let credentials = FakeCredentials::default();
		let api = FakeApi::new([anonymous()]);
		renew(&config(false), &credentials, &api).await.unwrap_err();
		assert_eq!(credentials.renewals.get(), 0);
		let api = FakeApi::new([tunnel(&["them"])]);
		renew(&config(false), &credentials, &api).await.unwrap();
		assert_eq!(credentials.renewals.get(), 1);
		assert_eq!(credentials.reads.get(), 2);
	}
}
