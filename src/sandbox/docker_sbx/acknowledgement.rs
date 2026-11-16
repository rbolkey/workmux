//! The user's acceptance, once per sandbox, of what sbx lets a sandbox under
//! `policy: deny` reach beyond `allowed_domains`, as Lima asks to accept its
//! reduced Git metadata isolation.

use anyhow::{Result, bail};
use std::path::PathBuf;
use tracing::warn;

use crate::config::Config;

use super::plan::Widening;

/// What an acknowledgement file holds.
const ACCEPTED: &str = "accepted-v1\n";

/// Directory of the acknowledgement files, one per sandbox, beside Lima's.
fn acknowledgement_dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?
        .join("docker-sbx-acknowledgements")
        .join("wider-network-policy-v1"))
}

/// Whether stdin is a terminal the user can answer from.
pub(super) fn can_ask() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Have the user accept `widenings` for sandbox `name`, unless they did when
/// it was created: print a `warning:` for each, then accept by
/// `sandbox.docker_sbx.accept_wider_network_policy`, or by asking when
/// stdin is a terminal, and record the acceptance. Fails when it isn't
/// accepted.
pub(super) fn acknowledge(config: &Config, name: &str, widenings: &[Widening]) -> Result<()> {
    use std::io::Write;

    if widenings.is_empty() {
        return Ok(());
    }
    let state_dir = acknowledgement_dir()?;
    let acknowledgement = state_dir.join(name);
    if let Ok(metadata) = std::fs::symlink_metadata(&acknowledgement) {
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || std::fs::read_to_string(&acknowledgement).as_deref().ok() != Some(ACCEPTED)
        {
            bail!(
                "Invalid sbx wider network policy acknowledgement at {}",
                acknowledgement.display()
            );
        }
        return Ok(());
    }

    for widening in widenings {
        eprintln!("warning: {}", widening.warning(name));
    }
    let accepted = if config.sandbox.docker_sbx.accepts_wider_network_policy() {
        true
    } else if can_ask() {
        eprint!("Create sbx sandbox {name} with this wider network access? [y/N] ");
        std::io::stderr().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        input.trim().eq_ignore_ascii_case("y")
    } else {
        false
    };
    if !accepted {
        bail!(
            "sbx sandbox {name} would reach more than sandbox.network.allowed_domains under \
             sandbox.network.policy: deny, as the warnings above say, and creating it needs \
             your acceptance. Set sandbox.docker_sbx.accept_wider_network_policy: true in your \
             global config for unattended use, or narrow sbx's global policy with \
             `sbx policy init deny-all` (after `sbx policy reset`)."
        );
    }

    std::fs::create_dir_all(&state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&acknowledgement)?;
        file.write_all(ACCEPTED.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&acknowledgement)?
        .write_all(ACCEPTED.as_bytes())?;
    Ok(())
}

/// Forget the acceptance for sandbox `name`, as it's removed, so a sandbox
/// created again under that name asks again. Best effort: a failure is
/// logged.
pub(crate) fn remove_acknowledgement(name: &str) {
    let Ok(dir) = acknowledgement_dir() else {
        return;
    };
    match std::fs::remove_file(dir.join(name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            warn!(sandbox = %name, error = %e, "failed to remove sbx network acknowledgement")
        }
        _ => {}
    }
}
