//! `sandbox.network` as smolvm's per-machine network.

use anyhow::Result;

use crate::config::{NetworkConfig, NetworkPolicy};

/// A machine's network: its create flags, and notes for `tracing`.
#[derive(Debug, Default)]
pub struct MachineNetwork {
    pub flags: Vec<String>,
    pub notes: Vec<String>,
}

/// The create flags for `network`.
pub fn machine_network(network: &NetworkConfig) -> Result<MachineNetwork> {
    if network.policy() == NetworkPolicy::Deny {
        anyhow::bail!(
            "sandbox.network.policy: deny is not supported by the smolvm backend yet; \
             use another sandbox.backend"
        );
    }
    // virtio-net always: the guest reaches the RPC server through its
    // gateway, and its own 127.0.0.1 never falls through to the host's.
    let flags: Vec<String> = ["--net", "--net-backend", "virtio-net"]
        .map(String::from)
        .to_vec();
    Ok(MachineNetwork {
        flags,
        notes: Vec::new(),
    })
}
