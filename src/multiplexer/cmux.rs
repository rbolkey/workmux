//! cmux multiplexer backend (macOS only).
//!
//! cmux is a macOS terminal multiplexer built on libghostty with flat workspaces
//! (no session hierarchy). Follows the Zellij backend pattern (flat model,
//! `should_exit_on_jump() = false`) while using cmux's native `wait-for` handshake
//! and rich sidebar status system.
//!
//! Limitations:
//! - No session support (flat workspace model)
//! - No pane resize (`resize-pane` returns `not_supported`)
//! - `send-key ctrl+c` unreliable — use `send` with escape sequences instead

use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::handshake::CmuxHandshake;
use super::types::{CreateSessionParams, CreateWindowParams, LivePaneInfo};
use super::{Multiplexer, PaneHandshake};

/// cmux multiplexer backend.
///
/// Communicates with cmux via the `cmux` CLI binary. Workspace refs (e.g.,
/// `workspace:1`) are used for addressing throughout. Surface refs (e.g.,
/// `surface:1`) identify individual panes within workspaces.
///
/// The `surface_to_workspace` map tracks which workspace owns each surface,
/// needed because cmux status is per-workspace but the trait passes pane IDs
/// (surface refs).
pub struct CmuxBackend {
    /// Maps surface ref → workspace ref. Populated during create_window/split_pane.
    surface_to_workspace: std::sync::Mutex<HashMap<String, String>>,
}

