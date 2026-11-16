//! sbx sandbox state and commands.

use anyhow::{Context, Result, bail};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tracing::{debug, info, warn};

use crate::config::{Config, DockerSbxAgent};

use super::acknowledgement;
use super::plan::{
    CreateRecord, CreateSettings, ExecRequest, LaunchPlan, LaunchTarget, NetworkRule, Platform,
    SbxCommand, SbxState, Widening,
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

    /// Return when the sandbox was created.
    pub fn created(&self) -> Option<SystemTime> {
        self.created_at.as_deref().and_then(parse_timestamp)
    }

    /// Return when the sandbox last ran a session.
    pub fn last_used(&self) -> Option<SystemTime> {
        self.last_used_at.as_deref().and_then(parse_timestamp)
    }
}

/// Parse sbx's UTC timestamps (`2026-11-14T03:26:22.21548Z`).
fn parse_timestamp(timestamp: &str) -> Option<SystemTime> {
    crate::util::parse_utc_timestamp(timestamp)
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
    /// `added` (by workmux or the user), `provisioned` (by a kit) or
    /// `default` (the global policy).
    #[serde(default)]
    pub provenance: SbxRuleProvenance,
}

/// A rule's `provenance` from `sbx policy ls --json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SbxRuleProvenance {
    #[serde(default)]
    pub created_via: String,
}

/// A check from `sbx diagnose --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SbxCheck {
    pub name: String,
    /// `pass`, `warn`, `fail` or `skip`.
    pub status: String,
    #[serde(default)]
    pub message: String,
    /// What sbx suggests doing about a failed check.
    #[serde(default)]
    pub hint: String,
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

/// Run `sbx diagnose --json`, reading its checks even when it exits 1 for a
/// failed check.
fn sbx_diagnose() -> Result<Vec<SbxCheck>> {
    let output = Command::new("sbx")
        .args(["diagnose", "--json"])
        .output()
        .context("Failed to run sbx diagnose")?;
    match parse_sbx_checks(&output.stdout) {
        Ok(checks) => Ok(checks),
        Err(e) if output.status.success() => Err(e),
        Err(_) => bail!(
            "sbx diagnose failed ({}): {}",
            output.status,
            sbx_error(&String::from_utf8_lossy(&output.stderr))
        ),
    }
}

/// Turn sbx's stderr into an error message that names the fix.
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

    /// Delete a sandbox by name. Succeeds if it doesn't exist.
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

/// A worktree's sbx sandbox about to be deleted, locked against launches.
pub struct SandboxRemoval {
    name: String,
    state_dir: PathBuf,
    _lock: Flock<File>,
}

/// Lock a worktree's sbx sandboxes for deletion, one per agent it ran.
///
/// Waits for any launch setting a sandbox up, then holds its lock until
/// [`SandboxRemoval::remove`] or drop. Best effort: failures are logged.
pub fn prepare_sandbox_removal(project_root: &Path, worktree_root: &Path) -> Vec<SandboxRemoval> {
    let Ok(state_root) = sbx_state_root() else {
        return Vec::new();
    };
    worktree_sandboxes_in(&state_root, project_root, worktree_root)
        .iter()
        .filter_map(|name| prepare_one_removal(name))
        .collect()
}

fn prepare_one_removal(name: &str) -> Option<SandboxRemoval> {
    let state_dir = sbx_state_dir_path(name).ok()?;
    if !state_dir.exists() {
        return None;
    }
    match lock_state_dir(&state_dir) {
        Ok(lock) => Some(SandboxRemoval {
            name: name.to_string(),
            state_dir,
            _lock: lock,
        }),
        Err(e) => {
            warn!(sandbox = %name, error = %e, "failed to lock sbx state dir");
            None
        }
    }
}

impl SandboxRemoval {
    /// Delete the sandbox and its state directory. Best effort: failures are
    /// logged.
    pub fn remove(self) {
        let name = &self.name;
        info!(sandbox = %name, "removing sbx sandbox");
        if let Err(e) = SbxInstance::remove_by_name(name) {
            warn!(sandbox = %name, error = %e, "failed to remove sbx sandbox");
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.state_dir) {
            warn!(sandbox = %name, error = %e, "failed to clean up state dir");
        }
        acknowledgement::remove_acknowledgement(name);
    }
}

