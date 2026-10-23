//! Remote registry: resolves remotes and their transports from global config.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::channels::TransportFactory;
use super::transport::{Loopback, Transport};
use crate::config::{Config, RemoteConfig, TransportKind};

/// The remotes registered in global config.
pub fn remotes() -> Result<BTreeMap<String, RemoteConfig>> {
    Ok(Config::load_global_only()?.remotes)
}

/// Build the transport that reaches `remote`.
pub fn transport(_name: &str, remote: &RemoteConfig) -> Result<Arc<dyn Transport>> {
    match remote.transport {
        TransportKind::Loopback => Ok(Arc::new(Loopback::new(crate::util::expand_tilde(
            &remote.target,
        ))?)),
        kind @ (TransportKind::Ssh | TransportKind::Openshell) => Err(anyhow!(
            "the {kind} transport is not supported by this workmux yet"
        )),
    }
}

pub fn transport_factory() -> TransportFactory {
    Arc::new(transport)
}

/// Check that `name` can name a remote.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("remote name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("remote name {name:?} may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(())
}
