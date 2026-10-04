//! Fail-closed checks of the remote tunnel and detection of other hosts.
//!
//! The tunnel comes from the SDK's `get_tunnel` with `include_ports` and
//! `include_access_control`. Deserialization already rejects unknown ACE
//! types and ACLs without `entries`; these checks reject what deserializes
//! but is not evidence: a missing ACL, an empty port list, and a port set
//! that differs from the configuration. Inverse, organization and other
//! non-anonymous rules are the user's responsibility and are not reviewed.

use std::collections::BTreeSet;

use tunnels::contracts::{Tunnel, TunnelAccessControl, TunnelAccessControlEntryType};

use crate::error::{Error, Result};

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

/// Whether `acl` grants anonymous access; a refusal unless `allow_anonymous`.
fn anonymous(acl: &TunnelAccessControl, allow_anonymous: bool) -> Result<bool> {
	let granted = acl.entries.iter().any(|entry| {
		matches!(entry.kind, TunnelAccessControlEntryType::Anonymous) && !entry.is_deny
	});
	if granted && !allow_anonymous {
		return Err(Error::value(ANONYMOUS_REFUSED));
	}
	Ok(granted)
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
	let acl = tunnel
		.access_control
		.as_ref()
		.ok_or_else(|| Error::value(MISSING_ACCESS_CONTROL))?;
	let mut anonymous_access = anonymous(acl, allow_anonymous)?;
	if tunnel.ports.is_empty() {
		return Err(Error::value(MISSING_PORTS));
	}
	let remote: BTreeSet<u16> = tunnel.ports.iter().map(|port| port.port_number).collect();
	let expected: BTreeSet<u16> = ports.iter().copied().collect();
	if remote.len() != tunnel.ports.len() || remote != expected {
		return Err(Error::value(PORTS_DIFFER));
	}
	for port in &tunnel.ports {
		let acl = port.access_control.as_ref().ok_or_else(|| {
			Error::value(format!(
				"Missing access-control evidence for port {}; refusing to host",
				port.port_number
			))
		})?;
		anonymous_access |= anonymous(acl, allow_anonymous)?;
	}
	Ok(Checked { anonymous_access })
}

