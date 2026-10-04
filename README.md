# devtunnel-service

Run an existing persistent Microsoft Dev Tunnel as a Linux systemd user service.
The `devtunnel-service` command manages hosting, restart, and daily renewal of the
tunnel's 30-day lease. It uses the devtunnel CLI's **existing authentication
context**.

**Authentication is outside this project's responsibility.** It never runs
login or logout, selects an identity provider, stores tokens, or requires Azure
Managed Identity. It also has no dependency on TRAPI or LiteLLM: the forwarded
service can be SSH, a web application, or anything else supported by devtunnel.

## Requirements

- Linux with a working systemd user manager, and Python 3.10+ (standard library
  only). `--help`, `--version` and deployment previews also work elsewhere.
- Microsoft's devtunnel CLI, installed and signed in **under the Unix user that
  runs the service**. No distribution packages it, and none of the installation
  channels below installs it. Microsoft's installer supports Linux x64 and arm64
  and writes `~/bin/devtunnel`:

  ```bash
  curl -sL https://aka.ms/DevTunnelCliInstall | bash
  ```

  See the [official instructions](https://learn.microsoft.com/azure/developer/dev-tunnels/get-started).
  The installer always fetches the latest CLI. Flags and JSON shapes were checked
  against `1.0.2030+fc9273aa0f` (`devtunnel --version`). Newer CLIs whose JSON
  output changes make startup and renewal fail closed and may require an update
  of this tool.
- An existing persistent tunnel that this CLI session can host and update, with
  its ports and access policies already configured.

Check the existing CLI context with `devtunnel user show` and
`devtunnel show TUNNEL_ID`. If login or permissions need attention, configure them
independently using your usual workflow. Devtunnel supports Microsoft and GitHub
accounts; this service does not choose between them. See the official
[credential commands](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/cli-commands#manage-user-credentials).

## Install

Every channel installs the same `devtunnel-service` package and command. Tagged
releases attach the wheel, sdist and Debian/Ubuntu package (`Architecture: all`)
to the GitHub Release, with `SHA256SUMS`.

| Channel | Command | Status |
| --- | --- | --- |
| uv tool (persistent) | `uv tool install devtunnel-service` | after PyPI publication; until then `uv tool install` the release wheel or `git+https://github.com/Acture/devtunnel-service@vX.Y.Z` |
| uvx (temporary) | `uvx devtunnel-service --help` | after PyPI publication; `uvx --from` the wheel works now |
| Debian 13, Ubuntu 24.04 | `sudo apt install ./devtunnel-service_X.Y.Z-1_all.deb` | from GitHub Releases; signed apt repository planned |
| Homebrew | `brew install acture/ac/devtunnel-service` | planned in the `acture/ac` tap |

`uvx` runs from a temporary cache. Use it for help, `doctor`, previews or a
foreground `host`, not for installing units: a real deployment refuses to make
systemd depend on that cache. Install persistently first, then deploy with the
installed command.

## Deploy

For example, to supervise an **already configured** tunnel with port 3000:

```bash
devtunnel-service deploy --name web --tunnel-id EXISTING_TUNNEL_ID --port 3000 --dry-run
```

Use your actual tunnel ID and port. Repeat `--port` for multiple ports; the set
must match the remote tunnel exactly. Use `--binary /absolute/path/to/devtunnel`
if the CLI is not on PATH (Microsoft's installer uses `~/bin/devtunnel`).

- `--dry-run`: print the units; no writes, network calls, or service changes.
- Without `--dry-run`: install configuration and units, but do not start hosting.
- Add `--start`: enable and start this instance. On redeployment, this explicitly
  restarts the instance and disconnects its existing clients.

The units run the `devtunnel-service` command found on PATH, by its stable path
(`/usr/bin`, `~/.local/bin` for uv tool, or Homebrew's `bin`), so they keep
working after upgrades. Deployment refuses commands inside temporary or cache
directories (such as uvx) and environments inside a source tree; the dry run
prints the reason instead. `--entry-point PATH` selects another persistent
installation explicitly.

The installer does not create tunnels, add ports, change access policies, or
change your CLI login. Existing hosts are not restarted without `--start`.

Generated files are user configuration (`XDG_CONFIG_HOME` is respected):

```text
~/.config/devtunnel-service/web.json
~/.config/systemd/user/devtunnel-web.service
~/.config/systemd/user/devtunnel-web-renew.service
~/.config/systemd/user/devtunnel-web-renew.timer
```

Configuration is mode `0600`; replaced files get a `.previous` backup. Unmanaged
units and same-named system-level services are not overwritten or shadowed.
For boot/logout persistence, an administrator may need to enable lingering for
your Unix account. The installer does not change that setting.

## Operate

```bash
systemctl --user enable --now devtunnel-web.service devtunnel-web-renew.timer
systemctl --user status devtunnel-web.service
journalctl --user -u devtunnel-web.service -f
devtunnel-service doctor --config ~/.config/devtunnel-service/web.json
```

The CLI's host output includes the connection URL. `doctor` is read-only: it
checks remote ports and access rules, not end-to-end connectivity or application
health. All commands use the current CLI authentication context. If credentials
expire or access is revoked, fix that context outside this wrapper; restarting
the service is **not** a substitute for login or credential renewal.

A host that exits is restarted after 30 seconds. A separate daily timer extends
the tunnel lease to 30 days without intentionally restarting its host connection.
Lease renewal is not authentication-token renewal.

Upgrading the package does not restart a running host. Restart the instance to
run the new version: `systemctl --user restart devtunnel-web.service`.

Stop the instance without deleting its tunnel:

```bash
systemctl --user disable --now devtunnel-web.service devtunnel-web-renew.timer
```

## Uninstall

Stop each instance and remove its units **before** removing the package;
otherwise systemd keeps restarting a host whose command is gone:

```bash
systemctl --user disable --now devtunnel-web.service devtunnel-web-renew.timer
rm ~/.config/systemd/user/devtunnel-web.service \
	~/.config/systemd/user/devtunnel-web-renew.service \
	~/.config/systemd/user/devtunnel-web-renew.timer
systemctl --user daemon-reload
```

Then remove the package with its channel (`uv tool uninstall devtunnel-service`,
`sudo apt remove devtunnel-service`, ...). Package removal, including
`apt purge`, keeps `~/.config/devtunnel-service`, the devtunnel CLI and its login.
Delete the instance configuration yourself if it is no longer needed.

## Access policy and scope

This version retains a private-access guard: startup and renewal reject anonymous
allow entries, unexpected ports, and missing or unrecognized access-control
evidence. It does not modify remote ACLs. An existing anonymously accessible SSH
tunnel is therefore **not** automatically migrated into this wrapper.

These are periodic checks, not continuous enforcement. Restrict who can modify
the remote tunnel. Client access and application authentication remain governed
by their respective existing policies; neither is configured by this repository.
See Microsoft's [security documentation](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/security).

## Development and verification

Source lives in `src/devtunnel_service/`, tests in `tests/`, and distribution
packaging in `packaging/`, including Debian metadata in `packaging/debian/`.
CI writes built packages and release staging files under the Git-ignored `dist/`.

```bash
uv run python -m unittest discover --start-directory tests -v
uv run ruff check && uv run ruff format --check && uv run ty check
```

Tests cover validation, CLI arguments, no-login behavior (including authentication
failure), generated units, entry-point persistence, dry-run behavior, private
writes, and protection of unmanaged services.

Packaging acceptance lives in `packaging/` and runs in CI:

- `check-dist.sh` builds the wheel and sdist from a copy of the source, deletes
  that copy, and checks both artifacts (including the test suite shipped in the
  sdist), uvx, and the uv tool entry point. With `--systemd` it deploys real
  user units against an offline fake devtunnel CLI.
- `build-deb.sh` builds the `.deb` from that sdist and `packaging/debian/` on Debian 13
  and runs lintian.
- `check-deb.sh` installs, upgrades, removes, purges and reinstalls the `.deb`
  on Debian 13 or Ubuntu 24.04 under a real systemd user manager, with
  networking disabled during installation.

CI runs these on amd64 and arm64. New tunnel provisioning and end-to-end client
connectivity through the real devtunnel service are not tested; verify those
for your deployment.

## Migrating from checkout-based deployments

Earlier revisions ran `deploy.py` and `host.py` from a Git checkout, and units
referenced that checkout. Install the package with any channel above, then
re-run `devtunnel-service deploy` with the same instance name: the managed units
are replaced (with `.previous` backups) and point at the installed command. The
checkout can be removed afterwards.

Earlier revisions also required identity IDs and performed login on startup.
Those options are removed; a legacy `identity` JSON field is ignored. The CLI
context must already work for the service user. Existing system services outside
this repository are not changed by this migration.

Dev Tunnels is a development feature without a production SLA. Follow your
organization's policies when forwarding services.

## License

[AGPL-3.0-only](LICENSE).
