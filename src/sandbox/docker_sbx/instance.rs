//! sbx sandbox state and commands.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tracing::{debug, info, warn};

use crate::config::Config;

use super::plan::{
    ExecRequest, LaunchPlan, LaunchTarget, NetworkRule, Platform, SbxCommand, SbxState,
};

/// A sandbox from `sbx ls --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SbxSandboxInfo {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
}

impl SbxSandboxInfo {
    /// Check if the sandbox is running.
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }

    /// When the sandbox was created.
    pub fn created(&self) -> Option<SystemTime> {
        self.created_at.as_deref().and_then(parse_timestamp)
    }

    /// When the sandbox last ran a session.
    pub fn last_used(&self) -> Option<SystemTime> {
        self.last_used_at.as_deref().and_then(parse_timestamp)
    }
}

/// Parse sbx's UTC timestamps (`2026-11-14T03:26:22.21548Z`).
fn parse_timestamp(timestamp: &str) -> Option<SystemTime> {
    crate::github::parse_github_timestamp(timestamp)
        .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// A rule from `sbx policy ls <name> --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SbxRule {
    pub scope: String,
    #[serde(default)]
    pub decision: String,
    /// `network`, `filesystem:read`, ...
    #[serde(default)]
    pub resource_type: String,
    #[serde(default)]
    pub resources: Vec<String>,
}

/// A check from `sbx diagnose --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SbxCheck {
    pub name: String,
    pub status: String,
}

#[derive(Deserialize)]
struct SbxRules {
    #[serde(default)]
    rules: Vec<SbxRule>,
}

#[derive(Deserialize)]
struct SbxDiagnose {
    #[serde(default)]
    checks: Vec<SbxCheck>,
}

/// Parse `sbx ls --json`.
pub fn parse_sbx_sandboxes(stdout: &[u8]) -> Result<Vec<SbxSandboxInfo>> {
    serde_json::from_slice(stdout).context("Failed to parse sbx ls output")
}

/// Parse `sbx policy ls <name> --json`.
pub fn parse_sbx_rules(stdout: &[u8]) -> Result<Vec<SbxRule>> {
    Ok(serde_json::from_slice::<SbxRules>(stdout)
        .context("Failed to parse sbx policy ls output")?
        .rules)
}

/// Parse `sbx diagnose --json`.
pub fn parse_sbx_checks(stdout: &[u8]) -> Result<Vec<SbxCheck>> {
    Ok(serde_json::from_slice::<SbxDiagnose>(stdout)
        .context("Failed to parse sbx diagnose output")?
        .checks)
}

/// What `sbx policy rm` prints for a rule that doesn't exist.
const RULE_NOT_FOUND: &str = "rule not found";

/// The error sbx prints for every sandbox verb before `sbx policy init`.
const POLICY_NOT_INITIALIZED: &str = "global network policy has not been initialized";

