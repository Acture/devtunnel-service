"""Install a user-level devtunnel host and daily lease-renewal timer."""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from devtunnel_service.host import validate_config

PROGRAM = "devtunnel-service"
MARKER = "# Managed by devtunnel-service\n"
REMEDY = (
    f"install {PROGRAM} persistently (uv tool install {PROGRAM}, the Debian package "
    "or Homebrew) and run that command, or pass --entry-point"
)


def quote(value: str | Path) -> str:
    text = str(value)
    if any(ord(char) < 32 for char in text):
        raise ValueError("Control characters are not supported in service paths")
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'


def render_units(name: str, config_path: Path, entry: Path) -> dict[str, str]:
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,47}", name):
        raise ValueError(
            "Instance name must contain lowercase letters, digits and hyphens"
        )
    prefix = "devtunnel-" + name
    command = quote(entry)
    settings = """Environment=PYTHONDONTWRITEBYTECODE=1
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


def transient_roots() -> list[Path]:
    """Directories whose contents may disappear: temporary files and caches (uvx)."""
    cache = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    roots = [tempfile.gettempdir(), "/tmp", "/var/tmp", cache]
    roots.append(os.environ.get("UV_CACHE_DIR") or "")
    return [Path(root).expanduser().resolve() for root in roots if root]


def persistence_problem(entry: Path) -> str | None:
    """Explain why units must not reference ``entry``; None if it is persistent."""
    # Check the PATH-visible link and its target: both must survive the session.
    for path in (entry, entry.resolve()):
        for root in transient_roots():
            if path.is_relative_to(root):
                return f"{path} is inside the temporary or cache directory {root}"
        environment = next(
            (parent for parent in path.parents if (parent / "pyvenv.cfg").is_file()),
            None,
        )
        if environment and (environment.parent / "pyproject.toml").is_file():
            return f"{path} belongs to the environment of the source tree {environment.parent}"
    return None


def entry_point(explicit: Path | None) -> Path:
    found = explicit or shutil.which(PROGRAM)
    if not found:
        raise ValueError(f"No {PROGRAM} command on PATH; {REMEDY}")
    # Keep symlinks: /usr/bin, ~/.local/bin and Homebrew's bin stay valid across
    # upgrades, while their versioned targets do not.
    return Path(found).expanduser().absolute()


def deploy(
    name: str,
    tunnel_id: str,
    ports: list[int],
    binary: str | None,
    entry: Path | None,
    *,
    dry_run: bool,
    start: bool,
) -> int:
    binary = binary or shutil.which("devtunnel")
    if not binary:
        raise ValueError("Install the official devtunnel CLI first, or pass --binary")
    devtunnel = Path(binary).expanduser().resolve()
    config = validate_config(
        {
            "tunnel_id": tunnel_id,
            "binary": str(devtunnel),
            "ports": sorted(ports),
            "allow_anonymous": False,
        }
    )
    config_root = (
        Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
        .expanduser()
        .resolve()
    )
    config_dir = config_root / "devtunnel-service"
    config_path = config_dir / (name + ".json")
    command = entry_point(entry)
    problem = persistence_problem(command)
    units = render_units(name, config_path, command)
    if dry_run:
        for unit, text in units.items():
            print(f"# {unit}\n{text}")
        if problem:
            print(
                f"Note: deployment would refuse this entry point: {problem}; {REMEDY}.",
                file=sys.stderr,
            )
        return 0
    if problem:
        raise ValueError(f"Refusing a non-persistent entry point: {problem}; {REMEDY}")
    if not sys.platform.startswith("linux") or not os.access(devtunnel, os.X_OK):
        raise ValueError("Linux/systemd and an executable devtunnel CLI are required")
    if not os.access(command, os.X_OK):
        raise ValueError(f"Entry point {command} is not executable; {REMEDY}")
    # Do not shadow an existing system-level tunnel (especially the SSH tunnel).
    for unit in units:
        result = subprocess.run(
            ["systemctl", "--system", "show", unit, "-p", "LoadState", "--value"],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        if result.returncode != 0 or not result.stdout.strip():
            raise ValueError(
                "Cannot verify system-unit ownership; no files were changed"
            )
        if result.stdout.strip() != "not-found":
            raise ValueError(
                f"A system unit already uses {unit}; choose a different instance name"
            )
    unit_dir = config_root / "systemd/user"
    for unit in units:
        path = unit_dir / unit
        if path.exists() and not path.read_text().startswith(MARKER):
            raise ValueError(f"Refusing to replace unmanaged unit {path}")
    with tempfile.TemporaryDirectory(prefix="devtunnel-units-") as folder:
        paths = []
        for unit, text in units.items():
            path = Path(folder) / unit
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
    for unit, text in units.items():
        path = unit_dir / unit
        if path.exists() and path.read_text() != text:
            write_private(path.with_suffix(path.suffix + ".previous"), path.read_text())
        write_private(path, text)
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    service = "devtunnel-" + name + ".service"
    timer = "devtunnel-" + name + "-renew.timer"
    if start:
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
    print("Entry point:", command)
    return 0
