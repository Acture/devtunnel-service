# devtunnel-service

Run an existing persistent Microsoft Dev Tunnel as a Linux systemd user service.
The wrapper manages hosting, restart, and daily renewal of the tunnel's 30-day
lease. It uses the devtunnel CLI's **existing authentication context**.

**Authentication is outside this repository's responsibility.** It never runs
login or logout, selects an identity provider, stores tokens, or requires Azure
Managed Identity. It also has no dependency on TRAPI or LiteLLM: the forwarded
service can be SSH, a web application, or anything else supported by devtunnel.

## Requirements

- Linux with a working systemd user manager, and Python 3.10+.
- The [official devtunnel CLI](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/get-started)
  installed and authenticated **under the Unix user that runs the service**.
- An existing persistent tunnel that this CLI session can host and update, with
  its ports and access policies already configured.

Check the existing CLI context with `devtunnel user show` and
`devtunnel show TUNNEL_ID`. If login or permissions need attention, configure them
independently using your usual workflow. Devtunnel supports Microsoft and GitHub
accounts; this service does not choose between them. See the official
[credential commands](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/cli-commands#manage-user-credentials).

CLI flags and JSON shapes were checked against `1.0.2030+fc9273aa0f`. Unknown
access-control schemas fail closed and may require a wrapper update. Only the
Python standard library is required; the installer does not install the CLI.

## Install

Keep the checkout and Python executable in a stable location: the service runs
`host.py` directly from this checkout.

For example, to supervise an **already configured** tunnel with port 3000:

```bash
python3 deploy.py --name web --tunnel-id EXISTING_TUNNEL_ID --port 3000 --dry-run
```

Use your actual tunnel ID and port. Repeat `--port` for multiple ports; the set
must match the remote tunnel exactly. Use `--binary /absolute/path/to/devtunnel`
if the CLI is not on PATH.

- `--dry-run`: print the units; no writes, network calls, or service changes.
- Without `--dry-run`: install configuration and units, but do not start hosting.
- Add `--start`: enable and start this instance. On redeployment, this explicitly
  restarts the instance and disconnects its existing clients.

The installer does not create tunnels, add ports, change access policies, or
change your CLI login. Existing hosts are not restarted without `--start`.

Generated files are outside Git (`XDG_CONFIG_HOME` is respected):

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
python3 host.py doctor --config ~/.config/devtunnel-service/web.json
```

The CLI's host output includes the connection URL. `doctor` is read-only: it
checks remote ports and access rules, not end-to-end connectivity or application
health. All commands use the current CLI authentication context. If credentials
expire or access is revoked, fix that context outside this wrapper; restarting
the service is **not** a substitute for login or credential renewal.

A host that exits is restarted after 30 seconds. A separate daily timer extends
the tunnel lease to 30 days without intentionally restarting its host connection.
Lease renewal is not authentication-token renewal.

Stop the instance without deleting its tunnel:

```bash
systemctl --user disable --now devtunnel-web.service devtunnel-web-renew.timer
```

## Access policy and scope

This version retains a private-access guard: startup and renewal reject anonymous
allow entries, unexpected ports, and missing or unrecognized access-control
evidence. It does not modify remote ACLs. An existing anonymously accessible SSH
tunnel is therefore **not** automatically migrated into this wrapper.

These are periodic checks, not continuous enforcement. Restrict who can modify
the remote tunnel. Client access and application authentication remain governed
by their respective existing policies; neither is configured by this repository.
See Microsoft's [security documentation](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/security).

## Verification and migration

```bash
python3 -m unittest discover -v
```

Tests cover validation, CLI arguments, no-login behavior (including authentication
failure), generated units, dry-run behavior, private writes, and protection of
unmanaged services. New tunnel provisioning and end-to-end client connectivity
have not been tested by this repository; verify those for your deployment.

Earlier revisions incorrectly required identity IDs and performed login on
startup. Those options are removed. Re-run deployment without identity arguments
to regenerate configuration; a legacy `identity` JSON field is ignored. The CLI
context must already work for the service user. Existing system services outside
this repository are not changed by this migration.

Dev Tunnels is a development feature without a production SLA. Follow your
organization's policies when forwarding services.