/// Run an sbx command that prints JSON and return its stdout.
fn sbx_json(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("sbx")
        .args(args)
        .arg("--json")
        .output()
        .with_context(|| format!("Failed to run sbx {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "sbx {} failed: {}",
            args.join(" "),
            sbx_error(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(output.stdout)
}

/// Turn sbx's stderr into an error message, naming the fix for errors the
/// user has to resolve.
fn sbx_error(stderr: &str) -> String {
    if stderr.contains(POLICY_NOT_INITIALIZED) {
        return format!(
            "{}. Choose a global network policy with `sbx policy init` first.",
            POLICY_NOT_INITIALIZED
        );
    }
    stderr.trim().to_string()
}

/// sbx operations.
pub struct SbxInstance;

impl SbxInstance {
    /// Check if sbx is available on the system.
    pub fn is_sbx_available() -> bool {
        Command::new("sbx")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// List all sbx sandboxes.
    pub fn list() -> Result<Vec<SbxSandboxInfo>> {
        parse_sbx_sandboxes(&sbx_json(&["ls"])?)
    }

    /// Stop a sandbox by name. Succeeds if it is already stopped.
    pub fn stop_by_name(name: &str) -> Result<()> {
        run_sbx_args(&["stop", name])
    }

    /// Delete a sandbox by name, with its policy rules and the secrets
    /// scoped to it. Succeeds if it doesn't exist.
    pub fn remove_by_name(name: &str) -> Result<()> {
        match run_sbx_args(&["rm", "-f", name]) {
            Err(e) if e.to_string().contains("not found") => Ok(()),
            result => result,
        }
    }
}

/// Run an sbx command, failing with sbx's error.
fn run_sbx_args(args: &[&str]) -> Result<()> {
    let output = Command::new("sbx")
        .args(args)
        .output()
        .with_context(|| format!("Failed to run sbx {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "sbx {} failed: {}",
            args.join(" "),
            sbx_error(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(())
}

/// Delete the worktree's sbx sandbox and its state directory, if workmux
/// created one for it (its state directory exists).
///
/// Called when a worktree is removed, before its directory goes away: the
/// sandbox name is derived from the worktree's path and git metadata.
/// Best effort: failures are logged, never fatal.
pub fn remove_sandbox_for_worktree(worktree_root: &Path) {
    let name = match super::sandbox_name(worktree_root) {
        Ok(name) => name,
        Err(e) => {
            debug!(worktree = %worktree_root.display(), error = %e, "no sbx sandbox name");
            return;
        }
    };
    let Ok(state_dir) = sbx_state_dir_path(&name) else {
        return;
    };
    if !state_dir.exists() {
        return;
    }
    info!(sandbox = %name, "removing sbx sandbox");
    if let Err(e) = SbxInstance::remove_by_name(&name) {
        warn!(sandbox = %name, error = %e, "failed to remove sbx sandbox");
        eprintln!("Warning: failed to remove sbx sandbox {name}: {e}");
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&state_dir) {
        warn!(sandbox = %name, error = %e, "failed to clean up state dir");
    }
}

/// The platform workmux is running on.
fn current_platform() -> Platform {
    Platform {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        kvm: Path::new("/dev/kvm").exists(),
    }
}

/// Gather what sbx reports about the sandbox `name`.
pub fn gather_state(name: &str) -> Result<SbxState> {
    if !SbxInstance::is_sbx_available() {
        bail!(
            "docker_sbx backend is enabled but sbx is not installed.\n\
             Install Docker Sandboxes: https://docs.docker.com/ai/sandboxes/\n\
             Or disable sandbox: set 'sandbox.enabled: false' in config."
        );
    }
    let checks = parse_sbx_checks(&sbx_json(&["diagnose"])?)?;
    let platform = current_platform();
    let sandbox = SbxInstance::list()?
        .into_iter()
        .find(|sandbox| sandbox.name == name);
    debug!(
        sandbox = %name,
        exists = sandbox.is_some(),
        running = sandbox.as_ref().is_some_and(SbxSandboxInfo::is_running),
        "sbx sandbox state"
    );
    // An existing sandbox's rules include the global ones; before it exists,
    // the global ones are what it will get. Listing fails before the global
    // policy is initialised, which the plan reports.
    let policy_ls = match &sandbox {
        Some(_) => vec!["policy", "ls", name],
        None => vec!["policy", "ls"],
    };
    let rules = match sbx_json(&policy_ls) {
        Ok(stdout) => parse_sbx_rules(&stdout)?,
        Err(e) => {
            debug!(error = %e, "failed to list sbx rules");
            Vec::new()
        }
    };
    Ok(SbxState {
        platform,
        checks,
        sandbox,
        rules,
    })
}

/// Host-side state directory for a sandbox: `$XDG_STATE_HOME/workmux/docker-sbx/<name>/`.
pub(crate) fn sbx_state_dir_path(name: &str) -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("docker-sbx").join(name))
}

/// File in a sandbox's state directory holding the network rules workmux
/// last wrote for it. Removed with the state directory when the sandbox is.
const NETWORK_RECORD_FILE: &str = "network-rules.json";

#[derive(Default, Deserialize, Serialize)]
struct NetworkRecord {
    #[serde(default)]
    rules: Vec<NetworkRule>,
}

/// The network rules workmux last wrote for a sandbox. Empty if missing or
/// corrupted.
fn read_network_record(state_dir: &Path) -> Vec<NetworkRule> {
    let path = state_dir.join(NETWORK_RECORD_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<NetworkRecord>(&content) {
            Ok(record) => record.rules,
            Err(e) => {
                warn!(?path, error = %e, "corrupted sbx network record, ignoring");
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Record the network rules workmux wrote for a sandbox.
fn write_network_record(state_dir: &Path, rules: &[NetworkRule]) -> Result<()> {
    let record = NetworkRecord {
        rules: rules.to_vec(),
    };
    let content = serde_json::to_string_pretty(&record)?;
    crate::util::write_atomic(&state_dir.join(NETWORK_RECORD_FILE), content.as_bytes())
}

/// Run one sbx command from a plan, failing with sbx's error.
fn run_sbx(command: &SbxCommand) -> Result<()> {
    debug!(args = ?command.args, "running sbx");
    let output = Command::new("sbx")
        .args(&command.args)
        .envs(command.env.iter().map(|(k, v)| (k, v)))
        .output()
        .with_context(|| format!("Failed to run sbx {}", command.args[0]))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Another launch into the same sandbox may have removed the rule.
        if command
            .args
            .starts_with(&["policy".to_string(), "rm".to_string()])
            && stderr.contains(RULE_NOT_FOUND)
        {
            debug!(args = ?command.args, "sbx rule already removed");
            return Ok(());
        }
        bail!("sbx {} failed: {}", command.args[0], sbx_error(&stderr));
    }
    Ok(())
}

/// Gather sbx's state, compute the launch plan and run its setup commands:
/// load the template and create the sandbox if it's missing, and reconcile
/// its policy rules, recording the ones workmux writes for
/// `sandbox.network`. Returns the plan, whose `exec` the caller runs.
pub fn prepare_launch(
    config: &Config,
    worktree_root: &Path,
    pane_cwd: &Path,
    agent: &str,
    exec: &ExecRequest,
) -> Result<LaunchPlan> {
    let worktree_root = worktree_root
        .canonicalize()
        .unwrap_or_else(|_| worktree_root.to_path_buf());
    let pane_cwd = pane_cwd
        .canonicalize()
        .unwrap_or_else(|_| pane_cwd.to_path_buf());
    let name = super::sandbox_name(&worktree_root)?;
    let state_dir = sbx_state_dir_path(&name)?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;
    let git = super::git::git_metadata(&worktree_root)?;
    // sbx mounts only directories that exist.
    if let Some(config_dir) = config.sandbox.resolved_agent_config_dir(agent) {
        let _ = std::fs::create_dir_all(&config_dir);
    }
    let opencode_config_dir = crate::agent_setup::opencode::opencode_config_dir()
        .filter(|dir| agent == "opencode" && dir.is_dir())
        .map(|dir| dir.canonicalize().unwrap_or(dir));

    let state = gather_state(&name)?;
    let network_record = read_network_record(&state_dir);
    let target = LaunchTarget {
        sandbox_name: &name,
        worktree_root: &worktree_root,
        pane_cwd: &pane_cwd,
        git: &git,
        opencode_config_dir: opencode_config_dir.as_deref(),
        state_dir: &state_dir,
        network_record: &network_record,
    };
    let plan = super::launch_plan(&config.sandbox, &target, agent, &state, exec)?;
    for note in &plan.notes {
        info!(sandbox = %name, note, "sbx network note");
        eprintln!("note: {note}");
    }

    if plan.create_template.is_some() {
        super::template::ensure_template(&config.sandbox.resolved_image(agent))?;
        info!(sandbox = %name, "creating sbx sandbox");
        eprintln!("  Workspaces:");
        if let Some(create) = plan.setup.first() {
            for workspace in create.args.iter().skip_while(|a| *a != "shell").skip(1) {
                eprintln!("    {workspace}");
            }
        }
    }
    // Record the rules about to be written before writing them, so a
    // failed launch still knows which rules are workmux's.
    let mut pending = network_record.clone();
    pending.extend(
        plan.network_rules
            .iter()
            .filter(|rule| !network_record.contains(rule))
            .cloned(),
    );
    if pending != network_record {
        write_network_record(&state_dir, &pending)?;
    }
    for (i, command) in plan.setup.iter().enumerate() {
        if i == 0 && plan.create_template.is_some() {
            let msg = format!("Creating sbx sandbox {name}");
            crate::spinner::with_spinner(&msg, || run_sbx(command))?;
        } else {
            run_sbx(command)?;
        }
    }
    if plan.network_rules != pending {
        write_network_record(&state_dir, &plan.network_rules)?;
    }
    Ok(plan)
}

/// A fresh identifier for one exec's processes in the guest.
pub fn new_session() -> Result<String> {
    Ok(format!(
        "{:016x}",
        getrandom::u64().context("Failed to generate an sbx session id")?
    ))
}

/// Run the plan's `sbx exec` in the foreground and return its exit code.
///
/// sbx leaves the guest command running when the `sbx exec` client goes
/// away, so a hangup or termination (closing the pane) hangs up the
/// session's processes in the guest before returning.
pub fn run_exec(plan: &LaunchPlan) -> Result<i32> {
    let hangup = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGHUP, libc::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&hangup))
            .context("Failed to install a signal handler")?;
    }

    debug!(sandbox = %plan.sandbox_name, args = ?plan.exec.args, "spawning sbx exec");
    let mut child = Command::new("sbx")
        .args(&plan.exec.args)
        .envs(plan.exec.env.iter().map(|(k, v)| (k, v)))
        .spawn()
        .context("Failed to execute sbx exec")?;
    // The client usually exits on the same hangup; hang up the guest side
    // whether it has or not. Its command ending then ends the client.
    let status = loop {
        if hangup.load(Ordering::Relaxed) {
            hang_up(plan);
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            if hangup.load(Ordering::Relaxed) {
                hang_up(plan);
            }
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    for command in &plan.teardown {
        if let Err(e) = run_sbx(command) {
            warn!(sandbox = %plan.sandbox_name, error = %e, "failed to remove sbx rule");
        }
    }
    Ok(status.code().unwrap_or(1))
}

fn hang_up(plan: &LaunchPlan) {
    info!(sandbox = %plan.sandbox_name, "hanging up the sbx session");
    if let Err(e) = run_sbx(&plan.hangup) {
        warn!(sandbox = %plan.sandbox_name, error = %e, "failed to hang up the sbx session");
    }
}

/// Ensure the worktree's sbx sandbox exists, creating it if necessary.
///
/// Should be called from the main process BEFORE creating tmux panes, so
/// setup errors and progress appear in the user's terminal. Returns the
/// sandbox name.
pub fn ensure_sandbox(config: &Config, worktree_root: &Path) -> Result<String> {
    let agent = crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name();
    let plan = prepare_launch(
        config,
        worktree_root,
        worktree_root,
        agent,
        &ExecRequest::default(),
    )?;
    info!(sandbox = %plan.sandbox_name, "sbx sandbox ready");
    Ok(plan.sandbox_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sbx_ls_json() {
        let sandboxes = parse_sbx_sandboxes(
            br#"[{"name":"wm-proj-1","id":"f467","agent":"shell","status":"stopped","last_used_at":"2026-08-14T20:59:48Z","workspaces":["/code/proj","/code/proj/.git:ro"],"created_at":"2026-08-14T20:59:48Z"}]"#,
        )
        .unwrap();
        assert_eq!(sandboxes[0].name, "wm-proj-1");
        assert!(!sandboxes[0].is_running());
    }

    #[test]
    fn sbx_ls_timestamps_are_when_the_sandbox_was_created_and_last_used() {
        use std::time::{Duration, UNIX_EPOCH};

        let sandboxes = parse_sbx_sandboxes(
            br#"[{"name":"wm-proj-1","status":"running","last_used_at":"2026-11-14T03:26:22.21548Z","created_at":"2026-11-14T03:26:20Z"},
                 {"name":"wm-proj-2","status":"stopped"}]"#,
        )
        .unwrap();
        // 2026-11-14T03:26:20Z
        let created = UNIX_EPOCH + Duration::from_secs(1_794_626_780);
        assert_eq!(sandboxes[0].created(), Some(created));
        assert_eq!(
            sandboxes[0].last_used(),
            Some(created + Duration::from_secs(2))
        );
        assert_eq!(sandboxes[1].created(), None);
        assert_eq!(sandboxes[1].last_used(), None);
    }

    #[test]
    fn parses_sbx_policy_ls_json() {
        let rules = parse_sbx_rules(
            br#"{"rules":[{"id":"r1","name":"","policy_id":"p","scope":"global","applies_to":"all","resource_type":"network","decision":"allow","resources":["api.anthropic.com"],"origin":"local","layer":"","status":"active","editable":false,"provenance":{"created_via":"default"},"actions":["net:connect:tcp"]},
                          {"id":"r2","scope":"sandbox:wm-proj-1","decision":"allow","resources":["localhost"]}]}"#,
        )
        .unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].scope, "sandbox:wm-proj-1");
        assert_eq!(rules[1].resources, vec!["localhost".to_string()]);
    }

    #[test]
    fn network_record_round_trips_and_starts_empty() {
        use super::super::plan::RuleDecision;

        let dir = tempfile::tempdir().unwrap();
        assert!(read_network_record(dir.path()).is_empty());

        let rules = vec![
            NetworkRule {
                decision: RuleDecision::Allow,
                resource: "**.example.com:443".to_string(),
            },
            NetworkRule {
                decision: RuleDecision::Deny,
                resource: "example.com:443".to_string(),
            },
        ];
        write_network_record(dir.path(), &rules).unwrap();
        assert_eq!(read_network_record(dir.path()), rules);

        std::fs::write(dir.path().join(NETWORK_RECORD_FILE), "not json").unwrap();
        assert!(read_network_record(dir.path()).is_empty());
    }

    #[test]
    fn parses_sbx_diagnose_json() {
        let checks = parse_sbx_checks(
            br#"{"version":"1.0","checks":[{"name":"Authentication","status":"pass","message":"authenticated","detail":"","hint":""}],"summary":{"pass":1}}"#,
        )
        .unwrap();
        assert_eq!(checks[0].name, "Authentication");
        assert_eq!(checks[0].status, "pass");
    }

    #[test]
    fn uninitialised_policy_error_names_sbx_policy_init() {
        let message = sbx_error("error: global network policy has not been initialized\n");
        assert!(message.contains("sbx policy init"), "{message}");
    }
}
