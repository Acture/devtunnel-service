# devtunnel-service

Run an **existing persistent Microsoft Dev Tunnel** as a Linux systemd user
service, authenticated with Azure Managed Identity. A separate daily timer renews
the tunnel's 30-day lease without intentionally restarting its host connection.

This is a small, standard-library-only deployment wrapper, not a tunnel server.
It does not install the CLI, create tunnels, add ports, or grant access. Installation
does **not** start hosting unless you explicitly pass `--start`.

## Requirements

- Linux with a working systemd user manager and Python 3.10+.
- The [official devtunnel CLI](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/get-started).
  Command flags and JSON shapes were checked against CLI `1.0.2030+fc9273aa0f`.
  A schema change fails closed and may require a wrapper update.
- An Azure Managed Identity attached to the machine, with permission to manage
  the tunnel. Supply its **object ID** or **client ID**, not a secret.
- A persistent tunnel owned by, or appropriately granted to, that identity.

No Azure CLI, client secret, or copied bearer token is required. The devtunnel CLI
owns its authentication cache. Instances under the same Unix user must use the
same identity; use separate Unix users for separate identities.

## Provision the tunnel once

Run these commands yourself, substituting your identity and a unique tunnel ID:

```bash
devtunnel user login --mi-object-id YOUR_MANAGED_IDENTITY_OBJECT_ID
devtunnel create my-model-api --expiration 30d
devtunnel port create my-model-api --port-number 4000 --protocol http
```

Alternatively, use `--mi-client-id YOUR_MANAGED_IDENTITY_CLIENT_ID` when logging in.
Do not pass `--allow-anonymous`. The wrapper requires private tunnel access and
rejects anonymous allow entries at both the tunnel and port levels. It never
changes access policies automatically.

For the model gateway, run [trapi2litellm](https://github.com/Acture/trapi2litellm)
on `127.0.0.1:4000` first. The tunnel forwards that same port; model discovery,
status, and inference remain on one endpoint.

## Install and start

Clone this repository to a stable location: the service executes `host.py` from
the checkout, so **keep the checkout and Python executable in place**.

```bash
python3 deploy.py \
  --name model-api \
  --tunnel-id my-model-api \
  --port 4000 \
  --mi-object-id YOUR_MANAGED_IDENTITY_OBJECT_ID \
  --dry-run
```

Replace the placeholder with a real UUID even for a dry run. If `devtunnel` is
not on your PATH, supply `--binary /absolute/path/to/devtunnel`.

Remove `--dry-run` to install only; add `--start` to enable and start hosting.
Re-running with `--start` restarts this instance and disconnects existing clients.
Without `--start`, existing hosts continue running; applying changes to an active
host requires an explicit restart.

Generated files, all outside Git:

```text
~/.config/devtunnel-service/model-api.json
~/.config/systemd/user/devtunnel-model-api.service
~/.config/systemd/user/devtunnel-model-api-renew.service
~/.config/systemd/user/devtunnel-model-api-renew.timer
```

`XDG_CONFIG_HOME` is respected. Configuration has mode `0600`; replaced files
have a `.previous` backup. Unmanaged units and same-named system services are not
overwritten or shadowed. For example, `--name ssh` is refused if a system-level
`devtunnel-ssh.service` already exists.

To start after an install-only deployment:

```bash
systemctl --user enable --now devtunnel-model-api.service devtunnel-model-api-renew.timer
systemctl --user status devtunnel-model-api.service
journalctl --user -u devtunnel-model-api.service -f
```

The CLI's host output includes the assigned URL. For services to survive logout
and start at boot, an administrator may need to enable lingering for your Unix
account with `loginctl enable-linger USERNAME`. The installer does not change it.

## Authentication from clients

There are **two independent authentication layers**: private tunnel access and
the application API key. Keeping one URL does not eliminate either layer.

- Use `devtunnel connect TUNNEL_ID` from an authorized client account, then point
  the SDK at the local forwarded port. The SDK only needs the gateway API key;
  the CLI handles tunnel authentication.
- Or access the tunnel HTTPS URL directly. Besides the application's
  `Authorization: Bearer ...` header, send
  `X-Tunnel-Authorization: tunnel <CONNECT_TOKEN>`. Obtain a short-lived connect
  token through an authorized tunnel identity; do not commit it or assume it is
  permanent. A gateway SDK base URL is the tunnel URL plus `/v1`.

An ordinary personal login is not automatically authorized for a tunnel owned
by a Managed Identity. Provision a specific client grant or connect token through
your organization's approved access workflow. This wrapper does not do that.
See Microsoft's [security documentation](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/security)
and [CLI reference](https://learn.microsoft.com/en-us/azure/developer/dev-tunnels/cli-commands).

## Checks and recovery

```bash
python3 -m unittest discover -v
python3 host.py doctor --config ~/.config/devtunnel-service/model-api.json
systemctl --user list-timers devtunnel-model-api-renew.timer
```

`doctor` is read-only and uses the current CLI login. It validates the exact
remote port set and tunnel/port access-control evidence. It does **not** prove
end-to-end connectivity or application health. `host` and `renew` log in using
Managed Identity before doing those checks. A host process that exits is restarted
after 30 seconds, with a fresh login. Daily lease renewal is separate from token
refresh and does not certify indefinite authentication of an active connection.

Startup and renewal reject anonymous access, unexpected ports, and missing or
unrecognized access-control evidence. These are checks, not continuous enforcement:
restrict who can modify the remote tunnel, because access policy can change while
a host is running. Login diagnostics are suppressed in service logs to avoid
copying credential-helper output; repeat a failing login interactively if needed.

Stop this instance without deleting its persistent tunnel:

```bash
systemctl --user disable --now devtunnel-model-api.service devtunnel-model-api-renew.timer
```

## Scope and verification

The repository includes offline tests for validation, CLI argument construction,
credential-output suppression, generated systemd units, dry-run behavior, and
private file writes. Development also checked the installed CLI's read-only JSON
schemas. It has **not** provisioned a new tunnel or demonstrated an end-to-end
client connection; run that acceptance check for your deployment.

Dev Tunnels is a development feature, not an SLA-backed production gateway.
Check your organization's policies before forwarding company services.