/// Describe the platform workmux is running on.
fn current_platform() -> Platform {
    Platform {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        kvm: Path::new("/dev/kvm").exists(),
    }
}

/// List the sandbox `name` and its rules, or the global rules while it
/// doesn't exist.
fn list_sandbox_and_rules(name: &str) -> Result<(Option<SbxSandboxInfo>, Vec<SbxRule>)> {
    let sandbox = SbxInstance::list()?
        .into_iter()
        .find(|sandbox| sandbox.name == name);
    debug!(
        sandbox = %name,
        exists = sandbox.is_some(),
        running = sandbox.as_ref().is_some_and(SbxSandboxInfo::is_running),
        "sbx sandbox state"
    );
    // An uninitialised global policy is reported by the plan. Any other
    // failure is fatal: the plan needs the rules to keep `deny` narrow.
    let policy_ls = match &sandbox {
        Some(_) => vec!["policy", "ls", name],
        None => vec!["policy", "ls"],
    };
    let rules = match sbx_json(&policy_ls) {
        Ok(stdout) => parse_sbx_rules(&stdout)?,
        Err(e) if e.to_string().contains(POLICY_NOT_INITIALIZED) => {
            debug!(error = %e, "failed to list sbx rules");
            Vec::new()
        }
        Err(e) => return Err(e.context("Failed to list sbx network rules")),
    };
    Ok((sandbox, rules))
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
    let platform = current_platform();
    // Run diagnose alongside the listings, which it doesn't depend on.
    let (checks, sandbox_and_rules) = std::thread::scope(|scope| {
        let checks = scope.spawn(sbx_diagnose);
        let sandbox_and_rules = list_sandbox_and_rules(name);
        (
            checks.join().expect("sbx diagnose thread panicked"),
            sandbox_and_rules,
        )
    });
    let checks = checks?;
    let (sandbox, rules) = sandbox_and_rules?;
    Ok(SbxState {
        platform,
        checks,
        sandbox,
        rules,
    })
}

/// Return a sandbox's host-side state directory.
pub(crate) fn sbx_state_dir_path(name: &str) -> Result<PathBuf> {
    Ok(sbx_state_root()?.join(name))
}

/// Return the directory holding every sbx sandbox's state directory.
fn sbx_state_root() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("docker-sbx"))
}

/// State file recording the worktree and agent a sandbox is for, written
/// first so that removing the worktree finds the sandbox by its exact name.
const SANDBOX_RECORD_FILE: &str = "sandbox.json";

#[derive(Deserialize, Serialize)]
struct SandboxRecord {
    worktree: PathBuf,
    agent: String,
}

/// Record the worktree and agent a sandbox is for.
fn write_sandbox_record(state_dir: &Path, worktree_root: &Path, agent: &str) -> Result<()> {
    let record = SandboxRecord {
        worktree: worktree_root
            .canonicalize()
            .unwrap_or_else(|_| worktree_root.to_path_buf()),
        agent: agent.to_string(),
    };
    let content = serde_json::to_string_pretty(&record)?;
    crate::util::write_atomic(&state_dir.join(SANDBOX_RECORD_FILE), content.as_bytes())
}

/// List the worktree's sandboxes that have state directories under
/// `state_root`, matching exact names, since a prefix could match another
/// project's.
fn worktree_sandboxes_in(
    state_root: &Path,
    project_root: &Path,
    worktree_root: &Path,
) -> Vec<String> {
    let worktree = worktree_root
        .canonicalize()
        .unwrap_or_else(|_| worktree_root.to_path_buf());
    let Ok(entries) = std::fs::read_dir(state_root) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let path = entry.path().join(SANDBOX_RECORD_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<SandboxRecord>(&content) {
            Ok(record)
                if record.worktree == worktree
                    && super::sandbox_name_in(project_root, &worktree, &record.agent) == name =>
            {
                names.push(name)
            }
            Ok(_) => {}
            Err(e) => warn!(?path, error = %e, "corrupted sbx sandbox record, ignoring"),
        }
    }
    names
}

