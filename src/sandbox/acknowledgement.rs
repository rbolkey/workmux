//! Acceptance of reduced git-metadata isolation, stored once per backend scope.

use anyhow::{Result, bail};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// What a backend asks the user to accept, and where the answer is kept.
pub(crate) struct GitIsolationAcknowledgement<'a> {
    /// Backend name used in errors, e.g. "Lima".
    pub backend: &'a str,
    /// Directory holding one acknowledgement file per scope key.
    pub state_dir: PathBuf,
    /// The scope the acceptance covers, e.g. a VM or project name.
    pub scope_key: &'a str,
    /// Warning printed before asking.
    pub warning: &'a str,
    /// Error when the user does not accept.
    pub refusal: &'a str,
    /// The backend's accept setting, which skips the prompt.
    pub accepted_by_config: bool,
}

const ACCEPTED: &str = "accepted-v1\n";

impl GitIsolationAcknowledgement<'_> {
    /// Ensure the user has accepted reduced git-metadata isolation for this scope.
    ///
    /// A stored acknowledgement must be a regular file with the expected content.
    /// Without one, the warning is printed and the accept setting or, on a
    /// terminal only, a `[y/N]` prompt decides; acceptance is then stored.
    pub(crate) fn acknowledge(&self) -> Result<()> {
        use std::io::IsTerminal;

        let stdin = std::io::stdin();
        let mut stdin_lock;
        let terminal: Option<&mut dyn BufRead> = if stdin.is_terminal() {
            stdin_lock = stdin.lock();
            Some(&mut stdin_lock)
        } else {
            None
        };
        self.acknowledge_with(terminal, &mut std::io::stderr())
    }

    pub(crate) fn acknowledge_with(
        &self,
        terminal: Option<&mut dyn BufRead>,
        stderr: &mut dyn Write,
    ) -> Result<()> {
        let acknowledgement = self.state_dir.join(self.scope_key);
        if let Ok(metadata) = std::fs::symlink_metadata(&acknowledgement) {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || !matches!(
                    std::fs::read_to_string(&acknowledgement).as_deref(),
                    Ok(ACCEPTED)
                )
            {
                bail!(
                    "Invalid {} reduced-isolation acknowledgement at {}",
                    self.backend,
                    acknowledgement.display()
                );
            }
            return Ok(());
        }

        writeln!(stderr, "{}", self.warning)?;

        let accepted = if self.accepted_by_config {
            true
        } else if let Some(terminal) = terminal {
            write!(
                stderr,
                "Continue with reduced Git metadata isolation? [y/N] "
            )?;
            stderr.flush()?;
            let mut input = String::new();
            terminal.read_line(&mut input)?;
            input.trim().eq_ignore_ascii_case("y")
        } else {
            false
        };

        if !accepted {
            bail!("{}", self.refusal);
        }

        std::fs::create_dir_all(&self.state_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(
        state_dir: &std::path::Path,
        accepted_by_config: bool,
    ) -> GitIsolationAcknowledgement<'static> {
        GitIsolationAcknowledgement {
            backend: "Lima",
            state_dir: state_dir.to_path_buf(),
            scope_key: "wm-project-abc",
            warning: "warning: reduced isolation",
            refusal: "Lima requires explicit acceptance",
            accepted_by_config,
        }
    }

    #[test]
    fn test_stored_acknowledgement_is_honoured_without_prompting() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("wm-project-abc"), "accepted-v1\n").unwrap();
        let mut stderr = Vec::new();

        ack(temp.path(), false)
            .acknowledge_with(None, &mut stderr)
            .unwrap();

        assert!(stderr.is_empty());
    }

    fn assert_invalid(temp: &tempfile::TempDir) {
        let mut stderr = Vec::new();
        let error = ack(temp.path(), true)
            .acknowledge_with(None, &mut stderr)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid Lima reduced-isolation acknowledgement at {}",
                temp.path().join("wm-project-abc").display()
            )
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn test_stored_acknowledgement_with_other_content_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("wm-project-abc"), "accepted\n").unwrap();
        assert_invalid(&temp);
    }

    #[test]
    fn test_stored_acknowledgement_that_is_a_directory_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("wm-project-abc")).unwrap();
        assert_invalid(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn test_stored_acknowledgement_that_is_a_symlink_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("elsewhere");
        std::fs::write(&target, "accepted-v1\n").unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("wm-project-abc")).unwrap();
        assert_invalid(&temp);
    }

    fn stored(state_dir: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(state_dir.join("wm-project-abc")).ok()
    }

    #[test]
    fn test_accept_setting_skips_the_prompt_and_stores_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("acks");
        let mut terminal: &[u8] = b"n\n";
        let mut stderr = Vec::new();

        ack(&state_dir, true)
            .acknowledge_with(Some(&mut terminal), &mut stderr)
            .unwrap();

        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "warning: reduced isolation\n"
        );
        assert_eq!(stored(&state_dir).as_deref(), Some("accepted-v1\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&state_dir), 0o700);
            assert_eq!(mode(&state_dir.join("wm-project-abc")), 0o600);
        }
    }

    #[test]
    fn test_yes_on_the_terminal_stores_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let mut terminal: &[u8] = b" Y \n";
        let mut stderr = Vec::new();

        ack(temp.path(), false)
            .acknowledge_with(Some(&mut terminal), &mut stderr)
            .unwrap();

        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "warning: reduced isolation\nContinue with reduced Git metadata isolation? [y/N] "
        );
        assert_eq!(stored(temp.path()).as_deref(), Some("accepted-v1\n"));
    }

    #[test]
    fn test_anything_but_yes_on_the_terminal_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let mut terminal: &[u8] = b"\n";
        let mut stderr = Vec::new();

        let error = ack(temp.path(), false)
            .acknowledge_with(Some(&mut terminal), &mut stderr)
            .unwrap_err();

        assert_eq!(error.to_string(), "Lima requires explicit acceptance");
        assert_eq!(stored(temp.path()), None);
    }

    #[test]
    fn test_without_a_terminal_it_refuses_without_prompting() {
        let temp = tempfile::tempdir().unwrap();
        let mut stderr = Vec::new();

        let error = ack(temp.path(), false)
            .acknowledge_with(None, &mut stderr)
            .unwrap_err();

        assert_eq!(error.to_string(), "Lima requires explicit acceptance");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "warning: reduced isolation\n"
        );
        assert_eq!(stored(temp.path()), None);
    }
}
