//! Black-box checks of `devtunnel-service deploy` through the built binary,
//! with a controlled environment and nothing written outside a scratch tree.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BINARY: &str = env!("CARGO_BIN_EXE_devtunnel-service");

/// A canonical temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
	fn new() -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let path = std::env::temp_dir().join(format!(
			"devtunnel-deploy-cli-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, Ordering::Relaxed)
		));
		let _ = fs::remove_dir_all(&path);
		fs::create_dir_all(path.join("home")).unwrap();
		Self(fs::canonicalize(path).unwrap())
	}

	fn path(&self, relative: &str) -> PathBuf {
		self.0.join(relative)
	}

	/// Runs `deploy --tunnel-id example-api --binary /unused/devtunnel ARGS`
	/// with only `HOME`, `PATH`, `XDG_CONFIG_HOME` and `UV_CACHE_DIR` set.
	fn deploy(&self, args: &[&str]) -> Output {
		let output = Command::new(BINARY)
			.args([
				"deploy",
				"--tunnel-id",
				"example-api",
				"--binary",
				"/unused/devtunnel",
			])
			.args(args)
			.env_clear()
			.env("HOME", self.path("home"))
			.env("PATH", "/usr/bin:/bin")
			.env("XDG_CONFIG_HOME", self.path("config"))
			.env("UV_CACHE_DIR", self.path("uv"))
			.output()
			.unwrap();
		assert!(!self.path("config").exists(), "nothing is created");
		output
	}
}

impl Drop for Scratch {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

fn text(bytes: &[u8]) -> &str {
	std::str::from_utf8(bytes).unwrap()
}

/// The dry-run output for instance `example` with configuration `config`.
fn units(config: &str) -> String {
	format!(
		"# devtunnel-example.service\n\
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
		ExecStart=\"/bin/sh\" host --config \"{config}\"\n\
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
		ExecStart=\"/bin/sh\" renew --config \"{config}\"\n\
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
		\n"
	)
}

const DRY_RUN: [&str; 7] = [
	"--name",
	"example",
	"--port",
	"4000",
	"--entry-point",
	"/bin/sh",
	"--dry-run",
];

#[test]
fn dry_run_prints_units_only() {
	let scratch = Scratch::new();
	let output = scratch.deploy(&DRY_RUN);
	let config = scratch.path("config/devtunnel-service/example.json");
	assert_eq!(text(&output.stderr), "");
	assert_eq!(text(&output.stdout), units(config.to_str().unwrap()));
	assert_eq!(output.status.code(), Some(0));
}

#[test]
fn uv_cache_entry_point_is_noted_then_refused() {
	let scratch = Scratch::new();
	let entry = scratch.path("uv/archive-v0/abc/bin/devtunnel-service");
	let args = ["--name", "example", "--port", "4000", "--entry-point"];
	let mut dry_run = args.to_vec();
	dry_run.extend([entry.to_str().unwrap(), "--dry-run"]);
	let output = scratch.deploy(&dry_run);
	let stderr = text(&output.stderr);
	assert!(
		stderr.starts_with("Note: deployment would refuse this entry point: "),
		"{stderr}"
	);
	assert!(
		stderr.contains("uv tool install devtunnel-service"),
		"{stderr}"
	);
	assert_eq!(output.status.code(), Some(0));

	let mut install = args.to_vec();
	install.push(entry.to_str().unwrap());
	let output = scratch.deploy(&install);
	let stderr = text(&output.stderr);
	assert!(
		stderr.starts_with("ValueError: Refusing a non-persistent entry point"),
		"{stderr}"
	);
	assert_eq!(stderr.lines().count(), 1, "{stderr}");
	assert_eq!(text(&output.stdout), "");
	assert_eq!(output.status.code(), Some(1));
}

#[test]
fn invalid_ports_are_value_errors() {
	let scratch = Scratch::new();
	for port in ["0", "70000"] {
		let mut args = DRY_RUN.to_vec();
		args[3] = port;
		let output = scratch.deploy(&args);
		assert_eq!(
			text(&output.stderr),
			"ValueError: Expected ports must be a nonempty list of unique port numbers\n"
		);
		assert_eq!(text(&output.stdout), "");
		assert_eq!(output.status.code(), Some(1));
	}
}

#[test]
fn invalid_instance_name_is_a_value_error() {
	let scratch = Scratch::new();
	let mut args = DRY_RUN.to_vec();
	args[1] = "SSH";
	let output = scratch.deploy(&args);
	assert_eq!(
		text(&output.stderr),
		"ValueError: Instance name must contain lowercase letters, digits and hyphens\n"
	);
	assert_eq!(output.status.code(), Some(1));
}

#[test]
fn allow_anonymous_dry_run_warns() {
	let scratch = Scratch::new();
	let mut args = DRY_RUN.to_vec();
	args.push("--allow-anonymous");
	let output = scratch.deploy(&args);
	assert_eq!(
		text(&output.stderr),
		"Warning: allow_anonymous is enabled: anonymous access rules are accepted, and \
		anyone who has the tunnel URL can then reach the forwarded services\n"
	);
	let config = scratch.path("config/devtunnel-service/example.json");
	assert_eq!(text(&output.stdout), units(config.to_str().unwrap()));
	assert_eq!(output.status.code(), Some(0));
}