/// State file holding the network rules workmux last wrote for a sandbox.
const NETWORK_RECORD_FILE: &str = "network-rules.json";

#[derive(Default, Deserialize, Serialize)]
struct NetworkRecord {
    #[serde(default)]
    rules: Vec<NetworkRule>,
}

/// Read the network rules workmux last wrote for a sandbox. Empty if missing
/// or corrupted.
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

/// State file recording the settings a sandbox was created with, written once
/// creating it fully succeeded.
const CREATED_RECORD_FILE: &str = "created.json";

/// Read a sandbox's create record.
fn read_created_record(state_dir: &Path) -> CreateRecord {
    let path = state_dir.join(CREATED_RECORD_FILE);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CreateRecord::Missing,
        Err(e) => return CreateRecord::Unreadable(e.to_string()),
    };
    match serde_json::from_str(&content) {
        Ok(settings) => CreateRecord::Recorded(Box::new(settings)),
        Err(e) => CreateRecord::Unreadable(e.to_string()),
    }
}

/// Record the settings a sandbox was created with, once it's fully set up.
fn write_created_record(state_dir: &Path, settings: &CreateSettings) -> Result<()> {
    let content = serde_json::to_string_pretty(settings)?;
    crate::util::write_atomic(&state_dir.join(CREATED_RECORD_FILE), content.as_bytes())
}

/// Forget a sandbox's create record before creating it again.
fn remove_created_record(state_dir: &Path) -> Result<()> {
    remove_state_file(state_dir, CREATED_RECORD_FILE)
}

fn remove_state_file(state_dir: &Path, file: &str) -> Result<()> {
    match std::fs::remove_file(state_dir.join(file)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Lock file in a sandbox's state directory.
const LOCK_FILE: &str = ".lock";

/// Lock a sandbox's state directory while a launch sets the sandbox up.
///
/// Concurrent launches (the panes of `target: all`) then never create it
/// twice, and removing the worktree waits for them.
fn lock_state_dir(state_dir: &Path) -> Result<Flock<File>> {
    let lock_path = state_dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("Failed to open lock file: {}", lock_path.display()))?;
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_file, errno)| errno)
        .with_context(|| format!("Failed to acquire lock: {}", lock_path.display()))
}

/// Run one sbx command from a plan and return its stderr.
fn run_sbx(command: &SbxCommand) -> Result<String> {
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
            return Ok(String::new());
        }
        bail!("sbx {} failed: {}", command.args[0], sbx_error(&stderr));
    }
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

/// Extract the warnings and notes `sbx create` printed, without its progress
/// lines.
fn create_notices(stderr: &str) -> Vec<String> {
    let mut notices: Vec<String> = Vec::new();
    let mut continuing = false;
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if line.starts_with("warning:") || line.starts_with("Note:") {
            notices.push(line.trim_end().to_string());
            continuing = true;
        } else if continuing
            && line.starts_with(char::is_whitespace)
            && !trimmed.is_empty()
            && !trimmed.starts_with(['\u{25cb}', '\u{2713}'])
            && let Some(notice) = notices.last_mut()
        {
            notice.push('\n');
            notice.push_str(line.trim_end());
        } else {
            continuing = false;
        }
    }
    notices
}

