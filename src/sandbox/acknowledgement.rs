//! Per-sandbox acceptance of weaker isolation, asked once and recorded in a
//! marker file.

use anyhow::{Result, bail};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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
    /// The error when it can't be accepted without a terminal.
    pub refused: &'a str,
    /// The error when the user answers no.
    pub declined: &'a str,
}

impl Acknowledgement<'_> {
    /// Accept by an existing marker, the config or `ask`, and record it.
    ///
    /// `ask` is `None` without a terminal to ask on. An interrupted answer
    /// counts as no.
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
            let accepted = match ask(self.question) {
                Ok(accepted) => accepted,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    eprintln!();
                    false
                }
                Err(e) => return Err(e.into()),
            };
            if !accepted {
                bail!("{}", self.declined);
            }
            true
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

/// Like [`ask_on_stdin`], but Ctrl-C returns an `Interrupted` error instead
/// of killing workmux.
pub(super) fn ask_on_stdin_interruptibly(question: &str) -> io::Result<bool> {
    let flags = interrupt_flags()?;
    flags.interrupted.store(false, Ordering::SeqCst);
    flags.idle.store(false, Ordering::SeqCst);
    eprint!("{question} [y/N] ");
    let answer = io::stderr()
        .flush()
        .and_then(|()| read_answer(&mut io::stdin(), &flags.interrupted));
    flags.idle.store(true, Ordering::SeqCst);
    answer
}

struct InterruptFlags {
    /// Set by SIGINT.
    interrupted: Arc<AtomicBool>,
    /// While set, SIGINT terminates as it would without a handler.
    idle: Arc<AtomicBool>,
}

/// Install the SIGINT handlers once; they leave SIGINT's default action in
/// place outside a prompt.
fn interrupt_flags() -> io::Result<&'static InterruptFlags> {
    static FLAGS: OnceLock<InterruptFlags> = OnceLock::new();
    if let Some(flags) = FLAGS.get() {
        return Ok(flags);
    }
    let flags = InterruptFlags {
        interrupted: Arc::new(AtomicBool::new(false)),
        idle: Arc::new(AtomicBool::new(true)),
    };
    signal_hook::flag::register_conditional_default(libc::SIGINT, Arc::clone(&flags.idle))?;
    signal_hook::flag::register(libc::SIGINT, Arc::clone(&flags.interrupted))?;
    Ok(FLAGS.get_or_init(|| flags))
}

/// Read one line from `input` and return whether it's `y`, failing with
/// `Interrupted` once `interrupted` is set.
fn read_answer(input: &mut (impl Read + AsFd), interrupted: &AtomicBool) -> io::Result<bool> {
    let mut line = Vec::new();
    // As large as stdin's buffer, so a read never leaves input behind it.
    let mut buf = [0u8; 8192];
    loop {
        if interrupted.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if !readable(input.as_fd().as_raw_fd(), Duration::from_millis(100))? {
            continue;
        }
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                line.extend_from_slice(&buf[..n]);
                if line.contains(&b'\n') {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let line = String::from_utf8_lossy(&line);
    Ok(line
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("y"))
}

/// Wait up to `timeout` for `fd` to be readable. A signal ends the wait early.
fn readable(fd: std::os::fd::RawFd, timeout: Duration) -> io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` is a valid array of one for the call's duration.
    let ready = unsafe { libc::poll(&mut pollfd, 1, timeout.as_millis() as libc::c_int) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(ready > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn acknowledgement(marker: &Path) -> Acknowledgement<'_> {
        Acknowledgement {
            marker,
            marker_name: "test acknowledgement",
            warnings: &[],
            accepted_by_config: false,
            question: "Continue?",
            refused: "needs acceptance",
            declined: "declined",
        }
    }

    #[test]
    fn an_interrupted_answer_declines_and_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("acks/sandbox");

        let error = acknowledgement(&marker)
            .acknowledge(Some(|_: &str| Err(io::ErrorKind::Interrupted.into())))
            .unwrap_err();

        assert_eq!(error.to_string(), "declined");
        assert!(!marker.exists());
    }

    #[test]
    fn a_yes_is_recorded_and_not_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("acks/sandbox");

        acknowledgement(&marker)
            .acknowledge(Some(|_: &str| Ok(true)))
            .unwrap();
        acknowledgement(&marker)
            .acknowledge(None::<fn(&str) -> io::Result<bool>>)
            .unwrap();

        assert_eq!(std::fs::read_to_string(&marker).unwrap(), ACCEPTED);
    }

    #[test]
    fn the_prompt_reads_one_answer_or_stops_at_an_interrupt() {
        let not_interrupted = AtomicBool::new(false);
        for (input, expected) in [("y\n", true), (" Y \n", true), ("n\n", false), ("", false)] {
            let (mut reader, mut writer) = io::pipe().unwrap();
            writer.write_all(input.as_bytes()).unwrap();
            drop(writer);
            assert_eq!(
                read_answer(&mut reader, &not_interrupted).unwrap(),
                expected,
                "{input:?}"
            );
        }

        // Ctrl-C while nothing has been typed.
        let (mut reader, _writer) = io::pipe().unwrap();
        let error = read_answer(&mut reader, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
