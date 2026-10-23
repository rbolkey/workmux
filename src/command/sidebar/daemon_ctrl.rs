//! Workstation daemon lifecycle as the sidebar uses it: start, kill, signal.

use anyhow::Result;
use std::path::PathBuf;

use crate::cmd::Cmd;
use crate::multiplexer::{Multiplexer, TmuxBackend, create_backend, detect_backend};

/// Ensure the workstation daemon is running, spawning it if needed.
/// Returns the socket path.
pub(super) fn ensure_daemon_running() -> Result<PathBuf> {
    crate::workstation_daemon::ensure_running()
}

/// Read the daemon PID from the tmux global option.
fn daemon_pid() -> Option<String> {
    Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_daemon_pid"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Kill the workstation daemon (sends SIGTERM, cleans up tmux option).
pub(super) fn kill_daemon() {
    if let Some(pid) = daemon_pid() {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-gu", "@workmux_sidebar_daemon_pid"])
        .run();
}

fn signal_pid(pid: &str) {
    let _ = std::process::Command::new("kill")
        .args(["-USR1", pid])
        .stderr(std::process::Stdio::null())
        .status();
}

pub(super) fn signal_daemon_for(mux: &dyn Multiplexer) {
    if mux.name() == "tmux" {
        let tmux = TmuxBackend::for_socket(&mux.instance_id());
        if let Ok(Some(pid)) = tmux.global_option("@workmux_sidebar_daemon_pid") {
            signal_pid(&pid);
        }
    }
}

/// Signal the workstation daemon to do an immediate refresh, bypassing tmux hook latency.
pub(super) fn signal_daemon() {
    let mux = create_backend(detect_backend());
    if mux.name() == "tmux" {
        signal_daemon_for(mux.as_ref());
    } else if let Some(pid) = daemon_pid() {
        signal_pid(&pid);
    }
}