/// Set up the sandbox of `selected_agent`, the pane agent, for a launch.
///
/// Gathers sbx's state, computes the plan and runs everything up to its
/// `exec`, which the caller runs.
pub fn prepare_launch(
    config: &Config,
    worktree_root: &Path,
    pane_cwd: &Path,
    selected_agent: &str,
    exec: &ExecRequest,
) -> Result<LaunchPlan> {
    let worktree_root = worktree_root
        .canonicalize()
        .unwrap_or_else(|_| worktree_root.to_path_buf());
    let pane_cwd = pane_cwd
        .canonicalize()
        .unwrap_or_else(|_| pane_cwd.to_path_buf());
    let name = super::sandbox_name(&worktree_root, selected_agent)?;
    let agent = super::agent_type(config, selected_agent);
    let state_dir = sbx_state_dir_path(&name)?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;
    write_sandbox_record(&state_dir, &worktree_root, selected_agent)?;
    // sbx mounts only directories that exist.
    if config.sandbox.docker_sbx.agent() == DockerSbxAgent::Shell
        && let Some(config_dir) = config.sandbox.resolved_agent_config_dir(agent)
    {
        let _ = std::fs::create_dir_all(&config_dir);
    }
    let opencode_config_dir = crate::agent_setup::opencode::opencode_config_dir()
        .filter(|dir| agent == "opencode" && dir.is_dir())
        .map(|dir| dir.canonicalize().unwrap_or(dir));

    let toolchain =
        crate::sandbox::toolchain::resolve_toolchain(&config.sandbox.toolchain(), &worktree_root);

    let _lock = lock_state_dir(&state_dir)?;
    // Removing the worktree removed the sandbox while this launch waited.
    if !state_dir.join(LOCK_FILE).exists() {
        bail!("sbx sandbox {name} was removed with its worktree");
    }
    let state = gather_state(&name)?;
    let network_record = read_network_record(&state_dir);
    let created_with = read_created_record(&state_dir);
    // Only creating the sandbox mounts the git metadata.
    let git = if super::plan::creates_sandbox(&state, &created_with) {
        Some(super::git::git_metadata(&worktree_root)?)
    } else {
        None
    };
    let target = LaunchTarget {
        sandbox_name: &name,
        worktree_root: &worktree_root,
        pane_cwd: &pane_cwd,
        git: git.as_ref(),
        opencode_config_dir: opencode_config_dir.as_deref(),
        state_dir: &state_dir,
        network_record: &network_record,
        toolchain: &toolchain,
        created_with: &created_with,
    };
    let plan = super::launch_plan(&config.sandbox, &target, agent, &state, exec)?;
    for note in &plan.notes {
        info!(sandbox = %name, note, "sbx launch note");
    }
    for warning in &plan.warnings {
        warn!(sandbox = %name, warning, "sbx launch warning");
    }

    // Ask before creating the sandbox, unless kits add rules at create: then
    // `after_create` asks once, after seeing them.
    let sbx = &config.sandbox.docker_sbx;
    let kits_add_rules = sbx.agent() == DockerSbxAgent::Kit || !sbx.kits().is_empty();
    if plan.create.is_some()
        && !(kits_add_rules && acknowledgement::can_ask() && !sbx.accepts_wider_network_policy())
    {
        acknowledgement::acknowledge(config, &name, &plan.widenings)?;
    }

    let image = config.sandbox.resolved_image(agent);
    if plan.create.is_some() {
        super::template::ensure_template(&image)?;
    } else {
        // Keep the template's freshness cache current for the next create.
        super::template::check_freshness_in_background(&image);
        if config.sandbox.docker_sbx.provision_script().is_some() {
            info!(sandbox = %name, "custom provision script only runs on sandbox creation; recreate sandbox to apply changes");
        }
    }
    // Kits add their rules at create, after the plan listed them.
    let after_create = || {
        if !config.sandbox.network_policy_is_deny() {
            return Ok(());
        }
        let (_, rules) = list_sandbox_and_rules(&name)
            .context("Failed to list the new sbx sandbox's network rules")?;
        let widenings = super::plan::widenings(&config.sandbox, &rules, &name, &plan.network_rules);
        for widening in &widenings {
            if let Widening::KitHosts(_) = widening {
                let warning = widening.warning(&name);
                warn!(sandbox = %name, warning, "sbx launch warning");
            }
        }
        acknowledgement::acknowledge(config, &name, &widenings)
    };
    let notices = apply_plan_with(&plan, &state_dir, &network_record, after_create, run_sbx)?;
    for notice in &notices {
        warn!(sandbox = %name, notice, "sbx create notice");
    }
    Ok(plan)
}

