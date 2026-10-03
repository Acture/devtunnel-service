//! Command-line entry point: `devtunnel-service deploy|host|renew|doctor`.

// SCAFFOLD: removed once every module is implemented (stubs leave items unused).
#![allow(dead_code)]

mod config;
mod credentials;
mod deploy;
mod error;
mod host;
mod log;
mod paths;
mod process;
mod remote;
mod service;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::error::Result;
use crate::host::Action;

pub(crate) const PROGRAM: &str = "devtunnel-service";
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(
	name = PROGRAM,
	version,
	about = "Run an existing persistent Microsoft Dev Tunnel as a Linux systemd user \
		service, using the devtunnel CLI's existing login."
)]
struct Cli {
	#[command(subcommand)]
	command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
	/// install user units for an existing tunnel
	#[command(long_about = "Install a user-level devtunnel host and daily lease-renewal timer.")]
	Deploy(DeployArgs),
	/// validate the tunnel, then host it in the foreground
	Host(ConfigArgs),
	/// validate the tunnel, then extend its lease to 30 days
	Renew(ConfigArgs),
	/// check configuration, remote ports and access rules (read-only)
	Doctor(ConfigArgs),
}

#[derive(Debug, Args)]
struct DeployArgs {
	/// instance name: lowercase letters, digits, -
	#[arg(long)]
	name: String,
	/// existing persistent tunnel; never created implicitly
	#[arg(long)]
	tunnel_id: String,
	/// forwarded port; repeat until the set matches the remote tunnel
	#[arg(long = "port", required = true, allow_negative_numbers = true)]
	ports: Vec<i64>,
	/// path of the devtunnel CLI (default: devtunnel on PATH)
	#[arg(long)]
	binary: Option<String>,
	/// persistent devtunnel-service command for the units (default: devtunnel-service on PATH)
	#[arg(long)]
	entry_point: Option<PathBuf>,
	/// print the units; no writes, network calls or service changes
	#[arg(long)]
	dry_run: bool,
	/// explicitly enable and (re)start hosting after installation
	#[arg(long)]
	start: bool,
	/// accept anonymous access rules on the tunnel: anyone with its URL can
	/// then reach the forwarded services
	#[arg(long)]
	allow_anonymous: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
	/// instance configuration written by deploy
	#[arg(long)]
	config: PathBuf,
}

async fn run(command: Command) -> Result<()> {
	let (action, args) = match command {
		Command::Deploy(args) => {
			let request = deploy::Request {
				name: args.name,
				tunnel_id: args.tunnel_id,
				ports: args.ports,
				binary: args.binary,
				entry_point: args.entry_point,
				dry_run: args.dry_run,
				start: args.start,
				allow_anonymous: args.allow_anonymous,
			};
			let env = deploy::Environment::from_process()?;
			return deploy::deploy(
				&request,
				&env,
				&deploy::Machine,
				&mut std::io::stdout(),
				&mut std::io::stderr(),
			)
			.await;
		}
		Command::Host(args) => (Action::Host, args),
		Command::Renew(args) => (Action::Renew, args),
		Command::Doctor(args) => (Action::Doctor, args),
	};
	host::run(action, &args.config).await
}

fn main() -> ExitCode {
	// Help, version and usage errors exit here, before any system access.
	let cli = Cli::parse();
	let outcome = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.map_err(error::Error::from)
		.and_then(|runtime| runtime.block_on(run(cli.command)));
	match outcome {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			// Console boundary: one line for operational failures.
			eprintln!("{error}");
			ExitCode::FAILURE
		}
	}
}
