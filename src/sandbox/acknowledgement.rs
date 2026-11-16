//! Per-sandbox acceptance of weaker isolation, asked once and recorded in a
//! marker file.

use anyhow::{Result, bail};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

/// What a marker holds.
const ACCEPTED: &str = "accepted-v1\n";

/// One backend's acknowledgement for one sandbox.
pub(super) struct Acknowledgement<'a> {
    /// The marker recording the acceptance.
    pub marker: &'a Path,
    /// What the marker is, for the error about an invalid one.
    pub marker_name: &'a str,
    /// Printed as `warning: ...` lines before deciding.
    pub warnings: &'a [String],
    /// Whether the user's config accepts it unattended.
    pub accepted_by_config: bool,
    /// The `[y/N]` question.
    pub question: &'a str,
    /// The error when it isn't accepted.
    pub refused: &'a str,
}

impl Acknowledgement<'_> {
    /// Accept by an existing marker, the config or `ask`, and record it.
    ///
    /// `ask` is `None` without a terminal to ask on.
    pub(super) fn acknowledge(
        &self,
        ask: Option<impl FnOnce(&str) -> io::Result<bool>>,
    ) -> Result<()> {
        let marker = self.marker;
        if let Ok(metadata) = std::fs::symlink_metadata(marker) {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || std::fs::read_to_string(marker).as_deref().ok() != Some(ACCEPTED)
            {
                bail!("Invalid {} at {}", self.marker_name, marker.display());
            }
            return Ok(());
        }

        for warning in self.warnings {
            eprintln!("warning: {warning}");
        }
        let accepted = if self.accepted_by_config {
            true
        } else if let Some(ask) = ask {
            ask(self.question)?
        } else {
            false
        };
        if !accepted {
            bail!("{}", self.refused);
        }

        let state_dir = marker.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(state_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(marker)?;
            file.write_all(ACCEPTED.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)?
            .write_all(ACCEPTED.as_bytes())?;
        Ok(())
    }
}

/// Check whether stdin is a terminal the user can answer from.
pub(super) fn can_ask() -> bool {
    io::stdin().is_terminal()
}

/// Ask `question` on stdin and return whether the answer is `y`.
pub(super) fn ask_on_stdin(question: &str) -> io::Result<bool> {
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}