/// Run a plan's commands up to its exec and return sbx's create notices.
///
/// The state directory is kept in step so the next launch recovers from one
/// that fails or is killed partway: rules are recorded before they're
/// written, and the create record only once the sandbox is fully set up. A
/// sandbox this launch creates is removed again when anything after
/// `sbx create` fails.
fn apply_plan_with(
    plan: &LaunchPlan,
    state_dir: &Path,
    network_record: &[NetworkRule],
    after_create: impl FnOnce() -> Result<()>,
    mut run: impl FnMut(&SbxCommand) -> Result<String>,
) -> Result<Vec<String>> {
    let name = &plan.sandbox_name;
    if let Some(create) = &plan.create {
        if let Some(remove) = &plan.remove {
            info!(sandbox = %name, "removing unfinished sbx sandbox to create it again");
            eprintln!(
                "Removing sbx sandbox {name}, as an earlier launch didn't finish setting it up; \
                 creating it again"
            );
            run(remove)?;
        }
        remove_created_record(state_dir)?;
        super::mask::write_masks(state_dir, &plan.masks)?;
        info!(sandbox = %name, "creating sbx sandbox");
        eprintln!("  Workspaces:");
        for workspace in &create.workspaces {
            eprintln!("    {workspace}");
        }
    }
    // Record the rules first, so a failed launch still knows they're
    // workmux's.
    let mut pending = network_record.to_vec();
    pending.extend(
        plan.network_rules
            .iter()
            .filter(|rule| !network_record.contains(rule))
            .cloned(),
    );
    if pending != network_record {
        write_network_record(state_dir, &pending)?;
    }
    let mut notices = Vec::new();
    if let Some(create) = &plan.create {
        let msg = format!("Creating sbx sandbox {name}");
        let stderr = crate::spinner::with_spinner(&msg, || run(&create.command))?;
        notices = create_notices(&stderr);
        if let Err(error) = after_create() {
            remove_unfinished(name, &mut run);
            return Err(error);
        }
    }
    let creating = plan.create.is_some();
    for command in &plan.setup {
        if let Err(error) = run(command) {
            if !creating {
                return Err(error);
            }
            remove_unfinished(name, &mut run);
            return Err(error.context(format!(
                "Setting up sbx sandbox {name} failed; the sandbox was removed, launch again"
            )));
        }
    }
    if plan.network_rules != pending {
        write_network_record(state_dir, &plan.network_rules)?;
    }
    // The template's workmux may predate the setup flags this one passes.
    if let Some(command) = &plan.status_hooks
        && let Err(error) = run(command)
    {
        remove_unfinished(name, &mut run);
        return Err(error.context(format!(
            "Installing workmux's status hooks in sbx sandbox {name} failed; the sandbox was \
             removed. If the sandbox image's workmux is older than this one, update it: \
             `workmux sandbox pull` for the published image, or `workmux sandbox install-dev` \
             for a local build"
        )));
    }
    let built_in_failed = format!(
        "workmux's built-in provisioning failed in sbx sandbox {name}; the sandbox was \
         removed. `sbx policy log` lists requests sbx blocked; launch again, or set \
         sandbox.docker_sbx.skip_default_provision if the image provides the toolchain"
    );
    let custom_failed = format!(
        "sandbox.docker_sbx.provision failed in sbx sandbox {name}; the sandbox was removed, \
         fix the script and launch again"
    );
    let provisioning = plan
        .default_provision
        .iter()
        .map(|command| (command, &built_in_failed))
        .chain(
            plan.provision
                .iter()
                .map(|command| (command, &custom_failed)),
        );
    for (command, failed) in provisioning {
        let msg = format!("Provisioning sbx sandbox {name}");
        if let Err(error) = crate::spinner::with_spinner(&msg, || run(command)) {
            remove_unfinished(name, &mut run);
            return Err(error.context(failed.clone()));
        }
    }
    if creating {
        write_created_record(state_dir, &plan.create_settings)?;
    }
    Ok(notices)
}

