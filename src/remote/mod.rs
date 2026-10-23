//! Remotes: project hosts other than the workstation, reached through a
//! transport and spoken to over a control channel (ADR 0001).

pub mod channel;
pub mod channels;
pub mod connection;
pub mod handshake;
pub mod registry;
pub mod router;
pub mod serve;
pub mod session;
pub mod ssh;
#[cfg(test)]
pub mod testing;
pub mod transport;

use anyhow::{Result, anyhow};
use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::config::RemoteConfig;
use channels::{ChannelPolicy, Channels, RemoteStatus};

/// The control channels of this process, when it is the workstation daemon
/// or a command running without one.
pub fn channels() -> &'static Channels {
    static CHANNELS: OnceLock<Channels> = OnceLock::new();
    CHANNELS.get_or_init(|| Channels::new(registry::transport_factory(), ChannelPolicy::default()))
}

/// Status of the remote `name`, or of every remote, checked in parallel.
pub fn statuses(
    channels: &Channels,
    remotes: &BTreeMap<String, RemoteConfig>,
    name: Option<&str>,
) -> Result<Vec<RemoteStatus>> {
    let selected: Vec<(&String, &RemoteConfig)> = match name {
        Some(name) => {
            let (name, remote) = remotes
                .get_key_value(name)
                .ok_or_else(|| anyhow!("no remote named {name}"))?;
            vec![(name, remote)]
        }
        None => remotes.iter().collect(),
    };
    Ok(std::thread::scope(|scope| {
        let checks: Vec<_> = selected
            .into_iter()
            .map(|(name, remote)| scope.spawn(move || channels.status(name, remote)))
            .collect();
        checks
            .into_iter()
            .map(|check| check.join().expect("status check panicked"))
            .collect()
    }))
}
