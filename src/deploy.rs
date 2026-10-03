//! Install a user-level devtunnel host and daily lease-renewal timer.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::PROGRAM;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths::{absolute, expand_user, is_executable, resolve, which};
use crate::process::Captured;
use crate::remote::ANONYMOUS_RISK;

pub(crate) const MARKER: &str = "# Managed by devtunnel-service\n";
pub(crate) const REMEDY: &str = "install devtunnel-service persistently (uv tool install \
	devtunnel-service, the Debian package or Homebrew) and run that command, or pass --entry-point";

/// Limit for each `systemctl --system show` ownership query.
const SHOW_TIMEOUT: Duration = Duration::from_secs(15);
/// The `[Service]` settings shared by both services.
const SETTINGS: &str = "UMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\n";

/// Arguments of `deploy`.
#[derive(Clone, Debug)]
pub(crate) struct Request {
	pub name: String,
	pub tunnel_id: String,
	/// As given; validated by [`crate::config::Config::from_value`].
	pub ports: Vec<i64>,
	pub binary: Option<String>,
	pub entry_point: Option<PathBuf>,
	pub dry_run: bool,
	pub start: bool,
	pub allow_anonymous: bool,
}

/// The process environment that deployment reads, captured once so tests
/// can supply their own.
#[derive(Clone, Debug)]
pub(crate) struct Environment {
	pub home: PathBuf,
	/// `XDG_CONFIG_HOME`, unset or empty as `None`.
	pub config_home: Option<PathBuf>,
	/// `XDG_CACHE_HOME`, unset or empty as `None`.
	pub cache_home: Option<PathBuf>,
	/// `UV_CACHE_DIR`, unset or empty as `None`.
	pub uv_cache_dir: Option<PathBuf>,
	/// `PATH`, for finding `devtunnel` and `devtunnel-service`.
	pub path: Option<OsString>,
	/// `tempfile.gettempdir()`: `std::env::temp_dir()`.
	pub temp_dir: PathBuf,
}

impl Environment {
	pub(crate) fn from_process() -> Result<Self> {
		let home = std::env::home_dir()
			.ok_or_else(|| Error::runtime("Could not determine home directory."))?;
		let var = |name: &str| {
			std::env::var_os(name)
				.filter(|value| !value.is_empty())
				.map(PathBuf::from)
		};
		Ok(Self {
			home,
			config_home: var("XDG_CONFIG_HOME"),
			cache_home: var("XDG_CACHE_HOME"),
			uv_cache_dir: var("UV_CACHE_DIR"),
			path: std::env::var_os("PATH"),
			temp_dir: std::env::temp_dir(),
		})
	}
}

/// What deployment needs from the machine; tests substitute it.
pub(crate) trait System {
	fn is_linux(&self) -> bool;
	/// [`crate::process::capture`].
	async fn capture(&self, argv: &[OsString], timeout: Duration) -> Result<Captured>;
	/// [`crate::process::check`].
	async fn check(&self, argv: &[OsString]) -> Result<()>;
}

/// This machine.
pub(crate) struct Machine;

impl System for Machine {
	fn is_linux(&self) -> bool {
		cfg!(target_os = "linux")
	}

	async fn capture(&self, argv: &[OsString], timeout: Duration) -> Result<Captured> {
		crate::process::capture(argv, timeout).await
	}

	async fn check(&self, argv: &[OsString]) -> Result<()> {
		crate::process::check(argv).await
	}
}

/// Quotes a path for a unit file like Python's `quote`: control characters
/// (below U+0020) are a `ValueError`, `Control characters are not supported
/// in service paths`; `\` and `"` are backslash-escaped, `%` doubled, and the
/// result wrapped in double quotes.
pub(crate) fn quote(value: &str) -> Result<String> {
	if value.chars().any(|char| char < ' ') {
		return Err(Error::value(
			"Control characters are not supported in service paths",
		));
	}
	let escaped = value
		.replace('\\', "\\\\")
		.replace('"', "\\\"")
		.replace('%', "%%");
	Ok(format!("\"{escaped}\""))
}

/// `str(path)` for text that must hold it: Python would carry undecodable
/// bytes as surrogates, which no unit or configuration file can represent.
fn utf8(path: &Path) -> Result<&str> {
	path.to_str().ok_or_else(|| {
		Error::value(format!(
			"Service paths must be valid UTF-8: {}",
			path.display()
		))
	})
}

/// `[a-z0-9][a-z0-9-]{0,47}` in full.
fn is_instance_name(name: &str) -> bool {
	let bytes = name.as_bytes();
	(1..=48).contains(&bytes.len())
		&& bytes[0] != b'-'
		&& bytes
			.iter()
			.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// The three units in order (`devtunnel-NAME.service`,
/// `devtunnel-NAME-renew.service`, `devtunnel-NAME-renew.timer`), byte-identical
/// to Python's `render_units` except that the `[Service]` sections omit
/// `Environment=PYTHONDONTWRITEBYTECODE=1`, which has no meaning for this
/// binary. `name` must match `[a-z0-9][a-z0-9-]{0,47}` in full, else
/// `ValueError` `Instance name must contain lowercase letters, digits and hyphens`.
pub(crate) fn render_units(
	name: &str,
	config_path: &Path,
	entry: &Path,
) -> Result<Vec<(String, String)>> {
	if !is_instance_name(name) {
		return Err(Error::value(
			"Instance name must contain lowercase letters, digits and hyphens",
		));
	}
	let prefix = format!("devtunnel-{name}");
	let command = quote(utf8(entry)?)?;
	let config = quote(utf8(config_path)?)?;
	let service = format!(
		"{MARKER}[Unit]
Description=Persistent devtunnel host ({name})
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
{SETTINGS}ExecStart={command} host --config {config}
Restart=always
RestartSec=30
KillSignal=SIGINT
TimeoutStopSec=30

[Install]
WantedBy=default.target
"
	);
	let renewal = format!(
		"{MARKER}[Unit]
Description=Renew devtunnel lease ({name})
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
{SETTINGS}ExecStart={command} renew --config {config}
TimeoutStartSec=180
"
	);
	let timer = format!(
		"{MARKER}[Unit]
Description=Daily devtunnel lease renewal ({name})

[Timer]
OnCalendar=daily
RandomizedDelaySec=300
Persistent=true
Unit={prefix}-renew.service

[Install]
WantedBy=timers.target
"
	);
	Ok(vec![
		(format!("{prefix}.service"), service),
		(format!("{prefix}-renew.service"), renewal),
		(format!("{prefix}-renew.timer"), timer),
	])
}

/// Maps an I/O failure on `path` to Python's `OSError` naming it.
fn at(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
	move |error| Error::io(&error, Some(path))
}

/// Creates `dir/{prefix}{random}` with `create`, retrying on name
/// collisions, like `tempfile.mkstemp` and `tempfile.mkdtemp`.
fn create_unique<T>(
	dir: &Path,
	prefix: &OsStr,
	create: impl Fn(&Path) -> io::Result<T>,
) -> Result<(PathBuf, T)> {
	loop {
		let mut name = prefix.to_owned();
		name.push(format!("{:016x}", RandomState::new().hash_one(())));
		let path = dir.join(name);
		match create(&path) {
			Ok(created) => return Ok((path, created)),
			Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
			Err(error) => return Err(Error::io(&error, Some(&path))),
		}
	}
}

/// Writes `text` through a mode-0600 temporary file in the same directory
/// (`.NAME` plus a random suffix), fsyncs it, renames it over `path`, then
/// fsyncs the directory. The temporary file never survives a failure.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
	let dir = match path.parent() {
		Some(parent) if !parent.as_os_str().is_empty() => parent,
		_ => Path::new("."),
	};
	let mut prefix = OsString::from(".");
	prefix.push(path.file_name().unwrap_or_default());
	let (temporary, mut file) = create_unique(dir, &prefix, |candidate| {
		OpenOptions::new()
			.write(true)
			.create_new(true)
			.mode(0o600)
			.open(candidate)
	})?;
	let written = file
		.write_all(text.as_bytes())
		.and_then(|()| file.sync_all())
		.map_err(at(&temporary))
		.and_then(|()| fs::rename(&temporary, path).map_err(at(path)));
	if let Err(error) = written {
		// Report the write failure, not a failure to clean up after it.
		let _ = fs::remove_file(&temporary);
		return Err(error);
	}
	File::open(dir)
		.and_then(|handle| handle.sync_all())
		.map_err(at(dir))
}