/// Remove a sandbox this launch couldn't finish setting up. Best effort.
fn remove_unfinished(name: &str, run: &mut impl FnMut(&SbxCommand) -> Result<String>) {
    let remove = SbxCommand {
        args: vec!["rm".to_string(), "-f".to_string(), name.to_string()],
        env: Vec::new(),
    };
    if let Err(e) = run(&remove) {
        warn!(sandbox = %name, error = %e, "failed to remove unfinished sbx sandbox");
    }
}

/// Generate an identifier for one exec's processes in the guest.
pub fn new_session() -> Result<String> {
    Ok(format!(
        "{:016x}",
        getrandom::u64().context("Failed to generate an sbx session id")?
    ))
}

/// Run the plan's `sbx exec` in the foreground and return its exit code.
///
/// sbx leaves the guest command running when the client goes away, so
/// SIGHUP or SIGTERM hangs up the session's guest processes.
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
    // Hang up the guest side whether or not the client already exited.
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

/// Ensure the worktree's sbx sandbox for `agent` exists, creating it if
/// necessary.
///
/// Should be called from the main process BEFORE creating tmux panes, so
/// setup errors and progress appear in the user's terminal. Returns the
/// sandbox name.
pub fn ensure_sandbox(config: &Config, worktree_root: &Path, agent: &str) -> Result<String> {
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
        // How it came to be tells a kit's rule from the user's and workmux's.
        assert_eq!(rules[0].provenance.created_via, "default");
        assert_eq!(rules[1].provenance.created_via, "");
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
    fn created_record_round_trips_and_is_missing_until_written() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_created_record(dir.path()), CreateRecord::Missing);

        let settings = CreateSettings {
            agent: "claude".to_string(),
            agent_mode: DockerSbxAgent::Kit,
            memory: Some("8g".to_string()),
            ..Default::default()
        };
        write_created_record(dir.path(), &settings).unwrap();
        assert_eq!(
            read_created_record(dir.path()),
            CreateRecord::Recorded(Box::new(settings))
        );

        remove_created_record(dir.path()).unwrap();
        assert_eq!(read_created_record(dir.path()), CreateRecord::Missing);
        remove_created_record(dir.path()).unwrap();
    }

    #[test]
    fn a_created_record_missing_a_field_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        for content in [
            "not json",
            r#"{"agent":"claude","agent_config_dir":"/home/me/.claude","template":"t"}"#,
        ] {
            std::fs::write(dir.path().join(CREATED_RECORD_FILE), content).unwrap();
            assert!(
                matches!(read_created_record(dir.path()), CreateRecord::Unreadable(_)),
                "{content}"
            );
        }
    }

    #[test]
    fn removal_finds_each_agents_sandbox_of_the_worktree_by_exact_name() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = temp.path().join("docker-sbx");
        let project = temp.path().join("proj");
        let worktree = temp.path().join("proj__worktrees/a");
        let other = temp.path().join("proj-x__worktrees/b");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let launch = |worktree: &Path, project: &Path, agent: &str| {
            let name = super::super::sandbox_name_in(project, worktree, agent);
            let state_dir = state_root.join(&name);
            std::fs::create_dir_all(&state_dir).unwrap();
            write_sandbox_record(&state_dir, worktree, agent).unwrap();
            name
        };
        let claude = launch(&worktree, &project, "claude");
        let personal = launch(&worktree, &project, "cc-personal");
        let base = claude.strip_suffix("-claude").unwrap();
        // Another worktree's sandbox whose name starts with this worktree's,
        // as one of a project named `proj-<hash8>` would.
        let lookalike = format!("{base}-deadbeef-claude");
        std::fs::create_dir_all(state_root.join(&lookalike)).unwrap();
        write_sandbox_record(&state_root.join(&lookalike), &other, "claude").unwrap();
        let unrecorded = format!("{base}-codex");
        std::fs::create_dir_all(state_root.join(&unrecorded)).unwrap();
        // A state directory named without an agent isn't a sandbox's.
        std::fs::create_dir_all(state_root.join(base)).unwrap();
        launch(&other, &project, "claude");

        let mut found = worktree_sandboxes_in(&state_root, &project, &worktree);
        found.sort();
        let mut expected = vec![claude.clone(), personal.clone()];
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn removal_finds_nothing_without_state() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            worktree_sandboxes_in(&temp.path().join("missing"), temp.path(), temp.path())
                .is_empty()
        );
    }

    #[test]
    fn parses_sbx_diagnose_json() {
        let checks = parse_sbx_checks(
            br#"{"version":"1.0","checks":[{"name":"Authentication","status":"pass","message":"authenticated","detail":"","hint":""},{"name":"Disk space","status":"fail","message":"1.3GiB free","detail":"","hint":"Free some space"}],"summary":{"pass":1,"fail":1}}"#,
        )
        .unwrap();
        assert_eq!(checks[0].name, "Authentication");
        assert_eq!(checks[0].status, "pass");
        assert_eq!(checks[1].message, "1.3GiB free");
        assert_eq!(checks[1].hint, "Free some space");
    }

    #[test]
    fn uninitialised_policy_error_names_sbx_policy_init() {
        let message = sbx_error("error: global network policy has not been initialized\n");
        assert!(message.contains("sbx policy init"), "{message}");
    }

    fn command(words: &str) -> SbxCommand {
        SbxCommand {
            args: words.split(' ').map(str::to_string).collect(),
            env: Vec::new(),
        }
    }

    fn rule() -> NetworkRule {
        NetworkRule {
            decision: super::super::plan::RuleDecision::Allow,
            resource: "example.com:443".to_string(),
        }
    }

    /// A plan that creates the sandbox again, with one setup command, the
    /// status hooks, the built-in provisioning and a provision script.
    fn creating_plan() -> LaunchPlan {
        LaunchPlan {
            sandbox_name: "wm-test".to_string(),
            remove: Some(command("rm -f wm-test")),
            create: Some(super::super::plan::CreateStep {
                command: command("create --name wm-test"),
                workspaces: vec!["/code/proj".to_string()],
            }),
            masks: vec![super::super::mask::Mask::empty(PathBuf::from(
                "/code/proj/.env",
            ))],
            setup: vec![command("setup")],
            status_hooks: Some(command("exec status-hooks")),
            default_provision: vec![command("exec built-in")],
            provision: Some(command("exec custom")),
            exec: command("exec agent"),
            hangup: command("exec hangup"),
            teardown: Vec::new(),
            notes: Vec::new(),
            warnings: Vec::new(),
            widenings: Vec::new(),
            network_rules: vec![rule()],
            create_settings: CreateSettings {
                agent: "claude".to_string(),
                ..Default::default()
            },
        }
    }

    /// Apply `plan` with a fake sbx that fails the command `fail`, returning
    /// the commands run and the result.
    fn apply(
        plan: &LaunchPlan,
        state_dir: &Path,
        fail: &str,
    ) -> (Vec<String>, Result<Vec<String>>) {
        let mut ran = Vec::new();
        let result = apply_plan_with(
            plan,
            state_dir,
            &[],
            || Ok(()),
            |command| {
                let line = command.args.join(" ");
                ran.push(line.clone());
                if line == fail {
                    bail!("sbx {line} failed");
                }
                Ok(String::new())
            },
        );
        (ran, result)
    }

    #[test]
    fn a_created_sandbox_is_recorded_only_once_set_up_and_provisioned() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        write_created_record(state_dir, &CreateSettings::default()).unwrap();
        let plan = creating_plan();

        let mut ran = Vec::new();
        apply_plan_with(
            &plan,
            state_dir,
            &[],
            || Ok(()),
            |command| {
                let line = command.args.join(" ");
                if line.starts_with("create") {
                    // A launch killed from here on is recovered by the next.
                    assert_eq!(read_created_record(state_dir), CreateRecord::Missing);
                    assert!(state_dir.join("masks").is_dir());
                    assert_eq!(read_network_record(state_dir), vec![rule()]);
                }
                ran.push(line);
                Ok(String::new())
            },
        )
        .unwrap();

        assert_eq!(
            ran,
            [
                "rm -f wm-test",
                "create --name wm-test",
                "setup",
                "exec status-hooks",
                "exec built-in",
                "exec custom"
            ]
        );
        assert_eq!(
            read_created_record(state_dir),
            CreateRecord::Recorded(Box::new(plan.create_settings))
        );
        assert_eq!(read_network_record(state_dir), vec![rule()]);
    }

    #[test]
    fn sbxs_warnings_from_a_successful_create_are_passed_on_without_its_progress() {
        // As sbx v0.51 prints them creating an opencode sandbox with no
        // OpenAI secret and no credential binding.
        let stderr = "  \u{25cb} opencode \u{2014} now ships as a kit; resolving docker.io/sbx/opencode:latest\n\
                      \x20 \u{2713} OpenAI \u{2014} using stored OAuth credentials\n\
                      warning: no OpenAI credentials available; opencode will start logged-out.\n\
                      \x20 try: sbx secret set openai --oauth\n\
                      \x20 or:  sbx secret set openai\n\
                      Note: no binding authorizes anthropic, github \u{2014} the credential was not injected. Create a binding (re-run interactively, or edit ~/.config/sbx/credentials.yaml) to use it.\n";
        let dir = tempfile::tempdir().unwrap();
        let plan = creating_plan();

        let notices = apply_plan_with(
            &plan,
            dir.path(),
            &[],
            || Ok(()),
            |command| {
                Ok(if command.args[0] == "create" {
                    stderr.to_string()
                } else {
                    String::new()
                })
            },
        )
        .unwrap();

        assert_eq!(
            notices,
            [
                "warning: no OpenAI credentials available; opencode will start logged-out.\n\
                 \x20 try: sbx secret set openai --oauth\n\
                 \x20 or:  sbx secret set openai",
                "Note: no binding authorizes anthropic, github \u{2014} the credential was not \
                 injected. Create a binding (re-run interactively, or edit \
                 ~/.config/sbx/credentials.yaml) to use it.",
            ]
        );
    }

    #[test]
    fn a_failure_after_create_removes_the_sandbox_it_created() {
        for (fail, message) in [
            ("setup", "Setting up sbx sandbox wm-test failed"),
            ("exec status-hooks", "workmux sandbox install-dev"),
            ("exec built-in", "built-in provisioning failed"),
            ("exec custom", "sandbox.docker_sbx.provision failed"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let state_dir = dir.path();

            let (ran, result) = apply(&creating_plan(), state_dir, fail);

            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains(message), "{fail}: {error}");
            assert_eq!(ran.last().unwrap(), "rm -f wm-test", "{fail}: {ran:?}");
            assert_eq!(
                read_created_record(state_dir),
                CreateRecord::Missing,
                "{fail}"
            );
            // The rules recorded before the failure stay workmux's.
            assert_eq!(read_network_record(state_dir), vec![rule()], "{fail}");
        }
    }

    #[test]
    fn a_failed_create_leaves_the_next_launch_to_create_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();

        let (ran, result) = apply(&creating_plan(), state_dir, "create --name wm-test");

        assert!(result.is_err());
        assert_eq!(ran, ["rm -f wm-test", "create --name wm-test"]);
        assert_eq!(read_created_record(state_dir), CreateRecord::Missing);
    }

    #[test]
    fn a_failure_in_an_existing_sandbox_leaves_it_and_its_records() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        write_created_record(state_dir, &CreateSettings::default()).unwrap();
        let plan = LaunchPlan {
            remove: None,
            create: None,
            masks: Vec::new(),
            status_hooks: None,
            default_provision: Vec::new(),
            provision: None,
            ..creating_plan()
        };

        let (ran, result) = apply(&plan, state_dir, "setup");

        assert!(result.is_err());
        assert_eq!(ran, ["setup"]);
        assert_eq!(
            read_created_record(state_dir),
            CreateRecord::Recorded(Box::default())
        );
    }
}
