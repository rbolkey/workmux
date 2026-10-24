//! `workmux remote attach`: the loop a workstation pane runs to show a
//! remote pane.
//!
//! It attaches through the transport's TTY exec. When the attachment ends it
//! asks the remote whether the remote pane has ended: if so the loop exits
//! and the workstation pane closes as it would locally; otherwise the
//! connection dropped, and the loop shows a reconnecting screen and retries
//! with backoff. While it runs, the workstation daemon keeps the remote's
//! control channel open.

use anyhow::{Result, anyhow};
use std::io::{Read, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::pane::{ATTACHED, PaneState, RemotePaneId};
use super::registry;
use super::transport::{Transport, TtyExec};

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// An attachment that lasted this long was a working connection: the next
/// drop starts the backoff over.
const STEADY: Duration = Duration::from_secs(10);

/// What the user chose on the reconnecting screen.
#[derive(Debug, PartialEq, Eq)]
enum Choice {
    Retry,
    Close,
}

/// Show remote pane `pane` on `remote` in this terminal until it ends or the
/// user closes the view.
pub fn run(remote: &str, pane: &RemotePaneId) -> Result<()> {
    let remotes = registry::remotes()?;
    let config = remotes
        .get(remote)
        .ok_or_else(|| anyhow!("no remote named {remote}"))?;
    let transport = registry::transport(remote, config)?;
    hold_channel(remote);

    let mut backoff = RETRY_INITIAL;
    loop {
        let started = Instant::now();
        let attached = attach_once(transport.as_ref(), pane);
        let reason = match (attached, pane_state(transport.as_ref(), pane)) {
            (_, Ok(PaneState::Ended)) => return Ok(()),
            (Err(error), _) | (Ok(()), Err(error)) => format!("{error:#}"),
            (Ok(()), Ok(PaneState::Running)) => format!("the connection to {remote} dropped"),
        };
        if started.elapsed() >= STEADY {
            backoff = RETRY_INITIAL;
        }
        if reconnecting_screen(remote, &reason, backoff) == Choice::Close {
            return Ok(());
        }
        clear_screen();
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

/// Attach this terminal to `pane` until the attachment ends.
fn attach_once(transport: &dyn Transport, pane: &RemotePaneId) -> Result<()> {
    let mut command = transport.tty_command(&TtyExec {
        argv: ["workmux", "remote", "pane-attach", &pane.to_string()]
            .map(String::from)
            .to_vec(),
        workdir: None,
        env: Vec::new(),
    })?;
    // The attachment's output passes through this process, which forces a
    // redraw once the session holder has attached it: some interfaces
    // (Claude Code) only repaint on a real size change, and the change must
    // reach them after the attach, however long connecting takes.
    command.stdout(Stdio::piped());
    let mut child = command.spawn()?;
    let output = child.stdout.take().expect("stdout is piped");
    let relayed = std::thread::spawn(move || {
        relay(output, std::io::stdout(), || {
            std::thread::spawn(force_resize);
        })
    });
    let status = child.wait();
    let _ = relayed.join();
    status?;
    Ok(())
}

/// Shrink this terminal by a row, then by two, and restore it, so the remote
/// pane's process sees a real size change and repaints. A dtach client that
/// has only just attached can hold the first change back until something
/// else wakes it, and then reports the size as it is by then; the second
/// shrink is that wake, and still a change.
fn force_resize() {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ and TIOCSWINSZ read and write a winsize.
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } != 0
        || size.ws_row < 3
    {
        return;
    }
    for shrink in [1, 2, 0] {
        let mut resized = size;
        resized.ws_row -= shrink;
        unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCSWINSZ, &resized) };
        if shrink != 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Ask the remote whether `pane` still runs. An error means the remote
/// could not be asked.
fn pane_state(transport: &dyn Transport, pane: &RemotePaneId) -> Result<PaneState> {
    let output = transport
        .exec_captured(&["workmux", "remote", "pane-state", &pane.to_string()].map(String::from))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr)
            .trim()
            .replace("\r\n", "\n");
        return Err(if stderr.is_empty() {
            anyhow!("checking the remote pane failed with {}", output.status)
        } else {
            anyhow!(stderr)
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .parse()
}

/// Keep `remote`'s control channel open for as long as this process runs,
/// through the workstation daemon, holding again whenever the daemon
/// restarts. Without a daemon there is no channel to keep.
fn hold_channel(remote: &str) {
    let remote = remote.to_string();
    std::thread::spawn(move || {
        loop {
            let Some(socket) = super::router::daemon_socket() else {
                return;
            };
            match crate::workstation_daemon::hold_for_pane(&socket, &remote) {
                Ok(stream) => {
                    // Held until the daemon goes away.
                    let _ = stream.set_read_timeout(None);
                    let _ = std::io::copy(&mut &stream, &mut std::io::sink());
                    std::thread::sleep(RETRY_INITIAL);
                }
                Err(error) => {
                    tracing::info!(remote, error = %format!("{error:#}"), "no control channel hold");
                    std::thread::sleep(RETRY_MAX);
                }
            }
        }
    });
}

/// Leave whatever modes the remote pane's interface left on, and clear.
fn clear_screen() {
    let mut out = std::io::stdout();
    // Main screen, cursor shown, mouse reporting and bracketed paste off.
    let _ = out.write_all(
        b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[2J\x1b[H",
    );
    let _ = out.flush();
}

/// Show that the connection to `remote` is being retried, for `wait` or
/// until the user retries (r) or closes (q).
fn reconnecting_screen(remote: &str, reason: &str, wait: Duration) -> Choice {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    clear_screen();
    print!("reconnecting to {remote}… (r: retry now, q: close)\r\n\r\n");
    for line in reason.lines() {
        print!("{line}\r\n");
    }
    let _ = std::io::stdout().flush();

    if crossterm::terminal::enable_raw_mode().is_err() {
        std::thread::sleep(wait);
        return Choice::Retry;
    }
    let deadline = Instant::now() + wait;
    let choice = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Choice::Retry;
        }
        match event::poll(left) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('r' | 'R') => break Choice::Retry,
                    KeyCode::Char('q' | 'Q') => break Choice::Close,
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => break Choice::Retry,
            },
            Ok(false) => break Choice::Retry,
            Err(_) => {
                std::thread::sleep(left);
                break Choice::Retry;
            }
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    choice
}

/// Copy an attachment's output from `from` to `to`, calling `attached` once
/// the session holder says the client is attached.
fn relay(
    mut from: impl Read,
    mut to: impl Write,
    attached: impl FnOnce(),
) -> std::io::Result<()> {
    let mut attached = Some(attached);
    // The end of the output so far, to spot the marker across reads.
    let mut tail = Vec::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let read = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(read) => &buf[..read],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(read)?;
        to.flush()?;
        if attached.is_none() {
            continue;
        }
        tail.extend_from_slice(read);
        if tail.windows(ATTACHED.len()).any(|window| window == ATTACHED) {
            attached.take().expect("not yet attached")();
        } else {
            tail.drain(..tail.len().saturating_sub(ATTACHED.len() - 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out one chunk per read.
    struct Chunks(std::collections::VecDeque<Vec<u8>>);

    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some(chunk) = self.0.pop_front() else {
                return Ok(0);
            };
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn relay_passes_output_through_and_notices_the_attach_once() {
        let chunks = [
            &b"motd\r\n\x1b[H"[..],
            b"\x1b[J",
            b"interface\x1b[H\x1b[J",
        ];
        let mut out = Vec::new();
        let mut attached = 0;

        relay(
            Chunks(chunks.iter().map(|c| c.to_vec()).collect()),
            &mut out,
            || attached += 1,
        )
        .unwrap();

        assert_eq!(out, b"motd\r\n\x1b[H\x1b[Jinterface\x1b[H\x1b[J");
        assert_eq!(attached, 1);
    }

    #[test]
    fn relay_without_an_attach_never_calls_it_attached() {
        let mut attached = false;

        relay(&b"dtach: No such file or directory\r\n"[..], std::io::sink(), || {
            attached = true
        })
        .unwrap();

        assert!(!attached);
    }
}