/// `Path(path).expanduser().resolve()`, which is not strict: what cannot be
/// resolved stays as given.
fn resolve_lenient(path: &Path, home: &Path) -> PathBuf {
	let path = expand_user(path, home);
	resolve(&path).unwrap_or(path)
}

/// Directories whose contents may disappear: the temporary directory,
/// `/tmp`, `/var/tmp`, the XDG cache (default `~/.cache`) and `UV_CACHE_DIR`,
/// each resolved.
pub(crate) fn transient_roots(env: &Environment) -> Vec<PathBuf> {
	let cache = env
		.cache_home
		.clone()
		.unwrap_or_else(|| env.home.join(".cache"));
	[
		env.temp_dir.clone(),
		"/tmp".into(),
		"/var/tmp".into(),
		cache,
	]
	.into_iter()
	.chain(env.uv_cache_dir.clone())
	.map(|root| resolve_lenient(&root, &env.home))
	.collect()
}

/// The nearest proper ancestor of `path` that marks a cache (`CACHEDIR.TAG`)
/// or, unless it is `home`, a source tree, with its kind.
fn enclosing<'a>(path: &'a Path, home: &Path) -> Option<(&'static str, &'a Path)> {
	path.ancestors().skip(1).find_map(|parent| {
		if parent.join("CACHEDIR.TAG").is_file() {
			Some(("cache", parent))
		} else if parent != home
			&& ["pyproject.toml", "Cargo.toml"]
				.iter()
				.any(|marker| parent.join(marker).is_file())
		{
			Some(("source tree", parent))
		} else {
			None
		}
	})
}

/// The entries of `dir` whose names start with `prefix` and end with
/// `suffix`, sorted. Like a glob, an unreadable directory matches nothing.
fn matching(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
	let mut found: Vec<PathBuf> = fs::read_dir(dir)
		.into_iter()
		.flatten()
		.flatten()
		.filter(|entry| {
			let name = entry.file_name();
			let name = name.as_encoded_bytes();
			name.starts_with(prefix.as_bytes()) && name.ends_with(suffix.as_bytes())
		})
		.map(|entry| entry.path())
		.collect();
	found.sort();
	found
}

/// Whether the PEP 610 record at `record` marks an editable install; `None`
/// if it cannot be read as a JSON object. Any `editable` value other than
/// `false` or `null` counts, failing closed.
fn editable(record: &Path) -> Option<bool> {
	let value: Value = serde_json::from_slice(&fs::read(record).ok()?).ok()?;
	let flag = match value.as_object()?.get("dir_info") {
		None => None,
		Some(info) => info.as_object()?.get("editable"),
	};
	Some(!matches!(
		flag,
		None | Some(Value::Null | Value::Bool(false))
	))
}

/// Why a Python environment cannot host the units' command.
fn environment_problem(environment: &Path, home: &Path) -> Option<String> {
	// uv tags each venv itself with CACHEDIR.TAG; only a tag above it marks a
	// cache (uvx), wherever uv.toml or --cache-dir put that cache.
	if let Some((kind, dir)) = enclosing(environment, home) {
		return Some(format!(
			"its environment {} is inside the {kind} {}",
			environment.display(),
			dir.display()
		));
	}
	let records = matching(environment, "lib", "")
		.into_iter()
		.flat_map(|lib| matching(&lib, "python", ""))
		.flat_map(|python| {
			matching(
				&python.join("site-packages"),
				"devtunnel_service-",
				".dist-info",
			)
		})
		.map(|info| info.join("direct_url.json"))
		.filter(|record| record.exists());
	for record in records {
		match editable(&record) {
			Some(false) => {}
			Some(true) => {
				return Some(format!(
					"it is an editable install that imports from a source tree ({})",
					record.display()
				));
			}
			// Python fails on a malformed record; the deployment refuses too.
			None => {
				return Some(format!(
					"its install record {} is unreadable",
					record.display()
				));
			}
		}
	}
	None
}

/// Why units must not reference `path` itself.
fn path_problem(path: &Path, roots: &[PathBuf], home: &Path) -> Option<String> {
	if let Some(root) = roots.iter().find(|root| path.starts_with(root)) {
		return Some(format!(
			"{} is inside the temporary or cache directory {}",
			path.display(),
			root.display()
		));
	}
	let environment = path
		.ancestors()
		.skip(1)
		.find(|parent| parent.join("pyvenv.cfg").is_file());
	match environment {
		Some(environment) => environment_problem(environment, home)
			.map(|problem| format!("{}: {problem}", path.display())),
		// A bare binary, as cargo builds it into target/ (tagged CACHEDIR.TAG).
		None => enclosing(path, home).map(|(kind, dir)| {
			format!("{} is inside the {kind} {}", path.display(), dir.display())
		}),
	}
}

/// Why units must not reference `entry`; `None` if it is persistent. Checks
/// both `entry` and its resolved target, with Python's messages:
///
/// - inside a transient root: `{path} is inside the temporary or cache directory {root}`;
/// - inside a Python environment (nearest ancestor with `pyvenv.cfg`), any
///   ancestor of that environment holding `CACHEDIR.TAG` (uv tags the venv
///   itself, so only a tag above it marks a cache):
///   `{path}: its environment {env} is inside the cache {dir}`; holding
///   `pyproject.toml` or `Cargo.toml`, other than `home`:
///   `{path}: its environment {env} is inside the source tree {dir}`; an
///   editable install (`dir_info.editable` in
///   `lib*/python*/site-packages/devtunnel_service-*.dist-info/direct_url.json`):
///   `{path}: it is an editable install that imports from a source tree ({record})`;
/// - outside any environment (a bare binary, as built by cargo), any
///   ancestor holding `CACHEDIR.TAG` (cargo tags `target/`):
///   `{path} is inside the cache {dir}`; holding `Cargo.toml` or
///   `pyproject.toml`, other than `home`: `{path} is inside the source tree {dir}`.
pub(crate) fn persistence_problem(entry: &Path, roots: &[PathBuf], home: &Path) -> Option<String> {
	// Check the PATH-visible link and its target: both must survive the session.
	let target = resolve(entry).unwrap_or_else(|_| entry.to_path_buf());
	[entry, target.as_path()]
		.into_iter()
		.find_map(|path| path_problem(path, roots, home))
}

/// `explicit` or `devtunnel-service` on `env.path`, made absolute with `~`
/// expanded but symbolic links kept: `/usr/bin`, `~/.local/bin` and
/// Homebrew's `bin` stay valid across upgrades, their versioned targets do
/// not. None found: `ValueError` `No devtunnel-service command on PATH; {REMEDY}`.
pub(crate) fn entry_point(explicit: Option<&Path>, env: &Environment) -> Result<PathBuf> {
	let found = match explicit {
		Some(explicit) => explicit.to_path_buf(),
		None => which(PROGRAM, env.path.as_deref())
			.ok_or_else(|| Error::value(format!("No {PROGRAM} command on PATH; {REMEDY}")))?,
	};
	let found = expand_user(&found, &env.home);
	absolute(&found).map_err(at(&found))
}

