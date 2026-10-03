//! Fail-closed checks of the remote tunnel and detection of other hosts.
//!
//! The tunnel comes from the SDK's `get_tunnel` with `include_ports` and
//! `include_access_control`. Deserialization already rejects unknown ACE
//! types and ACLs without `entries`; these checks reject what deserializes
//! but is not evidence: a missing ACL, an empty port list, and a port set
//! that differs from the configuration. Inverse, organization and other
//! non-anonymous rules are the user's responsibility and are not reviewed.

use tunnels::contracts::Tunnel;

use crate::error::Result;

pub(crate) const MISSING_ACCESS_CONTROL: &str =
	"Missing or unrecognized access-control evidence; refusing to host";
pub(crate) const ANONYMOUS_REFUSED: &str = "Anonymous tunnel access is enabled; refusing to host. \
	To accept it, redeploy with --allow-anonymous";
pub(crate) const MISSING_PORTS: &str = "Missing remote port inventory";
pub(crate) const PORTS_DIFFER: &str =
	"Remote ports differ from explicitly configured ports; refusing to host extra ports";
/// Shown by `deploy` (including `--dry-run`), at `host` start and by `renew`
/// whenever `allow_anonymous` is enabled.
pub(crate) const ANONYMOUS_RISK: &str = "allow_anonymous is enabled: anonymous access rules are \
	accepted, and anyone who has the tunnel URL can then reach the forwarded services";

/// What a successful check observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Checked {
	/// An anonymous allow rule exists at tunnel or port level.
	pub anonymous_access: bool,
}

/// Validates `tunnel` against the configured ports, in this order (all
/// `ValueError`):
///
/// 1. tunnel-level `access_control` is `None`: [`MISSING_ACCESS_CONTROL`];
/// 2. an anonymous allow entry (type `Anonymous`, not `is_deny`) while
///    `!allow_anonymous`: [`ANONYMOUS_REFUSED`];
/// 3. `ports` is empty: [`MISSING_PORTS`];
/// 4. remote port numbers contain duplicates or differ from `ports` as a
///    set: [`PORTS_DIFFER`];
/// 5. for each port, a `None` port-level `access_control`:
///    `Missing access-control evidence for port {n}; refusing to host`;
///    and port-level anonymous allow entries (including inherited ones) as
///    in step 2.
pub(crate) fn validate(tunnel: &Tunnel, ports: &[u16], allow_anonymous: bool) -> Result<Checked> {
	let _ = (tunnel, ports, allow_anonymous);
	todo!("remote::validate")
}

/// Whether another host serves the tunnel: the service reports host
/// connections (or omits the count) and some endpoint belongs to a host
/// other than `own_host_id` (`None` before this process first connected).
/// A reported count of zero means no host is connected, whatever endpoints
/// remain registered.
pub(crate) fn other_host(tunnel: &Tunnel, own_host_id: Option<&str>) -> bool {
	let _ = (tunnel, own_host_id);
	todo!("remote::other_host")
}
