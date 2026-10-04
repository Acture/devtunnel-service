//! Command-line entry point: `devtunnel-service deploy|host|renew|doctor`.

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

/// The grammar follows Python's argparse: unique prefixes of long options,
/// the last of repeated options wins, values may look like negative
/// numbers, and there is neither `-V` nor a `help` subcommand.
#[derive(Debug, Parser)]
#[command(
	name = PROGRAM,
	version,
	about = "Run an existing persistent Microsoft Dev Tunnel as a Linux systemd user \
		service, using the devtunnel CLI's existing login.",
	infer_long_args = true,
	args_override_self = true,
	disable_help_subcommand = true,
	disable_version_flag = true
)]
struct Cli {
	/// show program's version number and exit
	#[arg(long, action = clap::ArgAction::Version)]
	version: Option<bool>,
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
	#[arg(long, allow_negative_numbers = true)]
	name: String,
	/// existing persistent tunnel; never created implicitly
	#[arg(long, allow_negative_numbers = true)]
	tunnel_id: String,
	/// forwarded port; repeat until the set matches the remote tunnel
	#[arg(long = "port", required = true, allow_negative_numbers = true, value_parser = port)]
	ports: Vec<i64>,
	/// path of the devtunnel CLI (default: devtunnel on PATH)
	#[arg(long, allow_negative_numbers = true)]
	binary: Option<String>,
	/// persistent devtunnel-service command for the units (default: devtunnel-service on PATH)
	#[arg(long, allow_negative_numbers = true)]
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
	#[arg(long, allow_negative_numbers = true)]
	config: PathBuf,
}

/// An integer as Python's `int()` reads it: surrounding whitespace, a sign,
/// and underscores between digits. Values beyond `i64` saturate, so that
/// configuration validation rejects them as ports.
fn port(value: &str) -> std::result::Result<i64, String> {
	let invalid = || format!("invalid int value: '{value}'");
	let text = value.trim();
	let (negative, digits) = match text.as_bytes().first() {
		Some(b'-') => (true, &text[1..]),
		Some(b'+') => (false, &text[1..]),
		_ => (false, text),
	};
	let well_formed = !digits.is_empty()
		&& !digits.starts_with('_')
		&& !digits.ends_with('_')
		&& !digits.contains("__")
		&& digits
			.bytes()
			.all(|byte| byte.is_ascii_digit() || byte == b'_');
	if !well_formed {
		return Err(invalid());
	}
	let magnitude = digits
		.bytes()
		.filter(u8::is_ascii_digit)
		.fold(0i64, |number, digit| {
			number
				.saturating_mul(10)
				.saturating_add(i64::from(digit - b'0'))
		});
	Ok(if negative { -magnitude } else { magnitude })
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
	let runtime = match tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
	{
		Ok(runtime) => runtime,
		Err(error) => {
			log::failure(error::Error::from(error));
			return ExitCode::FAILURE;
		}
	};
	let outcome = runtime.block_on(run(cli.command));
	// Blocking DNS lookups can outlive the work; exit without waiting for them.
	runtime.shutdown_background();
	match outcome {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			// Console boundary: one line for operational failures.
			log::failure(&error);
			ExitCode::FAILURE
		}
	}
}

#[cfg(test)]
mod tests {
	use clap::Parser;

	use super::*;

	fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
		Cli::try_parse_from(std::iter::once(PROGRAM).chain(args.iter().copied()))
	}

	fn deploy_args(args: &[&str]) -> DeployArgs {
		match parse(args).unwrap().command {
			Command::Deploy(args) => args,
			command => panic!("unexpected {command:?}"),
		}
	}

	#[test]
	fn grammar_matches_argparse() {
		let args = deploy_args(&[
			"deploy", "--dry", "--tunnel", "t", "--name", "a", "--name", "b", "--port", "1",
			"--entry", "/opt/x", "--binary", "-1",
		]);
		assert!(args.dry_run);
		assert_eq!((args.name.as_str(), args.tunnel_id.as_str()), ("b", "t"));
		assert_eq!(args.binary.as_deref(), Some("-1"));
		assert!(matches!(
			parse(&["doctor", "--conf", "/c"]).unwrap().command,
			Command::Doctor(_)
		));
		for args in [&["-V"][..], &["help"], &["help", "deploy"]] {
			assert_ne!(parse(args).unwrap_err().exit_code(), 0, "{args:?}");
		}
		assert_eq!(
			parse(&["--version"]).unwrap_err().kind(),
			clap::error::ErrorKind::DisplayVersion
		);
	}

	#[test]
	fn ports_read_like_python_int() {
		for (value, port) in [
			(" 4000\n", 4000),
			("+4_000", 4000),
			("-1", -1),
			("0", 0),
			("99999999999999999999999", i64::MAX),
		] {
			assert_eq!(super::port(value), Ok(port), "{value:?}");
		}
		for value in ["", "abc", "4__0", "_4", "4_", "4.0", "0x10", "--1"] {
			assert!(super::port(value).is_err(), "{value:?}");
		}
	}
}
