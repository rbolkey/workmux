//! Per-sandbox acceptance of what sbx lets a `deny` sandbox reach beyond
//! `allowed_domains`, like Lima's reduced-isolation acknowledgement.

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::config::{Config, DockerSbxAgent};
use crate::sandbox::acknowledgement::{Acknowledgement, ask_on_stdin_interruptibly, can_ask};

use super::plan::Widening;

/// Return the acknowledgement file of sandbox `name`, beside Lima's.
pub(super) fn marker(name: &str) -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?
        .join("docker-sbx-acknowledgements")
        .join("wider-network-policy-v1")
        .join(name))
}

/// Check whether to ask after `sbx create` instead of before: kits add rules
/// at create, and asking once after seeing them needs a terminal.
pub(super) fn asks_after_create(config: &Config) -> bool {
    let sbx = &config.sandbox.docker_sbx;
    let kits_add_rules = sbx.agent() == DockerSbxAgent::Kit || !sbx.kits().is_empty();
    kits_add_rules && can_ask() && !sbx.accepts_wider_network_policy()
}

/// Have the user accept `widenings` for sandbox `name`, once per sandbox.
///
/// Accepts by `accept_wider_network_policy` or by asking on a terminal, and
/// fails otherwise. Ctrl-C at the question declines.
pub(super) fn acknowledge(config: &Config, name: &str, widenings: &[Widening]) -> Result<()> {
    if widenings.is_empty() {
        return Ok(());
    }
    let marker = marker(name)?;
    let warnings: Vec<String> = widenings.iter().map(|w| w.warning(name)).collect();
    let question = format!("Create sbx sandbox {name} with this wider network access?");
    let refused = format!(
        "sbx sandbox {name} would reach more than sandbox.network.allowed_domains under \
         sandbox.network.policy: deny, as the warnings above say, and creating it needs \
         your acceptance. Set sandbox.docker_sbx.accept_wider_network_policy: true in your \
         global config for unattended use, or narrow sbx's global policy with \
         `sbx policy init deny-all` (after `sbx policy reset`)."
    );
    Acknowledgement {
        marker: &marker,
        marker_name: "sbx wider network policy acknowledgement",
        warnings: &warnings,
        accepted_by_config: config.sandbox.docker_sbx.accepts_wider_network_policy(),
        question: &question,
        refused: &refused,
        declined: &format!(
            "Creating sbx sandbox {name} with this wider network access was declined"
        ),
    }
    .acknowledge(can_ask().then_some(ask_on_stdin_interruptibly))
}

/// Forget sandbox `name`'s acceptance at `marker`. Best effort: a failure is
/// logged.
pub(super) fn forget(name: &str, marker: &Path) {
    match std::fs::remove_file(marker) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            warn!(sandbox = %name, error = %e, "failed to remove sbx network acknowledgement")
        }
        _ => {}
    }
}
