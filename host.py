"""Persistent devtunnel hosting using the CLI's existing authentication context."""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path


def validate_config(config: dict) -> dict:
    if not isinstance(config, dict):
        raise ValueError("Configuration must be a JSON object")
    tunnel_id = config.get("tunnel_id")
    if not isinstance(tunnel_id, str) or not re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9._-]*", tunnel_id
    ):
        raise ValueError("A valid explicit tunnel ID is required")
    binary = Path(config["binary"])
    if not binary.is_absolute():
        raise ValueError("devtunnel binary path must be absolute")
    ports = config.get("ports")
    if (
        not isinstance(ports, list)
        or not ports
        or any(type(port) is not int or not 1 <= port <= 65535 for port in ports)
        or len(set(ports)) != len(ports)
    ):
        raise ValueError(
            "Expected ports must be a nonempty list of unique port numbers"
        )
    if config.get("allow_anonymous") is not False:
        raise ValueError("This service requires allow_anonymous=false")
    return config


def run_cli(args: list[str], *, as_json: bool = False):
    result = subprocess.run(args, capture_output=True, text=True, timeout=45)
    if result.returncode:
        # Never forward arbitrary CLI output into service logs.
        raise RuntimeError(
            f"devtunnel {args[1]} failed with exit {result.returncode}; "
            "check the CLI login and tunnel permissions under the service's Unix user"
        )
    return json.loads(result.stdout) if as_json else None


def require_private_access(access) -> None:
    if isinstance(access, dict):
        entries = access.get("entries")
    elif isinstance(access, list):
        entries = access
    else:
        raise ValueError(
            "Missing or unrecognized access-control evidence; refusing to host"
        )
    if not isinstance(entries, list):
        raise ValueError("Malformed access-control entries")
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("type"), str):
            raise ValueError("Malformed access-control entry")
        if entry["type"].lower() == "anonymous" and entry.get("isDeny") is not True:
            raise ValueError("Anonymous tunnel access is enabled; refusing to host")


def validate_remote(tunnel: dict, expected_ports: list[int]) -> None:
    if not isinstance(tunnel, dict):
        raise ValueError("Missing tunnel object")
    require_private_access(tunnel.get("accessControl"))
    ports = tunnel.get("ports")
    if not isinstance(ports, list):
        raise ValueError("Missing remote port inventory")
    if any(
        not isinstance(entry, dict) or type(entry.get("portNumber")) is not int
        for entry in ports
    ):
        raise ValueError("Malformed remote port inventory")
    actual = {entry["portNumber"] for entry in ports}
    if actual != set(expected_ports) or len(actual) != len(ports):
        raise ValueError(
            "Remote ports differ from explicitly configured ports; refusing to host extra ports"
        )
    # Port-level rules may override tunnel-level rules. Query each port in doctor.
    for entry in ports:
        if "accessControl" in entry:
            require_private_access(entry["accessControl"])


def inspect_remote(config: dict) -> None:
    binary, tunnel_id = config["binary"], config["tunnel_id"]
    data = run_cli([binary, "show", tunnel_id, "--json"], as_json=True)
    validate_remote(data["tunnel"], config["ports"])
    for port in config["ports"]:
        # Access list includes effective tunnel/port ACL evidence.
        data = run_cli(
            [binary, "access", "list", tunnel_id, "--port-number", str(port), "--json"],
            as_json=True,
        )
        entries = data.get("accessControlEntries")
        if entries is None:
            raise ValueError("Unrecognized port access-list schema; refusing to host")
        require_private_access(entries)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["host", "renew", "doctor"])
    parser.add_argument("--config", required=True, type=Path)
    args = parser.parse_args()
    config = validate_config(json.loads(args.config.read_text()))
    # Authentication belongs to the CLI/operator, never to this service wrapper.
    inspect_remote(config)
    if args.action == "doctor":
        print(
            json.dumps(
                {
                    "status": "ready",
                    "tunnel_id": config["tunnel_id"],
                    "ports": config["ports"],
                    "anonymous_access": False,
                }
            )
        )
    elif args.action == "renew":
        run_cli(
            [config["binary"], "update", config["tunnel_id"], "--expiration", "30d"]
        )
        print("Tunnel lease renewed; host connection not restarted.")
    else:
        # No shell and no token on the command line. systemd supervises the CLI.
        os.execv(config["binary"], [config["binary"], "host", config["tunnel_id"]])
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(1)