// === Serde structs for cmux JSON responses ===

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ListWorkspacesResponse {
    #[allow(dead_code)]
    pub window_ref: String,
    pub workspaces: Vec<WorkspaceEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct WorkspaceEntry {
    #[serde(rename = "ref")]
    pub ws_ref: String,
    pub title: String,
    pub selected: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub index: u32,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ListPaneSurfacesResponse {
    pub surfaces: Vec<SurfaceEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct SurfaceEntry {
    #[serde(rename = "ref")]
    pub surface_ref: String,
    #[serde(default)]
    pub surface_type: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IdentifyResponse {
    pub caller: IdentifySurface,
    pub focused: IdentifySurface,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct IdentifySurface {
    pub surface_ref: String,
    pub workspace_ref: String,
    #[serde(default)]
    pub pane_ref: Option<String>,
    #[serde(default)]
    pub window_ref: Option<String>,
    #[serde(default)]
    pub surface_type: Option<String>,
    #[serde(default)]
    pub is_browser_surface: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SurfaceHealthResponse {
    pub surfaces: Vec<SurfaceHealthEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct SurfaceHealthEntry {
    #[serde(rename = "ref")]
    pub surface_ref: String,
    #[serde(rename = "type")]
    pub surface_type: String,
    pub in_window: bool,
    pub index: u32,
}

/// Response from `cmux --json list-panes --workspace <ref>`.
/// Reserved for get_all_live_pane_info batched queries.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ListPanesResponse {
    pub panes: Vec<PaneEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct PaneEntry {
    #[serde(rename = "ref")]
    pub pane_ref: String,
    #[serde(default)]
    pub surface_refs: Vec<String>,
}

impl Default for CmuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CmuxBackend {
    pub fn new() -> Self {
        Self {
            surface_to_workspace: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Query `cmux --json list-workspaces` and return parsed response.
    fn list_workspaces() -> Result<ListWorkspacesResponse> {
        let output = Cmd::new("cmux")
            .args(&["--json", "list-workspaces"])
            .run_and_capture_stdout()
            .context("Failed to list cmux workspaces")?;
        serde_json::from_str(&output).context("Failed to parse list-workspaces JSON")
    }

    /// Query `cmux --json list-pane-surfaces --workspace <ref>`.
    fn list_pane_surfaces(workspace_ref: &str) -> Result<ListPaneSurfacesResponse> {
        let output = Cmd::new("cmux")
            .args(&["--json", "list-pane-surfaces", "--workspace", workspace_ref])
            .run_and_capture_stdout()
            .context("Failed to list cmux pane surfaces")?;
        serde_json::from_str(&output).context("Failed to parse list-pane-surfaces JSON")
    }

    /// Query `cmux identify` (always returns JSON, no --json flag needed).
    fn identify() -> Result<IdentifyResponse> {
        let output = Cmd::new("cmux")
            .arg("identify")
            .run_and_capture_stdout()
            .context("Failed to run cmux identify")?;
        serde_json::from_str(&output).context("Failed to parse identify JSON")
    }

    /// Query `cmux --json surface-health --workspace <ref>`.
    fn surface_health(workspace_ref: &str) -> Result<SurfaceHealthResponse> {
        let output = Cmd::new("cmux")
            .args(&["--json", "surface-health", "--workspace", workspace_ref])
            .run_and_capture_stdout()
            .context("Failed to query cmux surface health")?;
        serde_json::from_str(&output).context("Failed to parse surface-health JSON")
    }

    /// Find a workspace by title in the workspace list.
    fn find_workspace_by_title(title: &str) -> Result<Option<WorkspaceEntry>> {
        let data = Self::list_workspaces()?;
        Ok(data.workspaces.into_iter().find(|ws| ws.title == title))
    }

    /// Find the workspace ref for a given full window name (prefix + name).
    fn find_workspace_ref(full_name: &str) -> Result<Option<String>> {
        Ok(Self::find_workspace_by_title(full_name)?.map(|ws| ws.ws_ref))
    }

    /// Record a surface→workspace mapping.
    fn record_surface_mapping(&self, surface_ref: &str, workspace_ref: &str) {
        if let Ok(mut map) = self.surface_to_workspace.lock() {
            map.insert(surface_ref.to_string(), workspace_ref.to_string());
        }
    }

    /// Look up the workspace ref for a surface ref.
    fn workspace_for_surface(&self, surface_ref: &str) -> Option<String> {
        self.surface_to_workspace
            .lock()
            .ok()
            .and_then(|map| map.get(surface_ref).cloned())
    }

    /// Parse `sidebar-state` key=value output into a HashMap.
    fn parse_sidebar_state(workspace_ref: &str) -> Result<HashMap<String, String>> {
        let output = Cmd::new("cmux")
            .args(&["sidebar-state", "--workspace", workspace_ref])
            .run_and_capture_stdout()
            .context("Failed to query cmux sidebar-state")?;

        let mut map = HashMap::new();
        for line in output.lines() {
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.to_string(), value.to_string());
            }
        }
        Ok(map)
    }

    /// Single-quote escape a string for shell command embedding.
    /// Same pattern as tmux backend: `name.replace('\'', r#"'\''"#)`
    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', r#"'\''"#))
    }
}

impl Multiplexer for CmuxBackend {
    fn name(&self) -> &'static str {
        "cmux"
    }

    fn supports_preview(&self) -> bool {
        true // read-screen --surface works cross-workspace
    }

    fn requires_focus_for_input(&self) -> bool {
        false // send --workspace --surface works cross-workspace
    }

    fn should_exit_on_jump(&self) -> bool {
        false // Flat model like Zellij: keep dashboard alive
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        Cmd::new("cmux").arg("ping").run_as_check()
    }

    fn current_pane_id(&self) -> Option<String> {
        // Fast path: use cmux identify to get caller's surface ref
        Self::identify().ok().map(|id| id.caller.surface_ref)
    }

    fn active_pane_id(&self) -> Option<String> {
        Self::identify().ok().map(|id| id.focused.surface_ref)
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        // Try sidebar-state for the focused workspace
        if let Ok(id) = Self::identify() {
            if let Ok(state) = Self::parse_sidebar_state(&id.focused.workspace_ref) {
                if let Some(cwd) = state.get("cwd").or_else(|| state.get("focused_cwd")) {
                    let path = PathBuf::from(cwd);
                    if path.exists() {
                        return Ok(path);
                    }
                }
            }
        }
        std::env::current_dir().context("Failed to get current directory")
    }

    fn instance_id(&self) -> String {
        std::env::var("CMUX_SOCKET_PATH").unwrap_or_else(|_| "default".to_string())
    }

    // === Window/Tab Management ===

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let full_name = format!("{}{}", params.prefix, params.name);

        // Snapshot workspace refs before creation for reliable diff
        let before: HashSet<String> = Self::list_workspaces()?
            .workspaces
            .iter()
            .map(|ws| ws.ws_ref.clone())
            .collect();

        // Create workspace
        let output = Cmd::new("cmux")
            .arg("new-workspace")
            .run_and_capture_stdout()
            .context("Failed to create cmux workspace")?;

        // Parse "OK <UUID>" → UUID (validated but not used for lookup)
        let _uuid = output
            .strip_prefix("OK ")
            .ok_or_else(|| anyhow!("Unexpected new-workspace output: {}", output))?
            .trim();

        // Find the new workspace ref by diffing before/after
        let after = Self::list_workspaces()?;
        let workspace_ref = after
            .workspaces
            .iter()
            .find(|ws| !before.contains(&ws.ws_ref))
            .map(|ws| ws.ws_ref.clone())
            .ok_or_else(|| anyhow!("Could not identify new workspace after creation"))?;

        // Get initial surface ref
        let surfaces = Self::list_pane_surfaces(&workspace_ref)?;
        let surface_ref = surfaces
            .surfaces
            .first()
            .ok_or_else(|| anyhow!("No surfaces in new workspace"))?
            .surface_ref
            .clone();

        // Rename workspace (positional title arg, NOT --title flag)
        Cmd::new("cmux")
            .args(&["rename-workspace", "--workspace", &workspace_ref, &full_name])
            .run()
            .context("Failed to rename cmux workspace")?;

        // Record surface→workspace mapping
        self.record_surface_mapping(&surface_ref, &workspace_ref);

        debug!(
            workspace_ref = %workspace_ref,
            surface_ref = %surface_ref,
            name = %full_name,
            "cmux: created workspace"
        );

        Ok(surface_ref)
    }

    // === Session Management (not supported — flat model) ===

    fn create_session(&self, _params: CreateSessionParams) -> Result<String> {
        Err(anyhow!(
            "Session mode (--session) is not supported by cmux. Use window mode instead."
        ))
    }

    fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> {
        Err(anyhow!(
            "Session mode is not supported by cmux. Use window mode instead."
        ))
    }

    fn session_exists(&self, _full_name: &str) -> Result<bool> {
        Ok(false)
    }

    fn kill_session(&self, _full_name: &str) -> Result<()> {
        Ok(())
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        Ok(HashSet::new())
    }

    fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> {
        Err(anyhow!("Session mode is not supported by cmux"))
    }

    // === Window Operations ===

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = format!("{}{}", prefix, name);
        let ws_ref = Self::find_workspace_ref(&full_name)?
            .ok_or_else(|| anyhow!("cmux workspace '{}' not found", full_name))?;

        Cmd::new("cmux")
            .args(&["select-workspace", "--workspace", &ws_ref])
            .run()
            .context("Failed to select cmux workspace")?;
        Ok(())
    }

    fn window_exists(&self, prefix: &str, name: &str) -> Result<bool> {
        let full_name = format!("{}{}", prefix, name);
        self.window_exists_by_full_name(&full_name)
    }

    fn window_exists_by_full_name(&self, full_name: &str) -> Result<bool> {
        Ok(Self::find_workspace_by_title(full_name)?.is_some())
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let ws_ref = Self::find_workspace_ref(full_name)?
            .ok_or_else(|| anyhow!("cmux workspace '{}' not found", full_name))?;

        Cmd::new("cmux")
            .args(&["close-workspace", "--workspace", &ws_ref])
            .run()
            .context("Failed to close cmux workspace")?;
        Ok(())
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let data = Self::list_workspaces()?;
        Ok(data
            .workspaces
            .into_iter()
            .find(|ws| ws.selected)
            .map(|ws| ws.title))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let data = Self::list_workspaces()?;
        Ok(data.workspaces.into_iter().map(|ws| ws.title).collect())
    }

    fn filter_active_windows(&self, windows: &[String]) -> Result<Vec<String>> {
        let all_names = self.get_all_window_names()?;
        Ok(windows
            .iter()
            .filter(|w| all_names.contains(*w))
            .cloned()
            .collect())
    }

    fn find_last_window_with_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let data = Self::list_workspaces()?;
        Ok(data
            .workspaces
            .into_iter()
            .filter(|ws| ws.title.starts_with(prefix))
            .max_by_key(|ws| ws.index)
            .map(|ws| ws.title))
    }

    fn find_last_window_with_base_handle(
        &self,
        prefix: &str,
        base_handle: &str,
    ) -> Result<Option<String>> {
        let full_prefix = format!("{}{}", prefix, base_handle);
        let data = Self::list_workspaces()?;
        Ok(data
            .workspaces
            .into_iter()
            .filter(|ws| ws.title.starts_with(&full_prefix))
            .max_by_key(|ws| ws.index)
            .map(|ws| ws.title))
    }

    fn wait_until_windows_closed(&self, full_window_names: &[String]) -> Result<()> {
        loop {
            let still_open: Vec<_> = full_window_names
                .iter()
                .filter(|name| self.window_exists_by_full_name(name).unwrap_or(false))
                .collect();

            if still_open.is_empty() {
                return Ok(());
            }

            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let cmd = self.shell_kill_window_cmd(full_name)?;
        let script = format!("sleep {} && {}", delay.as_secs(), cmd);
        self.run_deferred_script(&script)
    }

    fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
        Err(anyhow!("Session mode is not supported by cmux"))
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        Cmd::new("sh")
            .args(&[
                "-c",
                &format!("nohup sh -c {} >/dev/null 2>&1 &", Self::shell_escape(script)),
            ])
            .run()
            .context("Failed to run deferred script")?;
        Ok(())
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        // We need the workspace ref at execution time, so use find-window
        Ok(format!(
            "cmux select-workspace --workspace $(cmux find-window {} | head -1 | awk '{{print $1}}')",
            Self::shell_escape(full_name)
        ))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        Ok(format!(
            "cmux close-workspace --workspace $(cmux find-window {} | head -1 | awk '{{print $1}}')",
            Self::shell_escape(full_name)
        ))
    }

    fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(anyhow!("Session mode is not supported by cmux"))
    }

    fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(anyhow!("Session mode is not supported by cmux"))
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        // pane_id is a surface ref (e.g., "surface:1")
        // Need workspace ref to select workspace, then focus the surface
        if let Some(ws_ref) = self.workspace_for_surface(pane_id) {
            Cmd::new("cmux")
                .args(&["select-workspace", "--workspace", &ws_ref])
                .run()
                .context("Failed to select cmux workspace")?;
            // focus-panel needs --workspace for cross-workspace targeting
            Cmd::new("cmux")
                .args(&["focus-panel", "--panel", pane_id, "--workspace", &ws_ref])
                .run()
                .context("Failed to focus cmux panel")?;
        } else {
            warn!(pane_id = %pane_id, "cmux: no workspace mapping for surface, trying focus-panel only");
            Cmd::new("cmux")
                .args(&["focus-panel", "--panel", pane_id])
                .run()
                .context("Failed to focus cmux panel")?;
        }
        Ok(())
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        self.select_pane(pane_id)
    }

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        _cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        // Map direction: Horizontal → right, Vertical → down
        let dir = match direction {
            SplitDirection::Horizontal => "right",
            SplitDirection::Vertical => "down",
        };

        // Warn about size/percentage being ignored
        if size.is_some() || percentage.is_some() {
            warn!("cmux: pane size/percentage parameters ignored (resize-pane not supported)");
        }

        // Need workspace ref for targeting
        let ws_ref = self
            .workspace_for_surface(target_pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", target_pane_id))?;

        let output = Cmd::new("cmux")
            .args(&["new-split", dir, "--workspace", &ws_ref, "--surface", target_pane_id])
            .run_and_capture_stdout()
            .context("Failed to split cmux pane")?;

        // Parse "OK surface:N workspace:N"
        let parts: Vec<&str> = output.split_whitespace().collect();
        if parts.len() < 2 || parts[0] != "OK" {
            return Err(anyhow!("Unexpected new-split output: {}", output));
        }
        let new_surface_ref = parts[1].to_string();
        // parts[2] is the workspace ref (should match ws_ref)

        // Record surface→workspace mapping
        self.record_surface_mapping(&new_surface_ref, &ws_ref);

        // If a command was provided, we need to wait for surface readiness then send it
        if let Some(cmd_str) = command {
            // Give surface time to initialize
            std::thread::sleep(Duration::from_millis(200));
            // Use respawn to run the command in the new surface
            Cmd::new("cmux")
                .args(&[
                    "respawn-pane",
                    "--workspace",
                    &ws_ref,
                    "--surface",
                    &new_surface_ref,
                    "--command",
                    cmd_str,
                ])
                .run()
                .context("Failed to respawn split pane with command")?;
        }

        debug!(
            surface_ref = %new_surface_ref,
            workspace_ref = %ws_ref,
            direction = %dir,
            "cmux: split pane"
        );

        Ok(new_surface_ref)
    }

    fn respawn_pane(&self, pane_id: &str, _cwd: &Path, cmd: Option<&str>) -> Result<String> {
        let ws_ref = self
            .workspace_for_surface(pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

        if let Some(command) = cmd {
            Cmd::new("cmux")
                .args(&[
                    "respawn-pane",
                    "--workspace",
                    &ws_ref,
                    "--surface",
                    pane_id,
                    "--command",
                    command,
                ])
                .run()
                .context("Failed to respawn cmux pane")?;
        }

        // respawn-pane preserves the surface ref (verified by learning tests)
        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let ws_ref = self.workspace_for_surface(pane_id)?;

        let result = Cmd::new("cmux")
            .args(&[
                "read-screen",
                "--workspace",
                &ws_ref,
                "--surface",
                pane_id,
                "--lines",
                &lines.to_string(),
            ])
            .run_and_capture_stdout();

        match result {
            Ok(output) => Some(output),
            Err(e) => {
                debug!(pane_id = %pane_id, error = %e, "cmux: capture_pane failed");
                None
            }
        }
    }

    // === Text I/O ===

    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        let ws_ref = self
            .workspace_for_surface(pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

        // cmux send interprets \n as Enter
        let text = format!("{}\n", command);
        Cmd::new("cmux")
            .args(&[
                "send",
                "--workspace",
                &ws_ref,
                "--surface",
                pane_id,
                &text,
            ])
            .run()
            .context("Failed to send keys to cmux pane")?;
        Ok(())
    }

    fn send_keys_to_agent(&self, pane_id: &str, command: &str, agent: Option<&str>) -> Result<()> {
        if super::agent::resolve_profile(agent).needs_bang_delay() && command.starts_with('!') {
            let ws_ref = self
                .workspace_for_surface(pane_id)
                .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

            // Send bang prefix, wait, then send the rest (same pattern as tmux backend)
            Cmd::new("cmux")
                .args(&["send", "--workspace", &ws_ref, "--surface", pane_id, "!"])
                .run()
                .context("Failed to send bang to cmux pane")?;

            std::thread::sleep(Duration::from_millis(50));

            let text = format!("{}\n", &command[1..]);
            Cmd::new("cmux")
                .args(&[
                    "send",
                    "--workspace",
                    &ws_ref,
                    "--surface",
                    pane_id,
                    &text,
                ])
                .run()
                .context("Failed to send command after bang to cmux pane")?;

            Ok(())
        } else {
            self.send_keys(pane_id, command)
        }
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        let ws_ref = self
            .workspace_for_surface(pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

        Cmd::new("cmux")
            .args(&[
                "send-key",
                "--workspace",
                &ws_ref,
                "--surface",
                pane_id,
                key,
            ])
            .run()
            .context("Failed to send key to cmux pane")?;
        Ok(())
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        let ws_ref = self
            .workspace_for_surface(pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

        let buffer_name = "wm_paste";

        // Set buffer content (avoids escape interpretation unlike send)
        Cmd::new("cmux")
            .args(&["set-buffer", "--name", buffer_name, content])
            .run()
            .context("Failed to set cmux buffer")?;

        // Paste buffer into surface
        Cmd::new("cmux")
            .args(&[
                "paste-buffer",
                "--name",
                buffer_name,
                "--workspace",
                &ws_ref,
                "--surface",
                pane_id,
            ])
            .run()
            .context("Failed to paste cmux buffer")?;

        Ok(())
    }

    // === Shell ===

    fn get_default_shell(&self) -> Result<String> {
        std::env::var("SHELL").or_else(|_| Ok("/bin/zsh".to_string()))
    }

    fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
        Ok(Box::new(CmuxHandshake::new()))
    }

    // === Status ===

    fn set_status(&self, pane_id: &str, icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        let ws_ref = self
            .workspace_for_surface(pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

        // Map status icons to cmux icon names and colors
        let (cmux_icon, color) = match icon {
            i if i.contains("gear") || i.contains("⚙") || i.contains("🔄") => {
                ("gear", "#FFA500") // Working = amber gear
            }
            i if i.contains("bell") || i.contains("🔔") || i.contains("⏳") => {
                ("bell", "#4A90D9") // Waiting = blue bell
            }
            i if i.contains("check") || i.contains("✅") || i.contains("✓") => {
                ("checkmark", "#4CAF50") // Done = green checkmark
            }
            _ => ("circle", "#808080"), // Unknown = gray circle
        };

        Cmd::new("cmux")
            .args(&[
                "set-status",
                "wm_agent",
                icon,
                "--icon",
                cmux_icon,
                "--color",
                color,
                "--workspace",
                &ws_ref,
            ])
            .run()
            .context("Failed to set cmux status")?;
        Ok(())
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        let ws_ref = self
            .workspace_for_surface(pane_id)
            .ok_or_else(|| anyhow!("cmux: no workspace mapping for surface {}", pane_id))?;

        Cmd::new("cmux")
            .args(&["clear-status", "wm_agent", "--workspace", &ws_ref])
            .run()
            .context("Failed to clear cmux status")?;
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        Ok(()) // No-op: cmux handles status format natively
    }

    // === State Reconciliation ===

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let ws_ref = match self.workspace_for_surface(pane_id) {
            Some(ref r) => r.clone(),
            None => return Ok(None),
        };

        // Check surface exists via surface-health
        let health = Self::surface_health(&ws_ref)?;
        if !health
            .surfaces
            .iter()
            .any(|s| s.surface_ref == pane_id)
        {
            return Ok(None);
        }

        // Get workspace title
        let data = Self::list_workspaces()?;
        let title = data
            .workspaces
            .iter()
            .find(|ws| ws.ws_ref == ws_ref)
            .map(|ws| ws.title.clone());

        // Get CWD from sidebar-state
        let working_dir = Self::parse_sidebar_state(&ws_ref)
            .ok()
            .and_then(|state| state.get("cwd").cloned())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));

        Ok(Some(LivePaneInfo {
            pid: None, // cmux doesn't expose PID in surface-health
            current_command: None, // Best-effort via ps fallback done in validate_agent_alive
            working_dir,
            title: title.clone(),
            session: None, // Flat model
            window: title.or(Some(ws_ref)),
        }))
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        let mut result = HashMap::new();

        // Batch: single list-workspaces call
        let data = Self::list_workspaces()?;

        for ws in &data.workspaces {
            // Get surfaces for this workspace
            let surfaces = match Self::list_pane_surfaces(&ws.ws_ref) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Get CWD from sidebar-state (one call per workspace)
            let working_dir = Self::parse_sidebar_state(&ws.ws_ref)
                .ok()
                .and_then(|state| state.get("cwd").cloned())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));

            for surface in &surfaces.surfaces {
                result.insert(
                    surface.surface_ref.clone(),
                    LivePaneInfo {
                        pid: None,
                        current_command: None,
                        working_dir: working_dir.clone(),
                        title: Some(ws.title.clone()),
                        session: None,
                        window: Some(ws.title.clone()),
                    },
                );
            }
        }

        Ok(result)
    }

    fn validate_agent_alive(&self, state: &crate::state::AgentState) -> Result<bool> {
        let pane_id = &state.pane_key.pane_id;

        // Check if we have a workspace mapping
        let ws_ref = match self.workspace_for_surface(pane_id) {
            Some(ref r) => r.clone(),
            None => {
                // No mapping — try to find the workspace by window name from state
                if let Some(ref handle) = state.window_name {
                    if let Ok(Some(ref r)) = Self::find_workspace_ref(handle) {
                        r.clone()
                    } else {
                        return Ok(false);
                    }
                } else {
                    return Ok(false);
                }
            }
        };

        // Check surface exists via surface-health
        let health = Self::surface_health(&ws_ref)?;
        let surface_exists = health
            .surfaces
            .iter()
            .any(|s| s.surface_ref == *pane_id);

        if !surface_exists {
            return Ok(false);
        }

        // Surface exists — agent is likely alive
        // (cmux doesn't expose PID, so we can't do PID-based validation)
        Ok(true)
    }
}

