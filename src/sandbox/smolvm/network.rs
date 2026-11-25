//! `sandbox.network` as smolvm's per-machine egress policy.
//!
//! smolvm enforces the allowlist on the host. Under `policy: deny` the
//! machine gets the listed hosts, the private floor (`--block-private`), and
//! the RPC gateway reopened through it. Measured on smolvm 1.36.1: the gateway
//! reaches the host's loopback only when both `100.96.0.1/32` and
//! `127.0.0.1/32` are allowed, and under `--block-private` the gateway's CIDR
//! must come from `--allow-private`.

use anyhow::Result;
use std::net::{IpAddr, ToSocketAddrs};

use crate::config::{NetworkConfig, NetworkPolicy};
use crate::sandbox::network_proxy::{AddressClass, classify_ip};

/// The guest's gateway (and resolver), through which it reaches workmux's
/// RPC server on the host's loopback.
const GATEWAY_CIDR: &str = "100.96.0.1/32";
const HOST_LOOPBACK_CIDR: &str = "127.0.0.1/32";

/// The resolved addresses of the exact hosts that set `allow_private_ips`.
pub type PrivateHosts = Vec<(String, Vec<IpAddr>)>;

/// A machine's network: its create flags, and notes for `tracing`.
#[derive(Debug, Default)]
pub struct MachineNetwork {
    pub flags: Vec<String>,
    pub notes: Vec<String>,
}

/// The create flags for `network`. `private_hosts` holds the addresses of
/// exact hosts with `allow_private_ips` ([`resolve_private_hosts`]).
pub fn machine_network(
    network: &NetworkConfig,
    private_hosts: &PrivateHosts,
) -> Result<MachineNetwork> {
    // virtio-net always: the guest reaches the RPC server through its
    // gateway, and its own 127.0.0.1 never falls through to the host's.
    let mut flags: Vec<String> = ["--net", "--net-backend", "virtio-net"]
        .map(String::from)
        .to_vec();
    if network.policy() != NetworkPolicy::Deny {
        return Ok(MachineNetwork {
            flags,
            notes: Vec::new(),
        });
    }
    // Validated at load; again here, since IP literals and bad wildcards
    // would otherwise reach smolvm as allowlist entries.
    network.validate()?;

    flags.push("--block-private".into());
    for cidr in [GATEWAY_CIDR, HOST_LOOPBACK_CIDR] {
        flags.extend(["--allow-private".into(), cidr.into()]);
    }
    let rules = network.allowed_domain_rules();
    for rule in &rules {
        // A pattern admits an exact name only, or `*.domain`'s subdomains
        // only, as the container backend's proxy does. `--allow-host` would
        // also admit an exact name's subdomains.
        flags.extend(["--allow-host-pattern".into(), rule.host.clone()]);
    }
    for (host, addrs) in private_hosts {
        let allowed = rules
            .iter()
            .any(|rule| rule.allow_private_ips && &rule.host == host);
        if !allowed {
            continue;
        }
        // Public addresses need no reopening, and loopback and link-local
        // stay closed, as on the container backend's proxy.
        for addr in addrs {
            if classify_ip(addr) == AddressClass::PrivateRoutable {
                let bits = if addr.is_ipv4() { 32 } else { 128 };
                flags.extend(["--allow-private".into(), format!("{addr}/{bits}")]);
            }
        }
    }

    let mut notes = Vec::new();
    if rules.is_empty() {
        // `--block-private` alone leaves the public internet open; any
        // allowlist entry turns the allowlist on. Names still resolve.
        flags.extend(["--allow-cidr".into(), GATEWAY_CIDR.into()]);
        notes.push(
            "network policy deny with no allowed_domains: the machine reaches only workmux's \
             RPC gateway, but DNS names still resolve (DNS isn't filtered)"
                .to_string(),
        );
    } else {
        notes.push(
            "network policy deny: smolvm allows each listed host on every port, not only 443 \
             as the container backend's proxy does; `smolvm machine egress-events` shows \
             blocked connections"
                .to_string(),
        );
    }
    if network.max_connections.is_some() {
        notes.push(
            "sandbox.network.max_connections isn't enforced by the smolvm backend".to_string(),
        );
    }
    Ok(MachineNetwork { flags, notes })
}

/// Check, at config load, that smolvm's `--allow-host-pattern` takes each
/// `allowed_domains` entry under `policy: deny`: a hostname or `*.domain`
/// of letters, digits and hyphens. smolvm would refuse it only at create,
/// in the pane.
pub fn validate_allowlist(network: &NetworkConfig) -> Result<()> {
    if network.policy() != NetworkPolicy::Deny {
        return Ok(());
    }
    for rule in network.allowed_domain_rules() {
        let name = rule.host.strip_prefix("*.").unwrap_or(&rule.host);
        let valid = name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if !valid {
            anyhow::bail!(
                "sandbox.network.allowed_domains: smolvm can't allow '{}': use a hostname of \
                 letters, digits and hyphens, or *.domain",
                rule.host
            );
        }
    }
    Ok(())
}

/// Resolve each exact host with `allow_private_ips` under `policy: deny`, so
/// its private addresses can be reopened at create. Each pane resolves
/// afresh. A host that doesn't resolve is left out with a warning: the
/// machine then can't reach it on a private address.
pub fn resolve_private_hosts(network: &NetworkConfig) -> PrivateHosts {
    if network.policy() != NetworkPolicy::Deny {
        return Vec::new();
    }
    network
        .allowed_domain_rules()
        .into_iter()
        .filter(|rule| rule.allow_private_ips && !rule.host.starts_with("*."))
        .filter_map(|rule| match (rule.host.as_str(), 0).to_socket_addrs() {
            Ok(addrs) => {
                let mut ips: Vec<IpAddr> = addrs.map(|addr| addr.ip()).collect();
                ips.sort();
                ips.dedup();
                Some((rule.host, ips))
            }
            Err(error) => {
                tracing::warn!(
                    host = %rule.host,
                    %error,
                    "could not resolve an allowed host with allow_private_ips; \
                     its private addresses stay blocked"
                );
                None
            }
        })
        .collect()
}
