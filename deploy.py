"""Install a user-level devtunnel host and daily lease-renewal timer."""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from host import validate_config

SOURCE = Path(__file__).resolve().parent
MARKER = "# Managed by devtunnel-service\n"


def quote(value) -> str:
    text = str(value)
    if any(ord(char) < 32 for char in text):
        raise ValueError("Control characters are not supported in service paths")
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'


def render_units(
    name: str, config_path: Path, python: Path, source: Path
) -> dict[str, str]:
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,47}", name):
        raise ValueError(
            "Instance name must contain lowercase letters, digits and hyphens"
        )
    prefix = "devtunnel-" + name
    command = f"{quote(python)} {quote(source / 'host.py')}"
    settings = f"""WorkingDirectory={str(source).replace("%", "%%")}
Environment=PYTHONDONTWRITEBYTECODE=1
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
"""
    service = (
        MARKER
        + f"""[Unit]
Description=Persistent devtunnel host ({name})
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
{settings}ExecStart={command} host --config {quote(config_path)}
Restart=always
RestartSec=30
KillSignal=SIGINT
TimeoutStopSec=30

[Install]
WantedBy=default.target
"""
    )
    renewal = (
        MARKER
        + f"""[Unit]
Description=Renew devtunnel lease ({name})
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
{settings}ExecStart={command} renew --config {quote(config_path)}
TimeoutStartSec=180
"""
    )
    timer = (
        MARKER
        + f"""[Unit]
Description=Daily devtunnel lease renewal ({name})

[Timer]
OnCalendar=daily
RandomizedDelaySec=300
Persistent=true
Unit={prefix}-renew.service

[Install]
WantedBy=timers.target
"""
    )
    return {
        prefix + ".service": service,
        prefix + "-renew.service": renewal,
        prefix + "-renew.timer": timer,
    }


def write_private(path: Path, text: str) -> None:
    fd, temporary = tempfile.mkstemp(prefix="." + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", required=True)
    parser.add_argument(
        "--tunnel-id",
        required=True,
        help="Existing persistent tunnel; never created implicitly",
    )
    parser.add_argument("--port", type=int, action="append", required=True)
    parser.add_argument("--binary", default=shutil.which("devtunnel"))
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--start",
        action="store_true",
        help="Explicitly start hosting after installation",
    )
    args = parser.parse_args()
    if not args.binary:
        raise ValueError("Install the official devtunnel CLI first, or pass --binary")
    binary = Path(args.binary).expanduser().resolve()
    config = validate_config(
        {
            "tunnel_id": args.tunnel_id,
            "binary": str(binary),
            "ports": sorted(args.port),
            "allow_anonymous": False,
        }
    )
    config_root = (
        Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
        .expanduser()
        .resolve()
    )
    config_dir = config_root / "devtunnel-service"
    config_path = config_dir / (args.name + ".json")
    if config_dir == SOURCE or SOURCE in config_dir.parents:
        raise ValueError("Runtime config must live outside the source checkout")
    units = render_units(args.name, config_path, Path(sys.executable).resolve(), SOURCE)
    if args.dry_run:
        for name, text in units.items():
            print(f"# {name}\n{text}")
        return 0
    if not sys.platform.startswith("linux") or not os.access(binary, os.X_OK):
        raise ValueError("Linux/systemd and an executable devtunnel CLI are required")
    # Do not shadow an existing system-level tunnel (especially the SSH tunnel).
    for name in units:
        result = subprocess.run(
            ["systemctl", "--system", "show", name, "-p", "LoadState", "--value"],
            capture_output=True,
            text=True,
            timeout=15,
        )
        if result.returncode != 0 or not result.stdout.strip():
            raise ValueError(
                "Cannot verify system-unit ownership; no files were changed"
            )
        if result.stdout.strip() != "not-found":
            raise ValueError(
                f"A system unit already uses {name}; choose a different instance name"
            )
    unit_dir = config_root / "systemd/user"
    for name in units:
        path = unit_dir / name
        if path.exists() and not path.read_text().startswith(MARKER):
            raise ValueError(f"Refusing to replace unmanaged unit {path}")
    with tempfile.TemporaryDirectory(prefix="devtunnel-units-") as folder:
        paths = []
        for name, text in units.items():
            path = Path(folder) / name
            path.write_text(text)
            paths.append(str(path))
        subprocess.run(["systemd-analyze", "--user", "verify", *paths], check=True)
    config_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    config_dir.chmod(0o700)
    unit_dir.mkdir(parents=True, exist_ok=True)
    if config_path.exists():
        write_private(
            config_path.with_suffix(".json.previous"), config_path.read_text()
        )
    write_private(config_path, json.dumps(config, indent=2) + "\n")
    for name, text in units.items():
        path = unit_dir / name
        if path.exists() and path.read_text() != text:
            write_private(path.with_suffix(path.suffix + ".previous"), path.read_text())
        write_private(path, text)
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    service = "devtunnel-" + args.name + ".service"
    timer = "devtunnel-" + args.name + "-renew.timer"
    if args.start:
        subprocess.run(["systemctl", "--user", "enable", service, timer], check=True)
        subprocess.run(["systemctl", "--user", "restart", service, timer], check=True)
        print(
            "Hosting requested. Inspect systemctl status/journal; CLI readiness is not yet certified."
        )
    else:
        print(
            "Installed only; no host was started or restarted. Existing hosts are left running."
        )
        print("To start: systemctl --user enable --now " + service + " " + timer)
    print("Config:", config_path)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(1)
