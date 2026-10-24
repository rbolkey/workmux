//! Remote registry: resolves remotes and their transports from global config.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::channels::TransportFactory;
use super::openshell::OpenShell;
use super::ssh::Ssh;
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
        TransportKind::Ssh => Ok(Arc::new(
            Ssh::new(&remote.target).with_uploaded_workmux(remote.upload_workmux == Some(true)),
        )),
        TransportKind::Openshell => Ok(Arc::new(OpenShell::new(
            &remote.target,
            remote.openshell.clone(),
        ))),
    }
}

pub fn transport_factory() -> TransportFactory {
    Arc::new(transport)
}

/// Check that `name` can name a remote.
pub fn validate_name(name: &str) -> Result<()> {
    validate("remote", name)
}

/// Check that `name` can name a project registered in global config.
pub fn validate_project_name(name: &str) -> Result<()> {
    validate("project", name)
}

fn validate(noun: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{noun} name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("{noun} name {name:?} may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(())
}