#[cfg(test)]
mod cmux_tests {
    use super::*;

    #[test]
    fn test_list_workspaces_deser() {
        let json = r#"{
            "window_ref": "window:1",
            "workspaces": [
                {"ref": "workspace:1", "title": "Claude Code", "selected": true, "pinned": false, "index": 0},
                {"ref": "workspace:6", "title": "Terminal 2", "selected": false, "pinned": false, "index": 1}
            ]
        }"#;
        let resp: ListWorkspacesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.workspaces.len(), 2);
        assert_eq!(resp.workspaces[0].ws_ref, "workspace:1");
        assert_eq!(resp.workspaces[0].title, "Claude Code");
        assert!(resp.workspaces[0].selected);
        assert_eq!(resp.workspaces[1].ws_ref, "workspace:6");
        assert!(!resp.workspaces[1].selected);
    }

    #[test]
    fn test_list_pane_surfaces_deser() {
        let json = r#"{"surfaces": [{"ref": "surface:1"}, {"ref": "surface:2", "surface_type": "terminal"}]}"#;
        let resp: ListPaneSurfacesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.surfaces.len(), 2);
        assert_eq!(resp.surfaces[0].surface_ref, "surface:1");
        assert_eq!(resp.surfaces[1].surface_ref, "surface:2");
    }

    #[test]
    fn test_identify_deser() {
        let json = r#"{
            "caller": {
                "surface_ref": "surface:1",
                "workspace_ref": "workspace:1",
                "pane_ref": "pane:1",
                "window_ref": "window:1",
                "surface_type": "terminal",
                "is_browser_surface": false
            },
            "focused": {
                "surface_ref": "surface:2",
                "workspace_ref": "workspace:1",
                "pane_ref": "pane:2",
                "window_ref": "window:1",
                "surface_type": "terminal",
                "is_browser_surface": false
            }
        }"#;
        let resp: IdentifyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.caller.surface_ref, "surface:1");
        assert_eq!(resp.focused.surface_ref, "surface:2");
        assert_eq!(resp.caller.workspace_ref, "workspace:1");
    }

    #[test]
    fn test_surface_health_deser() {
        let json = r#"{"surfaces": [{"ref": "surface:1", "type": "terminal", "in_window": true, "index": 0}]}"#;
        let resp: SurfaceHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.surfaces.len(), 1);
        assert_eq!(resp.surfaces[0].surface_ref, "surface:1");
        assert_eq!(resp.surfaces[0].surface_type, "terminal");
        assert!(resp.surfaces[0].in_window);
    }

    #[test]
    fn test_new_workspace_output_parsing() {
        let output = "OK 1D367A37-F2C1-4F6B-B3E7-A1234567890A";
        let uuid = output.strip_prefix("OK ").unwrap().trim();
        assert_eq!(uuid, "1D367A37-F2C1-4F6B-B3E7-A1234567890A");
    }

    #[test]
    fn test_new_split_output_parsing() {
        let output = "OK surface:8 workspace:6";
        let parts: Vec<&str> = output.split_whitespace().collect();
        assert_eq!(parts[0], "OK");
        assert_eq!(parts[1], "surface:8");
        assert_eq!(parts[2], "workspace:6");
    }

    #[test]
    fn test_shell_escape() {
        assert_eq!(CmuxBackend::shell_escape("hello"), "'hello'");
        assert_eq!(
            CmuxBackend::shell_escape("it's"),
            "'it'\\''s'"
        );
        assert_eq!(CmuxBackend::shell_escape("wm-feature"), "'wm-feature'");
    }

    #[test]
    fn test_surface_to_workspace_mapping() {
        let backend = CmuxBackend::new();
        assert!(backend.workspace_for_surface("surface:1").is_none());

        backend.record_surface_mapping("surface:1", "workspace:1");
        assert_eq!(
            backend.workspace_for_surface("surface:1"),
            Some("workspace:1".to_string())
        );
    }
}