/// `path` with `.previous` appended to its name.
fn previous(path: &Path) -> PathBuf {
	let mut name = path.as_os_str().to_owned();
	name.push(".previous");
	PathBuf::from(name)
}

fn read_text(path: &Path) -> Result<String> {
	fs::read_to_string(path).map_err(at(path))
}

/// Writes `units` into a fresh private directory under `temp_dir`, runs
/// `systemd-analyze --user verify` on them, and removes the directory
/// whether or not they passed.
async fn verify(units: &[(String, String)], temp_dir: &Path, system: &impl System) -> Result<()> {
	let (folder, ()) = create_unique(temp_dir, OsStr::new("devtunnel-units-"), |candidate| {
		DirBuilder::new().mode(0o700).create(candidate)
	})?;
	let verified = async {
		let mut argv = crate::argv!["systemd-analyze", "--user", "verify"].to_vec();
		for (unit, text) in units {
			let path = folder.join(unit);
			fs::write(&path, text).map_err(at(&path))?;
			argv.push(path.into());
		}
		system.check(&argv).await
	}
	.await;
	let removed = fs::remove_dir_all(&folder).map_err(at(&folder));
	verified.and(removed)
}

/// Python's `deploy`, step for step, writing what Python prints to `out`
/// (stdout) and `err` (stderr). Differences: `allow_anonymous` comes from
/// the request, and when it is enabled [`crate::remote::ANONYMOUS_RISK`] is
/// written to `err` as `Warning: ...` before anything else, dry run
/// included. The `--start` confirmation reads `Hosting requested. Inspect
/// systemctl status/journal; host readiness is not yet certified.`
pub(crate) async fn deploy(
	request: &Request,
	env: &Environment,
	system: &impl System,
	out: &mut dyn Write,
	err: &mut dyn Write,
) -> Result<()> {
	if request.allow_anonymous {
		writeln!(err, "Warning: {ANONYMOUS_RISK}")?;
	}
	let binary = match request
		.binary
		.as_deref()
		.filter(|binary| !binary.is_empty())
	{
		Some(binary) => PathBuf::from(binary),
		None => which("devtunnel", env.path.as_deref()).ok_or_else(|| {
			Error::value("Install the official devtunnel CLI first, or pass --binary")
		})?,
	};
	let binary = expand_user(&binary, &env.home);
	let devtunnel = resolve(&binary).map_err(at(&binary))?;
	let mut ports = request.ports.clone();
	ports.sort_unstable();
	let config = Config::from_value(&json!({
		"tunnel_id": request.tunnel_id,
		"binary": utf8(&devtunnel)?,
		"ports": ports,
		"allow_anonymous": request.allow_anonymous,
	}))?;
	let config_root = expand_user(
		env.config_home
			.as_deref()
			.unwrap_or(&env.home.join(".config")),
		&env.home,
	);
	let config_root = resolve(&config_root).map_err(at(&config_root))?;
	let config_dir = config_root.join("devtunnel-service");
	let config_path = config_dir.join(format!("{}.json", request.name));
	let command = entry_point(request.entry_point.as_deref(), env)?;
	let home = resolve(&env.home).map_err(at(&env.home))?;
	let problem = persistence_problem(&command, &transient_roots(env), &home);
	let units = render_units(&request.name, &config_path, &command)?;
	if request.dry_run {
		for (unit, text) in &units {
			writeln!(out, "# {unit}\n{text}")?;
		}
		if let Some(problem) = problem {
			writeln!(
				err,
				"Note: deployment would refuse this entry point: {problem}; {REMEDY}."
			)?;
		}
		return Ok(());
	}
	if let Some(problem) = problem {
		return Err(Error::value(format!(
			"Refusing a non-persistent entry point: {problem}; {REMEDY}"
		)));
	}
	if !system.is_linux() || !is_executable(&devtunnel) {
		return Err(Error::value(
			"Linux/systemd and an executable devtunnel CLI are required",
		));
	}
	if !is_executable(&command) {
		return Err(Error::value(format!(
			"Entry point {} is not executable; {REMEDY}",
			command.display()
		)));
	}
	// Do not shadow an existing system-level tunnel (especially the SSH tunnel).
	for (unit, _) in &units {
		let argv = crate::argv![
			"systemctl",
			"--system",
			"show",
			unit,
			"-p",
			"LoadState",
			"--value"
		];
		let result = system.capture(&argv, SHOW_TIMEOUT).await?;
		let stdout = String::from_utf8_lossy(&result.stdout);
		let state = stdout.trim();
		if !result.status.success() || state.is_empty() {
			return Err(Error::value(
				"Cannot verify system-unit ownership; no files were changed",
			));
		}
		if state != "not-found" {
			return Err(Error::value(format!(
				"A system unit already uses {unit}; choose a different instance name"
			)));
		}
	}
	let unit_dir = config_root.join("systemd/user");
	for (unit, _) in &units {
		let path = unit_dir.join(unit);
		if path.exists() && !read_text(&path)?.starts_with(MARKER) {
			return Err(Error::value(format!(
				"Refusing to replace unmanaged unit {}",
				path.display()
			)));
		}
	}
	verify(&units, &env.temp_dir, system).await?;
	fs::create_dir_all(&config_root).map_err(at(&config_root))?;
	DirBuilder::new()
		.recursive(true)
		.mode(0o700)
		.create(&config_dir)
		.map_err(at(&config_dir))?;
	fs::set_permissions(&config_dir, Permissions::from_mode(0o700)).map_err(at(&config_dir))?;
	fs::create_dir_all(&unit_dir).map_err(at(&unit_dir))?;
	if config_path.exists() {
		write_private(&previous(&config_path), &read_text(&config_path)?)?;
	}
	write_private(&config_path, &config.to_json())?;
	for (unit, text) in &units {
		let path = unit_dir.join(unit);
		if path.exists() {
			let old = read_text(&path)?;
			if old != *text {
				write_private(&previous(&path), &old)?;
			}
		}
		write_private(&path, text)?;
	}
	system
		.check(&crate::argv!["systemctl", "--user", "daemon-reload"])
		.await?;
	let service = format!("devtunnel-{}.service", request.name);
	let timer = format!("devtunnel-{}-renew.timer", request.name);
	if request.start {
		system
			.check(&crate::argv![
				"systemctl",
				"--user",
				"enable",
				&service,
				&timer
			])
			.await?;
		system
			.check(&crate::argv![
				"systemctl",
				"--user",
				"restart",
				&service,
				&timer
			])
			.await?;
		writeln!(
			out,
			"Hosting requested. Inspect systemctl status/journal; host readiness is not yet certified."
		)?;
	} else {
		writeln!(
			out,
			"Installed only; no host was started or restarted. Existing hosts are left running."
		)?;
		writeln!(
			out,
			"To start: systemctl --user enable --now {service} {timer}"
		)?;
	}
	writeln!(out, "Config: {}", config_path.display())?;
	writeln!(out, "Entry point: {}", command.display())?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::cell::RefCell;
	use std::os::unix::process::ExitStatusExt;
	use std::process::ExitStatus;
	use std::sync::atomic::{AtomicUsize, Ordering};

	use super::*;

	/// Any persistent executable stands in for the installed command.
	const ENTRY: &str = "/bin/sh";

	const UNITS: [&str; 3] = [
		"devtunnel-example.service",
		"devtunnel-example-renew.service",
		"devtunnel-example-renew.timer",
	];

	// Python 3.14's render_units("example", Path('/srv/conf dir/50%/c"q.json'),
	// Path("/bin/sh")), less Environment=PYTHONDONTWRITEBYTECODE=1.
	const SERVICE: &str = "# Managed by devtunnel-service\n\
		[Unit]\n\
		Description=Persistent devtunnel host (example)\n\
		Wants=network-online.target\n\
		After=network-online.target\n\
		StartLimitIntervalSec=0\n\
		\n\
		[Service]\n\
		Type=simple\n\
		UMask=0077\n\
		NoNewPrivileges=true\n\
		PrivateTmp=true\n\
		ExecStart=\"/bin/sh\" host --config \"/srv/conf dir/50%%/c\\\"q.json\"\n\
		Restart=always\n\
		RestartSec=30\n\
		KillSignal=SIGINT\n\
		TimeoutStopSec=30\n\
		\n\
		[Install]\n\
		WantedBy=default.target\n";
	const RENEWAL: &str = "# Managed by devtunnel-service\n\
		[Unit]\n\
		Description=Renew devtunnel lease (example)\n\
		After=network-online.target\n\
		Wants=network-online.target\n\
		\n\
		[Service]\n\
		Type=oneshot\n\
		UMask=0077\n\
		NoNewPrivileges=true\n\
		PrivateTmp=true\n\
		ExecStart=\"/bin/sh\" renew --config \"/srv/conf dir/50%%/c\\\"q.json\"\n\
		TimeoutStartSec=180\n";
	const TIMER: &str = "# Managed by devtunnel-service\n\
		[Unit]\n\
		Description=Daily devtunnel lease renewal (example)\n\
		\n\
		[Timer]\n\
		OnCalendar=daily\n\
		RandomizedDelaySec=300\n\
		Persistent=true\n\
		Unit=devtunnel-example-renew.service\n\
		\n\
		[Install]\n\
		WantedBy=timers.target\n";

	// Python 3.14's stdout for `deploy --name example --tunnel-id example-api
	// --port 4000 --port 22 --binary /unused/devtunnel --dry-run --entry-point
	// /bin/sh` with XDG_CONFIG_HOME=/nonexistent/devtunnel-golden, less
	// Environment=PYTHONDONTWRITEBYTECODE=1.
	const DRY_RUN: &str = "# devtunnel-example.service\n\
		# Managed by devtunnel-service\n\
		[Unit]\n\
		Description=Persistent devtunnel host (example)\n\
		Wants=network-online.target\n\
		After=network-online.target\n\
		StartLimitIntervalSec=0\n\
		\n\
		[Service]\n\
		Type=simple\n\
		UMask=0077\n\
		NoNewPrivileges=true\n\
		PrivateTmp=true\n\
		ExecStart=\"/bin/sh\" host --config \"/nonexistent/devtunnel-golden/devtunnel-service/example.json\"\n\
		Restart=always\n\
		RestartSec=30\n\
		KillSignal=SIGINT\n\
		TimeoutStopSec=30\n\
		\n\
		[Install]\n\
		WantedBy=default.target\n\
		\n\
		# devtunnel-example-renew.service\n\
		# Managed by devtunnel-service\n\
		[Unit]\n\
		Description=Renew devtunnel lease (example)\n\
		After=network-online.target\n\
		Wants=network-online.target\n\
		\n\
		[Service]\n\
		Type=oneshot\n\
		UMask=0077\n\
		NoNewPrivileges=true\n\
		PrivateTmp=true\n\
		ExecStart=\"/bin/sh\" renew --config \"/nonexistent/devtunnel-golden/devtunnel-service/example.json\"\n\
		TimeoutStartSec=180\n\
		\n\
		# devtunnel-example-renew.timer\n\
		# Managed by devtunnel-service\n\
		[Unit]\n\
		Description=Daily devtunnel lease renewal (example)\n\
		\n\
		[Timer]\n\
		OnCalendar=daily\n\
		RandomizedDelaySec=300\n\
		Persistent=true\n\
		Unit=devtunnel-example-renew.service\n\
		\n\
		[Install]\n\
		WantedBy=timers.target\n\
		\n";

	// Python 3.14's stderr for that dry run with UV_CACHE_DIR=/nonexistent/uv-cache
	// and an entry point inside it.
	const NOTE: &str = "Note: deployment would refuse this entry point: \
		/nonexistent/uv-cache/archive-v0/abc/bin/devtunnel-service is inside the temporary \
		or cache directory /nonexistent/uv-cache; install devtunnel-service persistently (uv \
		tool install devtunnel-service, the Debian package or Homebrew) and run that command, \
		or pass --entry-point.\n";

	/// A canonical temporary directory, removed on drop.
	struct Scratch(PathBuf);

	impl Scratch {
		fn new() -> Self {
			static NEXT: AtomicUsize = AtomicUsize::new(0);
			let path = std::env::temp_dir().join(format!(
				"devtunnel-deploy-{}-{}",
				std::process::id(),
				NEXT.fetch_add(1, Ordering::Relaxed)
			));
			let _ = fs::remove_dir_all(&path);
			fs::create_dir_all(&path).unwrap();
			Self(fs::canonicalize(path).unwrap())
		}

		fn path(&self, relative: &str) -> PathBuf {
			self.0.join(relative)
		}

		fn make(&self, relative: &str, text: &str) -> PathBuf {
			let path = self.path(relative);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(&path, text).unwrap();
			path
		}

		fn executable(&self, relative: &str) -> PathBuf {
			let path = self.make(relative, "");
			fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
			path
		}

		fn link(&self, relative: &str, target: &Path) -> PathBuf {
			let path = self.path(relative);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::os::unix::fs::symlink(target, &path).unwrap();
			path
		}
	}

	impl Drop for Scratch {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn entries(dir: &Path) -> Vec<String> {
		let mut names: Vec<String> = fs::read_dir(dir)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect();
		names.sort();
		names
	}

	fn mode(path: &Path) -> u32 {
		fs::metadata(path).unwrap().permissions().mode() & 0o777
	}

	/// Records every command and answers with scripted results.
	struct Fake {
		linux: bool,
		/// Exit code and stdout of every `systemctl --system show`.
		show: (i32, &'static str),
		/// The program whose checked run fails.
		failing: Option<&'static str>,
		calls: RefCell<Vec<String>>,
		/// The unit files `systemd-analyze` was given, read while it ran.
		verified: RefCell<Vec<(PathBuf, String)>>,
	}

	impl Fake {
		fn new(code: i32, stdout: &'static str) -> Self {
			Self {
				linux: true,
				show: (code, stdout),
				failing: None,
				calls: RefCell::default(),
				verified: RefCell::default(),
			}
		}

		fn calls(&self) -> Vec<String> {
			self.calls.borrow().clone()
		}

		fn record(&self, argv: &[OsString]) {
			let words: Vec<&str> = argv.iter().map(|arg| arg.to_str().unwrap()).collect();
			self.calls.borrow_mut().push(words.join(" "));
		}
	}

	impl System for Fake {
		fn is_linux(&self) -> bool {
			self.linux
		}

		async fn capture(&self, argv: &[OsString], timeout: Duration) -> Result<Captured> {
			assert_eq!(timeout, Duration::from_secs(15));
			self.record(argv);
			Ok(Captured {
				status: ExitStatus::from_raw(self.show.0 << 8),
				stdout: self.show.1.into(),
			})
		}

		async fn check(&self, argv: &[OsString]) -> Result<()> {
			self.record(argv);
			if argv[0] == "systemd-analyze" {
				for path in &argv[3..] {
					let path = PathBuf::from(path);
					assert_eq!(mode(path.parent().unwrap()), 0o700);
					let text = fs::read_to_string(&path).unwrap();
					self.verified.borrow_mut().push((path, text));
				}
			}
			if self.failing == argv[0].to_str() {
				return Err(Error::called_process(argv, ExitStatus::from_raw(1 << 8)));
			}
			Ok(())
		}
	}

	fn show(unit: &str) -> String {
		format!("systemctl --system show {unit} -p LoadState --value")
	}

	/// A scratch tree with an executable `bin/devtunnel`, an empty config
	/// home and temporary directory, and a request for `example`.
	fn fixture() -> (Scratch, Environment, Request) {
		let scratch = Scratch::new();
		let devtunnel = scratch.executable("bin/devtunnel");
		for dir in ["config", "tmp", "home", "empty"] {
			fs::create_dir(scratch.path(dir)).unwrap();
		}
		let env = Environment {
			home: scratch.path("home"),
			config_home: Some(scratch.path("config")),
			cache_home: None,
			uv_cache_dir: None,
			path: Some(scratch.path("empty").into()),
			temp_dir: scratch.path("tmp"),
		};
		let request = Request {
			name: "example".into(),
			tunnel_id: "example-api".into(),
			ports: vec![4000],
			binary: Some(devtunnel.to_str().unwrap().into()),
			entry_point: Some(ENTRY.into()),
			dry_run: false,
			start: false,
			allow_anonymous: false,
		};
		(scratch, env, request)
	}

	async fn run(
		request: &Request,
		env: &Environment,
		system: &Fake,
	) -> (Result<()>, String, String) {
		let (mut out, mut err) = (Vec::new(), Vec::new());
		let result = deploy(request, env, system, &mut out, &mut err).await;
		(
			result,
			String::from_utf8(out).unwrap(),
			String::from_utf8(err).unwrap(),
		)
	}

	async fn failure(request: &Request, env: &Environment, system: &Fake) -> String {
		run(request, env, system).await.0.unwrap_err().to_string()
	}

	#[tokio::test]
	async fn does_not_shadow_system_units() {
		let (scratch, env, request) = fixture();
		let system = Fake::new(0, "loaded\n");
		assert_eq!(
			failure(&request, &env, &system).await,
			"ValueError: A system unit already uses devtunnel-example.service; \
			choose a different instance name"
		);
		assert_eq!(system.calls(), [show(UNITS[0])]);
		assert!(entries(&scratch.path("config")).is_empty());
	}

	#[tokio::test]
	async fn failed_system_inventory_does_not_write() {
		let (scratch, env, request) = fixture();
		for (code, stdout) in [(1, ""), (0, " \n"), (1, "not-found\n")] {
			let system = Fake::new(code, stdout);
			assert_eq!(
				failure(&request, &env, &system).await,
				"ValueError: Cannot verify system-unit ownership; no files were changed"
			);
			assert!(entries(&scratch.path("config")).is_empty());
			assert!(entries(&scratch.path("tmp")).is_empty());
		}
	}

	#[tokio::test]
	async fn does_not_overwrite_unmanaged_user_unit() {
		let (scratch, env, request) = fixture();
		let unit = scratch.make(
			"config/systemd/user/devtunnel-example-renew.timer",
			"unrelated user content",
		);
		let system = Fake::new(0, "not-found\n");
		assert_eq!(
			failure(&request, &env, &system).await,
			format!(
				"ValueError: Refusing to replace unmanaged unit {}",
				unit.display()
			)
		);
		assert_eq!(fs::read_to_string(&unit).unwrap(), "unrelated user content");
		assert!(!scratch.path("config/devtunnel-service").exists());
		assert!(system.verified.borrow().is_empty());
	}

	#[tokio::test]
	async fn installs_private_config_and_units() {
		let (scratch, env, request) = fixture();
		let system = Fake::new(0, "not-found\n");
		let (result, out, err) = run(&request, &env, &system).await;
		result.unwrap();
		let config_dir = scratch.path("config/devtunnel-service");
		let config_path = config_dir.join("example.json");
		assert_eq!(mode(&config_path), 0o600);
		assert_eq!(mode(&config_dir), 0o700);
		assert_eq!(entries(&config_dir), ["example.json"]);
		let config = Config::load(&config_path).unwrap();
		assert_eq!(
			config,
			Config {
				tunnel_id: "example-api".into(),
				binary: scratch.path("bin/devtunnel").to_str().unwrap().into(),
				ports: vec![4000],
				allow_anonymous: false,
			}
		);
		assert_eq!(fs::read_to_string(&config_path).unwrap(), config.to_json());

		let units = render_units("example", &config_path, Path::new(ENTRY)).unwrap();
		assert!(units[0].1.contains(&format!(
			"ExecStart=\"{ENTRY}\" host --config \"{}\"",
			config_path.display()
		)));
		let unit_dir = scratch.path("config/systemd/user");
		let mut names = UNITS.to_vec();
		names.sort();
		assert_eq!(entries(&unit_dir), names);
		for (unit, text) in &units {
			assert_eq!(&fs::read_to_string(unit_dir.join(unit)).unwrap(), text);
			assert_eq!(mode(&unit_dir.join(unit)), 0o600);
		}

		let verified = system.verified.borrow();
		let checked: Vec<(String, String)> = verified
			.iter()
			.map(|(path, text)| {
				let name = path.file_name().unwrap().to_str().unwrap();
				(name.into(), text.clone())
			})
			.collect();
		assert_eq!(checked, units);
		let folder = verified[0].0.parent().unwrap();
		assert_eq!(folder.parent().unwrap(), env.temp_dir);
		let name = folder.file_name().unwrap().to_str().unwrap();
		assert!(name.starts_with("devtunnel-units-"), "{name}");
		assert!(
			entries(&env.temp_dir).is_empty(),
			"verify directory removed"
		);
		let paths = UNITS.map(|unit| folder.join(unit).display().to_string());
		assert_eq!(
			system.calls(),
			[
				show(UNITS[0]),
				show(UNITS[1]),
				show(UNITS[2]),
				format!("systemd-analyze --user verify {}", paths.join(" ")),
				"systemctl --user daemon-reload".into(),
			]
		);
		assert_eq!(
			out,
			format!(
				"Installed only; no host was started or restarted. Existing hosts are left \
				running.\n\
				To start: systemctl --user enable --now devtunnel-example.service \
				devtunnel-example-renew.timer\n\
				Config: {}\n\
				Entry point: {ENTRY}\n",
				config_path.display()
			)
		);
		assert_eq!(err, "");
	}

	#[tokio::test]
	async fn config_backup_always_unit_backup_only_when_changed() {
		let (scratch, env, request) = fixture();
		let system = Fake::new(0, "not-found\n");
		run(&request, &env, &system).await.0.unwrap();
		let config_dir = scratch.path("config/devtunnel-service");
		let unit_dir = scratch.path("config/systemd/user");
		let first = fs::read_to_string(config_dir.join("example.json")).unwrap();

		run(&request, &env, &system).await.0.unwrap();
		assert_eq!(
			entries(&config_dir),
			["example.json", "example.json.previous"]
		);
		let backup = config_dir.join("example.json.previous");
		assert_eq!(fs::read_to_string(&backup).unwrap(), first);
		assert_eq!(mode(&backup), 0o600);
		assert_eq!(
			entries(&unit_dir).len(),
			3,
			"unchanged units are kept as is"
		);

		let timer = unit_dir.join(UNITS[2]);
		let edited = format!("{MARKER}[Timer]\nOnCalendar=hourly\n");
		fs::write(&timer, &edited).unwrap();
		run(&request, &env, &system).await.0.unwrap();
		let backup = format!("{}.previous", UNITS[2]);
		let mut names = UNITS.to_vec();
		names.push(&backup);
		names.sort();
		assert_eq!(entries(&unit_dir), names);
		assert_eq!(fs::read_to_string(unit_dir.join(&backup)).unwrap(), edited);
		assert_ne!(fs::read_to_string(&timer).unwrap(), edited);
	}

	#[tokio::test]
	async fn start_enables_then_restarts() {
		let (_scratch, env, request) = fixture();
		let request = Request {
			start: true,
			..request
		};
		let system = Fake::new(0, "not-found\n");
		let (result, out, _) = run(&request, &env, &system).await;
		result.unwrap();
		let pair = "devtunnel-example.service devtunnel-example-renew.timer";
		assert_eq!(
			system.calls()[4..],
			[
				"systemctl --user daemon-reload".to_owned(),
				format!("systemctl --user enable {pair}"),
				format!("systemctl --user restart {pair}"),
			]
		);
		assert!(out.starts_with(
			"Hosting requested. Inspect systemctl status/journal; host readiness is not yet \
			certified.\nConfig: "
		));
		assert!(!out.contains("Installed only"));
	}

	#[tokio::test]
	async fn failed_verification_removes_directory_and_writes_nothing() {
		let (scratch, env, request) = fixture();
		let system = Fake {
			failing: Some("systemd-analyze"),
			..Fake::new(0, "not-found\n")
		};
		let error = failure(&request, &env, &system).await;
		assert!(
			error.starts_with("CalledProcessError: Command '['systemd-analyze', "),
			"{error}"
		);
		assert_eq!(system.verified.borrow().len(), 3);
		assert!(entries(&scratch.path("tmp")).is_empty());
		assert!(entries(&scratch.path("config")).is_empty());
		assert!(
			!system
				.calls()
				.iter()
				.any(|call| call.contains("daemon-reload"))
		);
	}

	#[tokio::test]
	async fn refuses_without_linux_or_executables() {
		let (scratch, env, request) = fixture();
		let required = "ValueError: Linux/systemd and an executable devtunnel CLI are required";
		let system = Fake {
			linux: false,
			..Fake::new(0, "not-found\n")
		};
		assert_eq!(failure(&request, &env, &system).await, required);
		assert!(system.calls().is_empty());

		let system = Fake::new(0, "not-found\n");
		let plain = scratch.make("bin/plain", "");
		let missing_cli = Request {
			binary: Some(plain.to_str().unwrap().into()),
			..request.clone()
		};
		assert_eq!(failure(&missing_cli, &env, &system).await, required);
		let missing_entry = Request {
			entry_point: Some("/nonexistent/devtunnel-service".into()),
			..request
		};
		assert_eq!(
			failure(&missing_entry, &env, &system).await,
			format!(
				"ValueError: Entry point /nonexistent/devtunnel-service is not executable; {REMEDY}"
			)
		);
		assert!(system.calls().is_empty());
		assert!(entries(&scratch.path("config")).is_empty());
	}

	#[tokio::test]
	async fn missing_devtunnel_cli_is_explicit() {
		let (scratch, env, request) = fixture();
		let system = Fake::new(1, "");
		for binary in [None, Some(String::new())] {
			let request = Request {
				binary,
				dry_run: true,
				..request.clone()
			};
			assert_eq!(
				failure(&request, &env, &system).await,
				"ValueError: Install the official devtunnel CLI first, or pass --binary"
			);
		}
		// Like Python's `binary or which(...)`, an empty --binary searches PATH.
		scratch.executable("cli/devtunnel");
		let env = Environment {
			path: Some(scratch.path("cli").into()),
			..env
		};
		let request = Request {
			binary: Some(String::new()),
			dry_run: true,
			..request
		};
		run(&request, &env, &system).await.0.unwrap();
	}

	#[tokio::test]
	async fn ports_are_sorted_and_validated() {
		let (scratch, env, request) = fixture();
		let system = Fake::new(0, "not-found\n");
		let request = Request {
			ports: vec![5000, 22],
			..request
		};
		run(&request, &env, &system).await.0.unwrap();
		let config = Config::load(&scratch.path("config/devtunnel-service/example.json")).unwrap();
		assert_eq!(config.ports, [22, 5000]);
		for ports in [vec![], vec![0], vec![70000], vec![-1], vec![22, 22]] {
			let request = Request {
				ports,
				dry_run: true,
				..request.clone()
			};
			assert_eq!(
				failure(&request, &env, &system).await,
				format!("ValueError: {}", crate::config::INVALID_PORTS)
			);
		}
	}

	#[tokio::test]
	async fn allow_anonymous_warns_first_and_is_written() {
		let (scratch, env, request) = fixture();
		let warning = format!("Warning: {ANONYMOUS_RISK}\n");
		let system = Fake::new(0, "not-found\n");
		let request = Request {
			allow_anonymous: true,
			..request
		};
		let dry_run = Request {
			dry_run: true,
			entry_point: Some(scratch.path("tmp/bin/devtunnel-service")),
			..request.clone()
		};
		let (result, _, err) = run(&dry_run, &env, &system).await;
		result.unwrap();
		assert!(err.starts_with(&warning), "{err}");
		assert!(err[warning.len()..].starts_with("Note: deployment would refuse"));

		let (result, _, err) = run(&request, &env, &system).await;
		result.unwrap();
		assert_eq!(err, warning);
		let config = Config::load(&scratch.path("config/devtunnel-service/example.json")).unwrap();
		assert!(config.allow_anonymous);

		// The warning precedes even a validation failure.
		let invalid = Request {
			name: "SSH".into(),
			..dry_run
		};
		let (result, out, err) = run(&invalid, &env, &system).await;
		assert_eq!(result.unwrap_err().name(), "ValueError");
		assert_eq!((out.as_str(), err.as_str()), ("", warning.as_str()));
	}

	fn joined_units() -> String {
		let units =
			render_units("example", Path::new("/tmp/config.json"), Path::new(ENTRY)).unwrap();
		let texts: Vec<String> = units.into_iter().map(|(_, text)| text).collect();
		texts.join("\n")
	}

	#[test]
	fn units_match_python() {
		let units = render_units(
			"example",
			Path::new("/srv/conf dir/50%/c\"q.json"),
			Path::new(ENTRY),
		)
		.unwrap();
		let expected: Vec<(String, String)> = UNITS
			.into_iter()
			.zip([SERVICE, RENEWAL, TIMER])
			.map(|(unit, text)| (unit.into(), text.into()))
			.collect();
		assert_eq!(units, expected);
	}

	#[test]
	fn no_implicit_creation_or_anonymous() {
		let text = joined_units();
		for absent in ["--allow-anonymous", "RuntimeMaxSec", " create ", "PYTHON"] {
			assert!(!text.contains(absent), "{absent}");
		}
		for present in [
			"Restart=always",
			"StartLimitIntervalSec=0",
			"OnCalendar=daily",
		] {
			assert!(text.contains(present), "{present}");
		}
	}

	#[test]
	fn units_run_installed_command_without_checkout() {
		let text = joined_units();
		assert!(!text.contains("WorkingDirectory"));
		assert!(!text.contains(".py"));
		let units =
			render_units("example", Path::new("/tmp/config.json"), Path::new(ENTRY)).unwrap();
		for (index, action) in [(0, "host"), (1, "renew")] {
			let line = format!("ExecStart=\"{ENTRY}\" {action} --config \"/tmp/config.json\"");
			assert!(units[index].1.contains(&line), "{line}");
		}
	}

	#[test]
	fn units_do_not_configure_authentication() {
		let text = joined_units();
		for value in [
			"Managed Identity",
			"--mi-",
			"user login",
			"169.254.169.254",
			"NO_PROXY",
		] {
			assert!(!text.contains(value), "{value}");
		}
	}

	#[test]
	fn instance_name_validation() {
		let (too_long, longest) = ("x".repeat(49), "x".repeat(48));
		for name in ["../ssh", "SSH", "", "a\nb", "-a", "é", &too_long] {
			assert_eq!(
				render_units(name, Path::new("/config"), Path::new("/entry"))
					.unwrap_err()
					.to_string(),
				"ValueError: Instance name must contain lowercase letters, digits and hyphens",
				"{name:?}"
			);
		}
		for name in ["a", "0-a-", &longest] {
			assert!(render_units(name, Path::new("/config"), Path::new("/entry")).is_ok());
		}
	}

	#[test]
	fn quote_systemd_specifiers_and_spaces() {
		assert_eq!(quote("/a b/50%/\"c\"").unwrap(), "\"/a b/50%%/\\\"c\\\"\"");
		assert_eq!(quote("/a\\b").unwrap(), "\"/a\\\\b\"");
		assert_eq!(quote("/a\u{7f}b").unwrap(), "\"/a\u{7f}b\"");
		for value in ["/a\nb", "/a\tb", "/a\u{1f}b"] {
			assert_eq!(
				quote(value).unwrap_err().to_string(),
				"ValueError: Control characters are not supported in service paths"
			);
		}
		let error = render_units("example", Path::new("/c\n.json"), Path::new(ENTRY)).unwrap_err();
		assert_eq!(error.name(), "ValueError");
	}

	#[test]
	fn private_atomic_write() {
		let scratch = Scratch::new();
		let path = scratch.path("config.json");
		write_private(&path, "first").unwrap();
		write_private(&path, "second").unwrap();
		assert_eq!(fs::read_to_string(&path).unwrap(), "second");
		assert_eq!(mode(&path), 0o600);
		assert_eq!(entries(&scratch.0), ["config.json"]);
	}

	#[test]
	fn private_write_leaves_no_temporary_file_on_failure() {
		let scratch = Scratch::new();
		let occupied = scratch.make("occupied/file", "");
		let error = write_private(occupied.parent().unwrap(), "text").unwrap_err();
		assert_eq!(error.name(), "IsADirectoryError", "{error}");
		assert_eq!(entries(&scratch.0), ["occupied"]);
		let error = write_private(&scratch.path("missing/config.json"), "text").unwrap_err();
		assert_eq!(error.name(), "FileNotFoundError", "{error}");
	}

	#[tokio::test]
	async fn dry_run_has_no_writes_or_subprocesses() {
		let (scratch, env, request) = fixture();
		let env = Environment {
			config_home: Some(scratch.path("config/missing")),
			..env
		};
		let request = Request {
			dry_run: true,
			..request
		};
		let system = Fake::new(1, "");
		let (result, out, err) = run(&request, &env, &system).await;
		result.unwrap();
		assert!(out.contains(&format!("ExecStart=\"{ENTRY}\" host")));
		assert_eq!(err, "");
		assert!(system.calls().is_empty());
		assert!(entries(&scratch.path("config")).is_empty());
		assert!(entries(&scratch.path("tmp")).is_empty());
	}

	#[tokio::test]
	async fn dry_run_matches_python() {
		let env = Environment {
			home: "/nonexistent/home".into(),
			config_home: Some("/nonexistent/devtunnel-golden".into()),
			cache_home: None,
			uv_cache_dir: Some("/nonexistent/uv-cache".into()),
			path: None,
			temp_dir: "/nonexistent/tmp".into(),
		};
		let request = Request {
			name: "example".into(),
			tunnel_id: "example-api".into(),
			ports: vec![4000, 22],
			binary: Some("/unused/devtunnel".into()),
			entry_point: Some(ENTRY.into()),
			dry_run: true,
			start: false,
			allow_anonymous: false,
		};
		let system = Fake::new(1, "");
		let (result, out, err) = run(&request, &env, &system).await;
		result.unwrap();
		assert_eq!((out.as_str(), err.as_str()), (DRY_RUN, ""));

		let entry = "/nonexistent/uv-cache/archive-v0/abc/bin/devtunnel-service";
		let request = Request {
			ports: vec![4000],
			entry_point: Some(entry.into()),
			..request
		};
		let (result, _, err) = run(&request, &env, &system).await;
		result.unwrap();
		assert_eq!(err, NOTE);
		assert!(system.calls().is_empty());
	}

	#[tokio::test]
	async fn entry_point_defaults_to_command_on_path() {
		let (scratch, env, request) = fixture();
		let found = scratch.executable("opt/tool/bin/devtunnel-service");
		let env = Environment {
			path: Some(scratch.path("opt/tool/bin").into()),
			..env
		};
		let request = Request {
			entry_point: None,
			dry_run: true,
			..request
		};
		let (result, out, _) = run(&request, &env, &Fake::new(1, "")).await;
		result.unwrap();
		assert!(out.contains(&format!("ExecStart=\"{}\" host", found.display())));
	}

	#[tokio::test]
	async fn missing_entry_point_is_explicit() {
		let (_scratch, env, request) = fixture();
		let request = Request {
			entry_point: None,
			dry_run: true,
			..request
		};
		assert_eq!(
			failure(&request, &env, &Fake::new(1, "")).await,
			format!("ValueError: No devtunnel-service command on PATH; {REMEDY}")
		);
	}

	#[tokio::test]
	async fn uvx_cache_entry_is_refused() {
		let (scratch, env, request) = fixture();
		let cache = scratch.path("uv");
		fs::create_dir(&cache).unwrap();
		let env = Environment {
			uv_cache_dir: Some(cache.clone()),
			..env
		};
		let entry = cache.join("archive-v0/abc/bin/devtunnel-service");
		let request = Request {
			entry_point: Some(entry.clone()),
			..request
		};
		let dry_run = Request {
			dry_run: true,
			..request.clone()
		};
		let system = Fake::new(0, "not-found\n");
		let (result, out, err) = run(&dry_run, &env, &system).await;
		result.unwrap();
		assert!(err.contains("would refuse"), "{err}");
		assert!(err.contains("uv tool install devtunnel-service"), "{err}");
		assert!(out.contains(entry.to_str().unwrap()));
		let error = failure(&request, &env, &system).await;
		assert!(
			error.starts_with("ValueError: Refusing a non-persistent entry point: "),
			"{error}"
		);
		assert!(error.ends_with(REMEDY), "{error}");
		assert!(system.calls().is_empty());
		assert!(entries(&scratch.path("config")).is_empty());
	}

	fn environment(home: &Path) -> Environment {
		Environment {
			home: home.into(),
			config_home: None,
			cache_home: None,
			uv_cache_dir: None,
			path: None,
			temp_dir: std::env::temp_dir(),
		}
	}

	#[test]
	fn temporary_directory_is_transient() {
		let scratch = Scratch::new();
		let env = environment(&scratch.path("home"));
		let roots = transient_roots(&env);
		assert!(roots.contains(&resolve(&std::env::temp_dir()).unwrap()));
		assert!(roots.contains(&resolve(Path::new("/tmp")).unwrap()));
		assert!(roots.contains(&resolve(Path::new("/var/tmp")).unwrap()));
		assert!(roots.contains(&scratch.path("home/.cache")));
		assert!(persistence_problem(&scratch.path("bin/tool"), &roots, &env.home).is_some());
	}

	#[test]
	fn cache_directories_are_transient() {
		let env = Environment {
			cache_home: Some("/srv/cache".into()),
			uv_cache_dir: Some("~/uv".into()),
			..environment(Path::new("/srv/home"))
		};
		let roots = transient_roots(&env);
		assert_eq!(roots.len(), 5);
		assert!(roots.contains(&PathBuf::from("/srv/cache")));
		assert!(roots.contains(&PathBuf::from("/srv/home/uv")));
		assert!(!roots.contains(&PathBuf::from("/srv/home/.cache")));
	}

	/// Layout of `uv tool install`: uv tags the venv itself with CACHEDIR.TAG.
	const TOOL: &str = "share/tools/devtunnel-service";
	const RECORD: &str = "lib/python3.12/site-packages/\
		devtunnel_service-0.1.0.dist-info/direct_url.json";

	fn tool_environment(scratch: &Scratch, direct_url: &str) -> PathBuf {
		scratch.make(&format!("{TOOL}/pyvenv.cfg"), "");
		scratch.make(&format!("{TOOL}/CACHEDIR.TAG"), "");
		scratch.make(&format!("{TOOL}/{RECORD}"), direct_url);
		let target = scratch.make(&format!("{TOOL}/bin/devtunnel-service"), "");
		scratch.link("bin/devtunnel-service", &target)
	}

	/// The problem with `entry` without transient roots, as Python's tests
	/// patch them away; "" if none.
	fn problem(scratch: &Scratch, entry: &Path) -> String {
		persistence_problem(entry, &[], &scratch.path("home")).unwrap_or_default()
	}

	#[test]
	fn tool_environment_link_is_persistent() {
		let scratch = Scratch::new();
		let link = tool_environment(&scratch, r#"{"url": "file:///w.whl", "archive_info": {}}"#);
		assert_eq!(problem(&scratch, &link), "");
		fs::write(
			scratch.path(&format!("{TOOL}/{RECORD}")),
			r#"{"url": "file:///src", "dir_info": {"editable": false}}"#,
		)
		.unwrap();
		assert_eq!(problem(&scratch, &link), "");
	}

	#[test]
	fn editable_install_is_refused() {
		let scratch = Scratch::new();
		let link = tool_environment(
			&scratch,
			r#"{"url": "file:///src/devtunnel-service", "dir_info": {"editable": true}}"#,
		);
		assert_eq!(
			problem(&scratch, &link),
			format!(
				"{}: it is an editable install that imports from a source tree ({})",
				scratch
					.path(&format!("{TOOL}/bin/devtunnel-service"))
					.display(),
				scratch.path(&format!("{TOOL}/{RECORD}")).display()
			)
		);
	}

	#[test]
	fn malformed_install_record_is_refused() {
		let scratch = Scratch::new();
		let link = tool_environment(&scratch, "[");
		let problem = problem(&scratch, &link);
		assert!(
			problem.ends_with("direct_url.json is unreadable"),
			"{problem}"
		);
	}

	#[test]
	fn checkout_environment_is_refused() {
		let scratch = Scratch::new();
		for marker in ["pyproject.toml", "Cargo.toml"] {
			let checkout = format!("checkout-{marker}");
			scratch.make(&format!("{checkout}/{marker}"), "");
			for env in [".venv", ".tox/py310"] {
				let env = format!("{checkout}/{env}");
				scratch.make(&format!("{env}/pyvenv.cfg"), "");
				let entry = scratch.make(&format!("{env}/bin/devtunnel-service"), "");
				assert_eq!(
					problem(&scratch, &entry),
					format!(
						"{}: its environment {} is inside the source tree {}",
						entry.display(),
						scratch.path(&env).display(),
						scratch.path(&checkout).display()
					)
				);
			}
		}
	}

	#[test]
	fn relocated_uv_cache_is_refused() {
		// uvx's environment in a cache moved by uv.toml or --cache-dir, which
		// UV_CACHE_DIR and the XDG cache directory do not reveal.
		let scratch = Scratch::new();
		scratch.make("srv/uv-cache/CACHEDIR.TAG", "");
		scratch.make("srv/uv-cache/archive-v0/h/pyvenv.cfg", "");
		scratch.make("srv/uv-cache/archive-v0/h/CACHEDIR.TAG", "");
		let entry = scratch.make("srv/uv-cache/archive-v0/h/bin/devtunnel-service", "");
		assert_eq!(
			problem(&scratch, &entry),
			format!(
				"{}: its environment {} is inside the cache {}",
				entry.display(),
				scratch.path("srv/uv-cache/archive-v0/h").display(),
				scratch.path("srv/uv-cache").display()
			)
		);
	}

	#[test]
	fn link_into_cache_is_refused() {
		let scratch = Scratch::new();
		let target = scratch.make("cache/archive-v0/x/bin/devtunnel-service", "");
		let link = scratch.link("bin/devtunnel-service", &target);
		let roots = [scratch.path("cache")];
		assert_eq!(
			persistence_problem(&link, &roots, &scratch.path("home")),
			Some(format!(
				"{} is inside the temporary or cache directory {}",
				target.display(),
				roots[0].display()
			))
		);
	}

	#[test]
	fn cargo_target_binary_is_refused() {
		let scratch = Scratch::new();
		scratch.make("repo/Cargo.toml", "");
		scratch.make("repo/target/CACHEDIR.TAG", "");
		let built = scratch.make("repo/target/debug/devtunnel-service", "");
		assert_eq!(
			problem(&scratch, &built),
			format!(
				"{} is inside the cache {}",
				built.display(),
				scratch.path("repo/target").display()
			)
		);
		scratch.make("tree/Cargo.toml", "");
		let built = scratch.make("tree/build/devtunnel-service", "");
		assert_eq!(
			problem(&scratch, &built),
			format!(
				"{} is inside the source tree {}",
				built.display(),
				scratch.path("tree").display()
			)
		);
	}

	#[test]
	fn bare_binary_under_home_with_stray_manifests_is_persistent() {
		let scratch = Scratch::new();
		scratch.make("home/Cargo.toml", "");
		scratch.make("home/pyproject.toml", "");
		for entry in [
			".cargo/bin/devtunnel-service",
			".local/bin/devtunnel-service",
		] {
			let entry = scratch.make(&format!("home/{entry}"), "");
			assert_eq!(problem(&scratch, &entry), "");
		}
		assert_eq!(problem(&scratch, Path::new(ENTRY)), "");
	}

	#[test]
	fn entry_point_keeps_stable_link() {
		let scratch = Scratch::new();
		let env = environment(&scratch.path("home"));
		let link = scratch.path("bin/devtunnel-service");
		assert_eq!(entry_point(Some(&link), &env).unwrap(), link);
		assert_eq!(
			entry_point(Some(Path::new("~/bin/x")), &env).unwrap(),
			scratch.path("home/bin/x")
		);
	}

	#[test]
	fn systemd_accepts_rendered_units() {
		let search_path = std::env::var_os("PATH");
		let Some(analyze) = which("systemd-analyze", search_path.as_deref()) else {
			eprintln!("skipped: requires systemd-analyze");
			return;
		};
		let scratch = Scratch::new();
		let units = render_units("example", &scratch.path("c.json"), Path::new(ENTRY)).unwrap();
		let mut command = std::process::Command::new(analyze);
		command.args(["--user", "verify"]);
		for (unit, text) in &units {
			command.arg(scratch.make(unit, text));
		}
		let output = command.output().unwrap();
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(output.status.success(), "{stderr}");
		assert!(!stderr.contains("not absolute"), "{stderr}");
	}
}
