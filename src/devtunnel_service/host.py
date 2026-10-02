"""Persistent devtunnel hosting using the CLI's existing authentication context."""

import json
import os
import re
import subprocess
from pathlib import Path
from typing import Literal, TypedDict, cast

Action = Literal["host", "renew", "doctor"]


class Config(TypedDict):
    tunnel_id: str
    binary: str
    ports: list[int]
    allow_anonymous: bool


def validate_config(config: object) -> Config:
    if not isinstance(config, dict):
        raise ValueError("Configuration must be a JSON object")
    tunnel_id = config.get("tunnel_id")
    if not isinstance(tunnel_id, str) or not re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9._-]*", tunnel_id
    ):
        raise ValueError("A valid explicit tunnel ID is required")
    binary = config.get("binary")
    if not isinstance(binary, str) or not Path(binary).is_absolute():
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
    return cast(Config, config)


def run_cli(args: list[str], *, as_json: bool = False) -> object:
    result = subprocess.run(
        args, capture_output=True, text=True, timeout=45, check=False
    )
    if result.returncode:
        # Never forward arbitrary CLI output into service logs.
        raise RuntimeError(
            f"devtunnel {args[1]} failed with exit {result.returncode}; "
            "check the CLI login and tunnel permissions under the service's Unix user"
        )
    return json.loads(result.stdout) if as_json else None


def require_private_access(access: object) -> None:
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


def validate_remote(tunnel: object, expected_ports: list[int]) -> None:
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


def inspect_remote(config: Config) -> None:
    binary, tunnel_id = config["binary"], config["tunnel_id"]
    data = run_cli([binary, "show", tunnel_id, "--json"], as_json=True)
    if not isinstance(data, dict):
        raise ValueError("Unrecognized tunnel schema; refusing to host")
    validate_remote(data.get("tunnel"), config["ports"])
    for port in config["ports"]:
        # Access list includes effective tunnel/port ACL evidence.
        data = run_cli(
            [binary, "access", "list", tunnel_id, "--port-number", str(port), "--json"],
            as_json=True,
        )
        entries = data.get("accessControlEntries") if isinstance(data, dict) else None
        if entries is None:
            raise ValueError("Unrecognized port access-list schema; refusing to host")
        require_private_access(entries)


def run(action: Action, config_path: Path) -> int:
    config = validate_config(json.loads(config_path.read_text()))
    # Authentication belongs to the CLI/operator, never to this service wrapper.
    inspect_remote(config)
    if action == "doctor":
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
    elif action == "renew":
        run_cli(
            [config["binary"], "update", config["tunnel_id"], "--expiration", "30d"]
        )
        print("Tunnel lease renewed; host connection not restarted.")
    else:
        # No shell and no token on the command line. systemd supervises the CLI.
        os.execv(config["binary"], [config["binary"], "host", config["tunnel_id"]])
    return 0