/// Whether another host serves the tunnel, judged while this host is not
/// connected. A reported host-connection count of zero means no host is
/// connected, whatever endpoints remain registered. Otherwise another host
/// is present when an endpoint belongs to a host other than `own_host_id`
/// (`None` before this process first connected), and also when the service
/// reports connections but lists no endpoint to attribute them to, so the
/// check fails closed. No count and no endpoints means no host.
pub(crate) fn other_host(tunnel: &Tunnel, own_host_id: Option<&str>) -> bool {
	let count = tunnel
		.status
		.as_ref()
		.and_then(|status| status.host_connection_count.as_ref())
		.map(|count| count.get_count());
	match count {
		Some(0) => false,
		Some(_) if tunnel.endpoints.is_empty() => true,
		_ => tunnel
			.endpoints
			.iter()
			.any(|endpoint| Some(endpoint.host_id.as_str()) != own_host_id),
	}
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, json};

	use super::*;

	fn ace(kind: &str) -> Value {
		json!({"type": kind, "subjects": [], "scopes": ["connect"]})
	}

	fn port(number: u16, entries: Value) -> Value {
		json!({"portNumber": number, "accessControl": {"entries": entries}})
	}

	fn tunnel(value: Value) -> Tunnel {
		serde_json::from_value(value).unwrap()
	}

	fn private() -> Value {
		json!({"accessControl": {"entries": []}, "ports": [port(4000, json!([]))]})
	}

	fn refusal(value: Value, ports: &[u16]) -> String {
		validate(&tunnel(value), ports, false)
			.unwrap_err()
			.to_string()
	}

	#[test]
	fn private_remote() {
		let checked = validate(&tunnel(private()), &[4000], false).unwrap();
		assert!(!checked.anonymous_access);
	}

	#[test]
	fn missing_acl_fails_closed() {
		assert_eq!(
			refusal(json!({"ports": [port(4000, json!([]))]}), &[4000]),
			format!("ValueError: {MISSING_ACCESS_CONTROL}")
		);
		let mut value = private();
		value["ports"][0]
			.as_object_mut()
			.unwrap()
			.remove("accessControl");
		assert_eq!(
			refusal(value, &[4000]),
			"ValueError: Missing access-control evidence for port 4000; refusing to host"
		);
	}

	#[test]
	fn malformed_acl_does_not_deserialize() {
		for acl in [
			json!({}),
			json!([]),
			json!({"entries": [ace("Unknown")]}),
			json!({"entries": [{"type": null, "subjects": [], "scopes": []}]}),
			json!({"entries": [{"type": "Anonymous", "isDeny": "true", "subjects": [], "scopes": []}]}),
		] {
			let mut value = private();
			value["accessControl"] = acl;
			assert!(serde_json::from_value::<Tunnel>(value).is_err());
		}
	}

	#[test]
	fn anonymous_allow_rejected_at_tunnel_and_port_level() {
		let mut value = private();
		value["accessControl"]["entries"] = json!([ace("Anonymous")]);
		assert_eq!(
			refusal(value, &[4000]),
			format!("ValueError: {ANONYMOUS_REFUSED}")
		);
		let mut inherited = ace("Anonymous");
		inherited["isInherited"] = json!(true);
		let mut value = private();
		value["ports"][0] = port(4000, json!([inherited]));
		assert_eq!(
			refusal(value, &[4000]),
			format!("ValueError: {ANONYMOUS_REFUSED}")
		);
	}

	#[test]
	fn anonymous_deny_is_not_a_grant() {
		let mut deny = ace("Anonymous");
		deny["isDeny"] = json!(true);
		let mut value = private();
		value["accessControl"]["entries"] = json!([deny]);
		assert!(
			!validate(&tunnel(value), &[4000], false)
				.unwrap()
				.anonymous_access
		);
	}

	#[test]
	fn allow_anonymous_accepts_and_reports() {
		let mut value = private();
		value["ports"][0] = port(4000, json!([ace("Anonymous")]));
		assert!(
			validate(&tunnel(value), &[4000], true)
				.unwrap()
				.anonymous_access
		);
	}

	#[test]
	fn other_rules_are_not_reviewed() {
		let mut inverse = ace("Organizations");
		inverse["isInverse"] = json!(true);
		let mut value = private();
		value["accessControl"]["entries"] = json!([inverse, ace("None"), ace("IPAddressRanges")]);
		validate(&tunnel(value), &[4000], false).unwrap();
	}

	#[test]
	fn empty_extra_missing_or_duplicate_ports() {
		let mut value = private();
		value["ports"] = json!([]);
		assert_eq!(
			refusal(value, &[4000]),
			format!("ValueError: {MISSING_PORTS}")
		);
		for ports in [
			json!([port(22, json!([]))]),
			json!([port(4000, json!([])), port(22, json!([]))]),
			json!([port(4000, json!([])), port(4000, json!([]))]),
		] {
			let mut value = private();
			value["ports"] = ports;
			assert_eq!(
				refusal(value, &[4000]),
				format!("ValueError: {PORTS_DIFFER}")
			);
		}
		assert_eq!(
			refusal(private(), &[4000, 5000]),
			format!("ValueError: {PORTS_DIFFER}")
		);
	}

	fn hosted(endpoints: &[&str], count: Option<Value>) -> Tunnel {
		let endpoints: Vec<Value> = endpoints
			.iter()
			.map(|host| json!({"connectionMode": "TunnelRelay", "hostId": host}))
			.collect();
		let mut value = json!({"endpoints": endpoints});
		if let Some(count) = count {
			value["status"] = json!({"hostConnectionCount": count});
		}
		tunnel(value)
	}

	#[test]
	fn detects_other_hosts() {
		assert!(!other_host(&hosted(&[], None), Some("me")));
		// Connections without endpoints cannot be attributed: fail closed.
		assert!(other_host(&hosted(&[], Some(json!(1))), Some("me")));
		assert!(other_host(&hosted(&[], Some(json!(1))), None));
		assert!(!other_host(&hosted(&["me"], Some(json!(1))), Some("me")));
		assert!(other_host(
			&hosted(&["me", "them"], Some(json!(1))),
			Some("me")
		));
		assert!(other_host(&hosted(&["them"], None), Some("me")));
		assert!(other_host(
			&hosted(&["them"], Some(json!({"current": 2}))),
			Some("me")
		));
		assert!(other_host(&hosted(&["stale"], Some(json!(1))), None));
	}

	#[test]
	fn zero_host_connections_means_no_host() {
		assert!(!other_host(&hosted(&["stale"], Some(json!(0))), Some("me")));
		assert!(!other_host(
			&hosted(&["stale"], Some(json!({"current": 0}))),
			None
		));
	}
}
