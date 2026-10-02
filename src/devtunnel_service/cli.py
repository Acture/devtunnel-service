"""Command-line entry point: ``devtunnel-service deploy|host|renew|doctor``."""

import argparse
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

from devtunnel_service import __version__, deploy, host

ACTIONS: dict[host.Action, str] = {
    "host": "validate the tunnel, then run the devtunnel host in the foreground",
    "renew": "validate the tunnel, then extend its lease to 30 days",
    "doctor": "check configuration, remote ports and access rules (read-only)",
}


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog=deploy.PROGRAM,
        description="Run an existing persistent Microsoft Dev Tunnel as a Linux "
        "systemd user service, using the devtunnel CLI's existing login.",
    )
    parser.add_argument(
        "--version", action="version", version=f"%(prog)s {__version__}"
    )
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")
    install = commands.add_parser(
        "deploy",
        help="install user units for an existing tunnel",
        description=deploy.__doc__,
    )
    install.add_argument(
        "--name", required=True, help="instance name: lowercase letters, digits, -"
    )
    install.add_argument(
        "--tunnel-id",
        required=True,
        help="existing persistent tunnel; never created implicitly",
    )
    install.add_argument(
        "--port",
        type=int,
        action="append",
        required=True,
        help="forwarded port; repeat until the set matches the remote tunnel",
    )
    install.add_argument(
        "--binary", help="path of the devtunnel CLI (default: devtunnel on PATH)"
    )
    install.add_argument(
        "--entry-point",
        type=Path,
        help=f"persistent {deploy.PROGRAM} command for the units "
        f"(default: {deploy.PROGRAM} on PATH)",
    )
    install.add_argument(
        "--dry-run",
        action="store_true",
        help="print the units; no writes, network calls or service changes",
    )
    install.add_argument(
        "--start",
        action="store_true",
        help="explicitly enable and (re)start hosting after installation",
    )
    for action, text in ACTIONS.items():
        command = commands.add_parser(action, help=text, description=text)
        command.add_argument(
            "--config",
            required=True,
            type=Path,
            help="instance configuration written by deploy",
        )
    return parser


def run(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.command == "deploy":
        return deploy.deploy(
            args.name,
            args.tunnel_id,
            args.port,
            args.binary,
            args.entry_point,
            dry_run=args.dry_run,
            start=args.start,
        )
    return host.run(args.command, args.config)


def main() -> int:
    try:
        return run()
    except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as error:
        # Console boundary: one line for operational failures; bugs keep tracebacks.
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        return 1
