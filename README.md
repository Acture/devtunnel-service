# devtunnel-service

Run an existing persistent Microsoft Dev Tunnel as a Linux systemd user service.
The `devtunnel-service` command manages hosting, restart, and daily renewal of the
tunnel's 30-day lease. It uses the devtunnel CLI's **existing authentication
context**.

This tree is version 0.2.0 (unreleased), a Rust implementation that hosts the
tunnel itself through Microsoft's
[dev-tunnels Rust SDK](https://github.com/microsoft/dev-tunnels) instead of
running `devtunnel host`. The published packages still ship 0.1.0, the Python
implementation, which stays in this tree until the Rust release replaces it.
See [Changes from 0.1.0](#changes-from-010).

**Authentication is outside this project's responsibility.** It never runs
login or logout, selects an identity provider, stores tokens, or requires Azure
Managed Identity. Before every relay connection it asks the signed-in CLI for a
host token (`devtunnel token TUNNEL_ID --scopes host --json`) and holds that
token (24-hour lifetime) in process memory only: never on disk, in logs or in
argv. Lease renewal still runs `devtunnel update TUNNEL_ID --expiration 30d`.
There is no dependency on TRAPI or LiteLLM: the forwarded service can be SSH, a
web application, or anything else supported by devtunnel.

## Requirements

- Linux x64 or arm64 with a working systemd user manager. `--help`, `--version`
  and deployment previews also work elsewhere. The 0.2.0 binary needs only the
  system C runtime (nothing when built static); the 0.1.0 packages need
  Python 3.10+ (standard library only).
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
  its access policies already configured and the forwarded ports already added.
  Only the configured ports are forwarded, and the service refuses to create
  missing ones.

Check the existing CLI context with `devtunnel user show` and
`devtunnel show TUNNEL_ID`. If login or permissions need attention, configure them
independently using your usual workflow. Devtunnel supports Microsoft and GitHub
accounts; this service does not choose between them. See the official
[credential commands](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/cli-commands#manage-user-credentials).

## Install

Every channel installs the same `devtunnel-service` package and command. Tagged
releases attach the wheel, sdist and Debian/Ubuntu package (`Architecture: all`)
to the GitHub Release, with `SHA256SUMS`. These channels ship 0.1.0, the Python
implementation; until 0.2.0 packages exist, [build it from source](#build-from-source).

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

### Build from source

Building 0.2.0 needs Rust 1.88 or newer, a C toolchain and perl (OpenSSL is
vendored and built from source):

```bash
cargo build --release --locked
```

For a static binary, install musl-tools and build for the musl target of your
architecture:

```bash
sudo apt install musl-tools
rustup target add x86_64-unknown-linux-musl  # or aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
```

Install the binary as a persistent command before deploying, for example with
`cargo install --path . --locked`, which puts it in `~/.cargo/bin`, or by copying
it to `/usr/local/bin`. Deployment refuses binaries inside cargo's `target/`
directory or any source tree.

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
- Add `--allow-anonymous`: accept anonymous access rules on the tunnel; see
  [Access policy and scope](#access-policy-and-scope).

The units run the `devtunnel-service` command found on PATH, by its stable path
(`/usr/bin`, `~/.local/bin` for uv tool, `~/.cargo/bin` for cargo install, or
Homebrew's `bin`), so they keep working after upgrades. Deployment refuses
commands inside temporary or cache directories (such as uvx, or cargo's
`target/`) and commands or environments inside a source tree; the dry run prints
the reason instead. `--entry-point PATH` selects another persistent installation
explicitly.

The installer does not create tunnels, add ports, change access policies, or
change your CLI login. Existing hosts are not restarted without `--start`.

Generated files are user configuration (`XDG_CONFIG_HOME` is respected):

```text
~/.config/devtunnel-service/web.json
~/.config/systemd/user/devtunnel-web.service
~/.config/systemd/user/devtunnel-web-renew.service
~/.config/systemd/user/devtunnel-web-renew.timer
```

Configuration is mode `0600`; replaced files get a `.previous` backup. It holds
`"allow_anonymous": true` only with `--allow-anonymous`, otherwise `false`, as
in every configuration 0.1.0 wrote. Unmanaged units and same-named system-level
services are not overwritten or shadowed. For boot/logout persistence, an
administrator may need to enable lingering for your Unix account. The installer
does not change that setting.

## Operate

```bash
systemctl --user enable --now devtunnel-web.service devtunnel-web-renew.timer
systemctl --user status devtunnel-web.service
journalctl --user -u devtunnel-web.service -f
devtunnel-service doctor --config ~/.config/devtunnel-service/web.json
```

The host logs to the journal with priorities (add `-p warning` to `journalctl`
for warnings and errors only) and, when it connects, logs each port's
forwarding URL as the service reports it. `doctor` is read-only: it checks
remote ports and access rules, not end-to-end connectivity or application
health, and its JSON line reports whether it observed anonymous access. All
commands use the current CLI authentication context. Operational errors print
one line with 0.1.0's format and prefixes (`ValueError:`, `RuntimeError:`,
`FileNotFoundError:`, ...) and exit with status 1.

The service never displaces a live host. At start and after a disconnect, if
another host is connected to the tunnel, it stands by, rechecks every minute,
and takes over once none is connected; displacement, standby and takeover are
logged. An endpoint left by a host that exited uncleanly can delay the takeover
by about 1–2 minutes. On SIGTERM or SIGINT (`systemctl --user stop` or
`restart`) it unregisters its endpoint before exiting.

Network and relay failures reconnect with backoff, from 2 seconds doubling up to
60. Login, permission and check failures exit with status 1, so systemd shows
them and restarts the host after 30 seconds; while standing by, only failed
checks exit and other errors are retried at the next poll. If credentials expire
or access is revoked, fix the CLI context outside this wrapper; restarting the
service is **not** a substitute for login or credential renewal.

A separate daily timer extends the tunnel lease to 30 days without intentionally
restarting its host connection. Lease renewal is not authentication-token
renewal.

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
`sudo apt remove devtunnel-service`, `cargo uninstall devtunnel-service`, ...).
Package removal, including `apt purge`, keeps `~/.config/devtunnel-service`, the
devtunnel CLI and its login. Delete the instance configuration yourself if it is
no longer needed.

## Access policy and scope

Checks fail closed at host start, before every reconnection, on every standby
poll, and in `renew` and `doctor`. They refuse:

- anonymous allow rules at tunnel or port level, unless `allow_anonymous` is
  set; the error names `--allow-anonymous`;
- remote ports that differ from the configured set, or an empty port list;
- missing tunnel- or port-level access control;
- access-control entry types this version does not know.

Inverse, organization and other non-anonymous rules are not reviewed; they are
your responsibility. The service does not modify remote ACLs. A live connection
is not re-checked periodically: these are checks, not continuous enforcement.
Restrict who can modify the remote tunnel.

With `allow_anonymous`, hosting proceeds, and `deploy` (including `--dry-run`),
host start and `renew` warn that anyone with the tunnel URL can reach the
forwarded services. An existing anonymously accessible tunnel, such as an SSH
tunnel, is therefore hosted only after an explicit redeploy with
`--allow-anonymous`.

Client access and application authentication remain governed by their
respective existing policies; neither is configured by this repository.
See Microsoft's [security documentation](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/security).

## Changes from 0.1.0

- `host` hosts the tunnel through the dev-tunnels Rust SDK instead of running
  `devtunnel host`. The CLI still mints the host token and renews the lease.
- Only the configured ports are forwarded, and they must already exist on the
  tunnel.
- `deploy --allow-anonymous` sets the new `allow_anonymous`; it defaults to
  `false`, which 0.1.0 configurations already hold.
- Checks also run before every reconnection and on every standby poll, and
  `doctor` reports observed anonymous access.
- A live host is never displaced: the service stands by and takes over later,
  and unregisters its endpoint when stopped.
- Network and relay failures reconnect with backoff; other failures exit with
  status 1 for systemd to restart.
- Logs carry journal priorities and each port's forwarding URL.
- Units no longer set `Environment=PYTHONDONTWRITEBYTECODE=1`. Existing units
  and configurations keep working with the new command at the same path;
  redeploying rewrites the units, with `.previous` backups.
- Error lines keep 0.1.0's one-line format and prefixes.

## Development and verification

The Rust crate is rooted at `Cargo.toml` with its sources in `src/*.rs`; the
Python package lives in `src/devtunnel_service/`. Tests of both live in
`tests/`, and distribution packaging in `packaging/`, including Debian
metadata in `packaging/debian/`. CI writes built packages and release staging
files under the Git-ignored `dist/`.

The Rust implementation:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit
```

`cargo audit` reads `.cargo/audit.toml`, which records why each accepted
advisory is acceptable; all of them come from the russh version that the SDK
pins. Rust tests use no network, real devtunnel CLI or systemd.

The Python implementation, while both live in this tree:

```bash
uv run python -m unittest discover --start-directory tests -v
uv run ruff check && uv run ruff format --check && uv run ty check
```

The Python tests cover validation, CLI arguments, no-login behavior (including
authentication failure), generated units, entry-point persistence, dry-run
behavior, private writes, and protection of unmanaged services.

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

CI runs these on amd64 and arm64, together with the Rust checks, a static musl
build whose `--version` must match `Cargo.toml`, and `cargo audit`. New tunnel
provisioning and end-to-end client connectivity through the real devtunnel
service are not tested; verify those for your deployment.

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
