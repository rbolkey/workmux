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

use crate::config::Config;

use super::plan::{
    CreateSettings, ExecRequest, LaunchPlan, LaunchTarget, NetworkRule, Platform, SbxCommand,
    SbxState,
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

/// A secret from `sbx secret ls --json`: its service and scope, never its
/// value.
#[derive(Debug, Clone, Deserialize)]
pub struct SbxSecret {
    /// `global`, or the name of the sandbox it's scoped to.
    pub scope: String,
    /// The service, such as `github`.
    pub name: String,
    /// What's stored, never the value: `(stored)` for an API key or token,
    /// `(oauth configured)` for an OAuth login.
    #[serde(default)]
    pub secret: String,
}

impl SbxSecret {
    /// Whether the secret is an OAuth login rather than an API key.
    pub fn is_oauth(&self) -> bool {
        self.secret == "(oauth configured)"
    }
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
struct SbxSecrets {
    #[serde(default)]
    secrets: Vec<SbxSecret>,
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

/// Parse `sbx secret ls --json`. It lists services and scopes, never values.
pub fn parse_sbx_secrets(stdout: &[u8]) -> Result<Vec<SbxSecret>> {
    Ok(serde_json::from_slice::<SbxSecrets>(stdout)
        .context("Failed to parse sbx secret ls output")?
        .secrets)
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

/// Run `sbx diagnose --json`. It exits 1 when any check fails, disk space
/// included, but still prints every check: read them whatever the exit
/// status, so the plan can name the failing one.
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

/// A worktree's sbx sandbox about to be deleted with it, locked so that no
/// launch sets it up in the meantime.
pub struct SandboxRemoval {
    name: String,
    state_dir: PathBuf,
    _lock: Flock<File>,
}

/// Prepare to delete a worktree's sbx sandbox, `name` (see
/// [`super::sandbox_name_in`]), if workmux created one for it (its state
/// directory exists): wait for a launch setting it up to finish, then hold
/// its lock until [`SandboxRemoval::remove`], or until the removal is
/// called off and the value dropped.
///
/// Called whenever a worktree is removed, before the worktree leaves its
/// path, which a launch setting the sandbox up still needs. Best effort:
/// failures are logged, never fatal.
pub fn prepare_sandbox_removal(name: &str) -> Option<SandboxRemoval> {
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
            eprintln!("Warning: failed to remove sbx sandbox {name}: {e:#}");
            None
        }
    }
}

impl SandboxRemoval {
    /// Delete the sandbox and its state directory, once nothing can stop
    /// the worktree's removal. Best effort: failures are logged, never
    /// fatal.
    pub fn remove(self) {
        let name = &self.name;
        info!(sandbox = %name, "removing sbx sandbox");
        if let Err(e) = SbxInstance::remove_by_name(name) {
            warn!(sandbox = %name, error = %e, "failed to remove sbx sandbox");
            eprintln!("Warning: failed to remove sbx sandbox {name}: {e}");
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.state_dir) {
            warn!(sandbox = %name, error = %e, "failed to clean up state dir");
        }
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

/// The sandbox `name` from `sbx ls`, and its rules (the global ones while
/// it doesn't exist).
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
    // An existing sandbox's rules include the global ones; before it exists,
    // the global ones are what it will get. Listing fails before the global
    // policy is initialised, which the plan reports. Otherwise the plan
    // needs the rules to keep `deny` from being widened: fail closed.
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

/// Secret names and scopes from `sbx secret ls`, never values, or sbx's
/// error when listing them failed. The credential warnings are advisory
/// and leave a failure out; placeholder agent credentials fail on it.
fn list_secrets() -> Result<(Vec<SbxSecret>, Option<String>)> {
    match sbx_json(&["secret", "ls"]) {
        Ok(stdout) => Ok((parse_sbx_secrets(&stdout)?, None)),
        Err(e) => {
            debug!(error = %e, "failed to list sbx secrets");
            Ok((Vec::new(), Some(e.to_string())))
        }
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
    let platform = current_platform();
    // The listings are independent but for the rules, which depend on
    // whether the sandbox exists; run them side by side.
    let (checks, sandbox_and_rules, secrets) = std::thread::scope(|scope| {
        let checks = scope.spawn(sbx_diagnose);
        let secrets = scope.spawn(list_secrets);
        let sandbox_and_rules = list_sandbox_and_rules(name);
        (
            checks.join().expect("sbx diagnose thread panicked"),
            sandbox_and_rules,
            secrets.join().expect("sbx secret ls thread panicked"),
        )
    });
    let checks = checks?;
    let (sandbox, rules) = sandbox_and_rules?;
    let (secrets, secrets_error) = secrets?;
    Ok(SbxState {
        platform,
        checks,
        sandbox,
        rules,
        secrets,
        secrets_error,
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

/// File in a sandbox's state directory recording the settings it was
/// created with, written once creating it fully succeeded. A sandbox
/// without it was left half set up by a launch that failed or was killed.
const CREATED_RECORD_FILE: &str = "created.json";

/// The settings a sandbox was created with, or `None` if no launch
/// finished setting it up (or the record is corrupted).
fn read_created_record(state_dir: &Path) -> Option<CreateSettings> {
    let path = state_dir.join(CREATED_RECORD_FILE);
    let content = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&content) {
        Ok(settings) => Some(settings),
        Err(e) => {
            warn!(?path, error = %e, "corrupted sbx create record, ignoring");
            None
        }
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

/// File in a sandbox's state directory marking it as being created, from
/// just before `sbx create` until the create record is written. A sandbox
/// with neither predates the record.
const CREATING_MARKER_FILE: &str = "creating";

fn remove_state_file(state_dir: &Path, file: &str) -> Result<()> {
    match std::fs::remove_file(state_dir.join(file)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Lock file in a sandbox's state directory.
const LOCK_FILE: &str = ".lock";

/// Hold an exclusive lock on a sandbox's state directory while a launch
/// sets the sandbox up, so concurrent launches into one worktree's sandbox
/// (the panes of `target: all`) neither create it twice nor remove one that
/// another is still setting up, and removing the worktree waits for them.
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
/// load the template, write its masks' sources and create the sandbox if
/// it's missing, reconcile its
/// policy rules, recording the ones workmux writes for `sandbox.network`
/// and the toolchain, and provision a sandbox it created (built-in
/// provisioning, then the `provision` script), removing it again if
/// provisioning fails. Returns the plan, whose `exec` the caller runs.
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
    // sbx mounts only directories that exist.
    if let Some(config_dir) = config.sandbox.resolved_agent_config_dir(agent) {
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
    let unfinished = state_dir.join(CREATING_MARKER_FILE).exists();
    // Only creating the sandbox mounts the git metadata; finding it walks
    // the worktree.
    let git = if super::plan::creates_sandbox(&state, created_with.as_ref(), unfinished) {
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
        created_with: created_with.as_ref(),
        unfinished,
    };
    let plan = super::launch_plan(&config.sandbox, &target, agent, &state, exec)?;
    for note in &plan.notes {
        info!(sandbox = %name, note, "sbx network note");
        eprintln!("note: {note}");
    }
    for warning in &plan.warnings {
        warn!(sandbox = %name, warning, "sbx launch warning");
        eprintln!("warning: {warning}");
    }

    let creating = plan.create_template.is_some();
    let image = config.sandbox.resolved_image(agent);
    if creating {
        super::template::ensure_template(&image)?;
        if let Some(remove) = &plan.remove {
            let why = if unfinished {
                "an earlier launch didn't finish setting it up"
            } else {
                "it predates workmux's record of how it was created"
            };
            info!(sandbox = %name, why, "removing sbx sandbox to create it again");
            eprintln!("Removing sbx sandbox {name}, as {why}; creating it again");
            run_sbx(remove)?;
        }
        remove_created_record(&state_dir)?;
        std::fs::write(state_dir.join(CREATING_MARKER_FILE), b"")
            .context("Failed to mark the sbx sandbox as being created")?;
        super::mask::write_masks(&state_dir, &plan.masks)?;
        info!(sandbox = %name, "creating sbx sandbox");
        eprintln!("  Workspaces:");
        if let Some(create) = plan.setup.first() {
            for workspace in create.args.iter().skip_while(|a| *a != "shell").skip(1) {
                eprintln!("    {workspace}");
            }
        }
    } else {
        // The template matters only when a sandbox is created; keep its
        // freshness cache current for the next one.
        super::template::check_freshness_in_background(&image);
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
    // Everything after `sbx create` finishes a sandbox this launch creates:
    // when any of it fails, remove the sandbox so the next launch creates
    // and provisions it again.
    let remove_unfinished = |error: anyhow::Error| {
        if let Err(rm) = SbxInstance::remove_by_name(&name) {
            warn!(sandbox = %name, error = %rm, "failed to remove unfinished sbx sandbox");
        }
        error
    };
    for (i, command) in plan.setup.iter().enumerate() {
        if i == 0 && creating {
            let msg = format!("Creating sbx sandbox {name}");
            crate::spinner::with_spinner(&msg, || run_sbx(command))?;
        } else if creating {
            run_sbx(command)
                .map_err(remove_unfinished)
                .with_context(|| {
                    format!(
                        "Setting up sbx sandbox {name} failed; the sandbox was removed, \
                     launch again"
                    )
                })?;
        } else {
            run_sbx(command)?;
        }
    }
    if plan.network_rules != pending {
        write_network_record(&state_dir, &plan.network_rules)?;
    }
    let provision = |command: &SbxCommand| {
        let msg = format!("Provisioning sbx sandbox {name}");
        crate::spinner::with_spinner(&msg, || run_sbx(command)).map_err(remove_unfinished)
    };
    for command in &plan.default_provision {
        provision(command).context(format!(
            "workmux's built-in provisioning failed in sbx sandbox {name}; the sandbox \
             was removed. `sbx policy log` lists requests sbx blocked; launch again, or set \
             sandbox.docker_sbx.skip_default_provision if the image provides the toolchain"
        ))?;
    }
    if let Some(command) = &plan.provision {
        provision(command).context(format!(
            "sandbox.docker_sbx.provision failed in sbx sandbox {name}; \
             the sandbox was removed, fix the script and launch again"
        ))?;
    } else if !creating && config.sandbox.docker_sbx.provision_script().is_some() {
        info!(sandbox = %name, "custom provision script only runs on sandbox creation; recreate sandbox to apply changes");
    }
    if creating {
        write_created_record(&state_dir, &plan.create_settings)?;
        remove_state_file(&state_dir, CREATING_MARKER_FILE)?;
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
    fn parses_sbx_secret_ls_json_as_services_and_scopes() {
        let secrets = parse_sbx_secrets(
            br#"{"secrets":[{"scope":"global","type":"service","name":"anthropic","secret":"(oauth configured)"},
                            {"scope":"wm-proj-1","type":"service","name":"github","secret":"(stored)"}],
                 "custom_secrets":[],"shadowed_services":[],"env_only_count":0}"#,
        )
        .unwrap();
        let services: Vec<(&str, &str)> = secrets
            .iter()
            .map(|s| (s.scope.as_str(), s.name.as_str()))
            .collect();
        assert_eq!(
            services,
            vec![("global", "anthropic"), ("wm-proj-1", "github")]
        );
        assert!(secrets[0].is_oauth());
        assert!(!secrets[1].is_oauth());
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
        assert_eq!(read_created_record(dir.path()), None);

        let settings = CreateSettings {
            agent: "claude".to_string(),
            memory: Some("8g".to_string()),
            ..Default::default()
        };
        write_created_record(dir.path(), &settings).unwrap();
        assert_eq!(read_created_record(dir.path()), Some(settings));

        remove_created_record(dir.path()).unwrap();
        assert_eq!(read_created_record(dir.path()), None);
        remove_created_record(dir.path()).unwrap();
    }

    #[test]
    fn records_from_before_placeholders_read_as_credential_mounts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CREATED_RECORD_FILE),
            r#"{"agent":"claude","template":"t","toolchain":"none"}"#,
        )
        .unwrap();
        let record = read_created_record(dir.path()).unwrap();
        assert_eq!(
            record.agent_credentials,
            crate::config::AgentCredentials::Mount
        );
        assert!(record.credential_placeholders.is_empty());
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
}
