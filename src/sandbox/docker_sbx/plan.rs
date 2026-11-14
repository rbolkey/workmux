//! The launch plan: what a pane launch asks of sbx.
//!
//! [`launch_plan`] is pure, but for checking whether a writable extra mount
//! is a file. Callers gather sbx's state (see
//! [`super::instance::gather_state`]), compute the plan, run its setup
//! commands, then run its exec in the foreground.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::SandboxConfig;
use crate::shell::shell_quote;

use super::instance::{SbxCheck, SbxRule, SbxSandboxInfo};

/// The host sbx runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `std::env::consts::OS`
    pub os: String,
    /// `std::env::consts::ARCH`
    pub arch: String,
    /// Whether `/dev/kvm` exists (Linux only)
    pub kvm: bool,
}

/// What sbx reports about a worktree's sandbox, as parsed data.
#[derive(Debug, Clone)]
pub struct SbxState {
    pub platform: Platform,
    /// Checks from `sbx diagnose --json`.
    pub checks: Vec<SbxCheck>,
    /// The sandbox from `sbx ls --json`, or `None` when it doesn't exist.
    pub sandbox: Option<SbxSandboxInfo>,
    /// Rules from `sbx policy ls <name> --json`, or the global ones from
    /// `sbx policy ls --json` when the sandbox is missing.
    pub rules: Vec<SbxRule>,
}

/// The worktree a launch is for, with the host paths the plan mounts.
#[derive(Debug, Clone)]
pub struct LaunchTarget<'a> {
    /// The worktree's sandbox name (see [`super::sandbox_name`]).
    pub sandbox_name: &'a str,
    pub worktree_root: &'a Path,
    /// The pane's working directory, under `worktree_root`.
    pub pane_cwd: &'a Path,
    /// The git metadata the worktree uses.
    pub git: &'a GitMetadata,
    /// OpenCode's global config directory (`~/.config/opencode/`), when it
    /// exists; mounted read-only for the opencode agent.
    pub opencode_config_dir: Option<&'a Path>,
    /// Host-side state directory for the sandbox (shims live under it).
    pub state_dir: &'a Path,
    /// The network rules workmux last wrote for the sandbox (see
    /// [`LaunchPlan::network_rules`]).
    pub network_record: &'a [NetworkRule],
}

/// Whether an sbx rule allows or denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleDecision {
    Allow,
    Deny,
}

impl RuleDecision {
    /// sbx's name for the decision, as in `sbx policy allow|deny`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// A network rule workmux keeps on a sandbox for `sandbox.network`:
/// `sbx policy <decision> network --sandbox <name> <resource>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkRule {
    pub decision: RuleDecision,
    pub resource: String,
}

/// A linked worktree's git metadata, as found on the host (see
/// [`super::git::git_metadata`]).
#[derive(Debug, Clone, Default)]
pub struct GitMetadata {
    /// The main repository's git dir.
    pub common_dir: PathBuf,
    /// The worktree's own admin dir, `<common_dir>/worktrees/<name>`.
    pub admin_dir: PathBuf,
    /// `.git` pointer files under the worktree: its own and its submodules'.
    pub pointer_files: Vec<PathBuf>,
    /// Submodule git dirs under `<common_dir>/modules`.
    pub module_roots: Vec<PathBuf>,
    /// Files the repository config includes or runs, inside the worktree or
    /// the git dir.
    pub config_paths: Vec<PathBuf>,
    /// Whether `<common_dir>/rr-cache` exists.
    pub rr_cache: bool,
}

/// The command and environment to run in the sandbox.
#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    /// The command: one argument is a shell command line, as a pane's is;
    /// several are its words, each passed on as is.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Identifies the exec's processes in the guest, so they can be hung up.
    pub session: String,
    /// The port of the supervisor's RPC server on the workstation's
    /// loopback, when it runs one.
    pub rpc_port: Option<u16>,
    /// Whether stdin is a terminal. sbx's `exec -t` fails without one.
    pub tty: bool,
}

/// One `sbx` invocation. Environment values are passed through the process
/// environment and named with a bare `-e KEY`, so they never appear in argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbxCommand {
    /// Arguments after `sbx`.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The ordered sbx commands for one launch.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub sandbox_name: String,
    /// Template the sandbox is created from, when the plan creates it.
    pub create_template: Option<String>,
    /// Commands to run, in order, before `exec`.
    pub setup: Vec<SbxCommand>,
    /// The interactive exec that runs the command.
    pub exec: SbxCommand,
    /// Hangs up the exec's processes in the guest. sbx leaves them running
    /// when the `sbx exec` client goes away, as it does when a pane closes.
    pub hangup: SbxCommand,
    /// Commands to run after `exec` ends: removing the RPC rule.
    pub teardown: Vec<SbxCommand>,
    /// Notes for the user: settings sbx can't enforce that only limit access,
    /// and what sbx allows beyond them.
    pub notes: Vec<String>,
    /// The `sandbox.network` rules workmux has written for the sandbox once
    /// `setup` has run; the record the next launch reconciles against.
    pub network_rules: Vec<NetworkRule>,
}

/// Home directory of sbx's `agent` user in the guest.
const GUEST_HOME: &str = "/home/agent";

/// The image's system PATH.
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Environment variable marking the processes of one exec in the guest.
const SESSION_ENV: &str = "WM_SBX_SESSION";

/// sbx's name for the workstation in policy rules.
const WORKSTATION_HOST: &str = "localhost";

/// Where an agent's config directory is linked under the guest home, or
/// `None` for agents whose config directory workmux doesn't mount.
fn agent_home_subpath(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some(".claude"),
        "copilot" => Some(".copilot"),
        "gemini" => Some(".gemini"),
        "agy" => Some(".gemini/antigravity-cli"),
        "codex" => Some(".codex"),
        "opencode" => Some(".local/share/opencode"),
        "pi" => Some(".pi/agent"),
        "omp" => Some(".omp/agent"),
        _ => None,
    }
}

/// The subdirectory of an agent's config directory it downloads binaries
/// into, and the guest-local directory that takes its place.
fn guest_binaries_dir(agent: &str) -> Option<(&'static str, &'static str)> {
    match agent {
        "pi" => Some(("bin", "/var/lib/workmux/pi-agent-bin")),
        "omp" => Some(("tools", "/var/lib/workmux/omp-agent-tools")),
        _ => None,
    }
}

/// The agent's host config directory and where it's linked under the guest
/// home, for agents whose config directory workmux mounts.
fn config_dir_and_home_subpath(
    config: &SandboxConfig,
    agent: &str,
) -> Option<(PathBuf, &'static str)> {
    let subpath = agent_home_subpath(agent)?;
    Some((config.resolved_agent_config_dir(agent)?, subpath))
}

/// A command run as root in the sandbox.
fn root_exec(name: &str, command: Vec<String>) -> SbxCommand {
    let mut args = words(&["exec", "-u", "root", name]);
    args.extend(command);
    SbxCommand {
        args,
        env: Vec::new(),
    }
}

/// Link a host directory, mounted at its host path, to `subpath` under the
/// guest home. Parent directories are created first and owned by the agent,
/// so it can still write beside the link (`~/.local/state`).
fn link_into_home(name: &str, host_dir: &Path, subpath: &str) -> Vec<SbxCommand> {
    let mut commands = Vec::new();
    let parents: Vec<String> = Path::new(subpath)
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| format!("{GUEST_HOME}/{}", p.display()))
        .collect();
    if !parents.is_empty() {
        let mut command = words(&["install", "-d", "-o", "agent", "-g", "agent"]);
        command.extend(parents.into_iter().rev());
        commands.push(root_exec(name, command));
    }
    let mut command = words(&["ln", "-sfnT"]);
    command.push(host_dir.to_string_lossy().into_owned());
    command.push(format!("{GUEST_HOME}/{subpath}"));
    commands.push(root_exec(name, command));
    commands
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// The interactive `sbx exec` that runs the command in the pane's working
/// directory, with a terminal when stdin is one. Values travel in the
/// process environment; argv names them only.
fn exec_command(target: &LaunchTarget, exec: &ExecRequest) -> SbxCommand {
    let mut env = exec.env.clone();
    env.push((SESSION_ENV.to_string(), exec.session.clone()));
    let interactive = if exec.tty { "-it" } else { "-i" };
    let mut args = words(&["exec", interactive, "-w"]);
    args.push(target.pane_cwd.to_string_lossy().into_owned());
    for (key, _) in &env {
        args.push("-e".to_string());
        args.push(key.clone());
    }
    // Put the shims first. PATH goes by value: in sbx's own environment it
    // would also change where sbx itself is looked up.
    args.push("-e".to_string());
    args.push(format!(
        "PATH={}:{GUEST_HOME}/.local/bin:{GUEST_PATH}",
        target.state_dir.join("shims/bin").display()
    ));
    args.push(target.sandbox_name.to_string());
    // bash, so BASH_ENV brings in the environment sbx keeps in
    // /etc/sandbox-persistent.sh.
    let command = match exec.command.as_slice() {
        [line] => line.clone(),
        words => words
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" "),
    };
    args.extend(words(&["bash", "-c", &command]));
    SbxCommand { args, env }
}

/// Send SIGHUP to every guest process that carries the exec's session
/// marker, as a closing terminal would: the command and what it started.
fn hangup_command(target: &LaunchTarget, exec: &ExecRequest) -> SbxCommand {
    let script = format!(
        "for p in /proc/[0-9]*; do \
         grep -qxzF {SESSION_ENV}={} \"$p/environ\" 2>/dev/null \
         && kill -HUP \"${{p#/proc/}}\"; done; true",
        exec.session
    );
    SbxCommand {
        args: words(&["exec", target.sandbox_name, "sh", "-c", &script]),
        env: Vec::new(),
    }
}

/// Workspaces that keep the main repository's git metadata read-only, as the
/// container backend does: the git dir is read-only, the parts commits write
/// (objects, refs, logs, the worktree's admin dir, submodule git dirs) are
/// writable inside it, and their control files are read-only again. sbx
/// mounts parents before children, and read-only mounts hold even for root.
fn git_workspaces(git: &GitMetadata) -> Vec<String> {
    let rw = |path: PathBuf| path.to_string_lossy().into_owned();
    let ro = |path: PathBuf| format!("{}:ro", path.display());
    let common = &git.common_dir;

    let mut workspaces = vec![ro(common.clone())];
    for dir in ["objects", "refs", "logs"] {
        workspaces.push(rw(common.join(dir)));
    }
    if git.rr_cache {
        workspaces.push(rw(common.join("rr-cache")));
    }
    workspaces.push(rw(git.admin_dir.clone()));
    workspaces.extend(git.module_roots.iter().cloned().map(rw));

    workspaces.push(ro(common.join("objects/info")));
    for name in ["config.worktree", "gitdir", "commondir"] {
        workspaces.push(ro(git.admin_dir.join(name)));
    }
    for root in &git.module_roots {
        for name in [
            "hooks",
            "info",
            "objects/info",
            "modules",
            "worktrees",
            "config",
            "config.worktree",
        ] {
            workspaces.push(ro(root.join(name)));
        }
    }
    workspaces.extend(git.pointer_files.iter().cloned().map(ro));
    workspaces.extend(git.config_paths.iter().cloned().map(ro));
    workspaces
}

/// `sandbox.extra_mounts` as workspaces, read-only unless writable. sbx
/// mounts a host path only at that same path, and a file only read-only.
fn extra_mount_workspaces(config: &SandboxConfig, target: &LaunchTarget) -> Result<Vec<String>> {
    let protected = [
        target.worktree_root.join(".git"),
        target.git.admin_dir.clone(),
        target.git.common_dir.clone(),
    ];
    let mut workspaces = Vec::new();
    for mount in config.extra_mounts() {
        let (host, guest, read_only) = mount.resolve()?;
        if guest != host {
            bail!(
                "sandbox.extra_mounts entry {} sets guest_path {} but the docker_sbx \
                 backend mounts a host path only at that same path. Remove guest_path, \
                 or use the container or lima backend.",
                host.display(),
                guest.display()
            );
        }
        if read_only {
            workspaces.push(format!("{}:ro", host.display()));
            continue;
        }
        // Checking the host is the plan's only look at the filesystem.
        if host.is_file() {
            bail!(
                "sandbox.extra_mounts entry {} is a writable file but the docker_sbx \
                 backend mounts files only read-only. Remove writable: true, or mount \
                 its directory instead.",
                host.display()
            );
        }
        if protected
            .iter()
            .any(|path| guest.starts_with(path) || path.starts_with(&guest))
        {
            bail!(
                "Writable extra mount at {} overlaps protected Git metadata",
                guest.display()
            );
        }
        workspaces.push(host.to_string_lossy().into_owned());
    }
    Ok(workspaces)
}

/// `sandbox.network` as rules scoped to the sandbox. Under `deny`, port 443
/// to each allowed domain, as workmux's CONNECT proxy allows on the
/// container backend; under `allow`, none.
///
/// workmux's `*.d` matches subdomains at any depth but not `d`; sbx's `*.d`
/// matches one label and its `**.d` any depth and `d` too. So `*.d` becomes
/// `**.d` with a deny for `d`, unless `d` is itself allowed.
fn network_rules(config: &SandboxConfig) -> Vec<NetworkRule> {
    if !config.network_policy_is_deny() {
        return Vec::new();
    }
    let hosts: Vec<String> = config
        .network
        .allowed_domains()
        .iter()
        .map(|entry| entry.host().to_ascii_lowercase())
        .collect();
    let allowed = |host: &str| {
        hosts.iter().any(|pattern| match pattern.strip_prefix('*') {
            Some(suffix) => host.ends_with(suffix),
            None => host == pattern,
        })
    };

    let mut rules = Vec::new();
    let mut add = |decision, resource: String| {
        let rule = NetworkRule { decision, resource };
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    };
    for host in &hosts {
        match host.strip_prefix("*.") {
            Some(base) => {
                add(RuleDecision::Allow, format!("**.{base}:443"));
                if !allowed(base) {
                    add(RuleDecision::Deny, format!("{base}:443"));
                }
            }
            None => add(RuleDecision::Allow, format!("{host}:443")),
        }
    }
    rules
}

/// What sbx enforces differently from `sandbox.network` without widening
/// it: settings it has no equivalent for, and what the user's global sbx
/// policy allows besides (workmux never writes it, and a sandbox's allow
/// can't take a global one back).
fn network_notes(config: &SandboxConfig, state: &SbxState, name: &str) -> Vec<String> {
    if !config.network_policy_is_deny() {
        return Vec::new();
    }
    let mut notes = Vec::new();
    if config.network.max_connections.is_some() {
        notes.push(
            "sandbox.network.max_connections is ignored under the docker_sbx backend: \
             sbx doesn't cap concurrent connections."
                .to_string(),
        );
    }
    let global_allows: usize = state
        .rules
        .iter()
        .filter(|rule| {
            rule.scope == "global" && rule.decision == "allow" && rule.resource_type == "network"
        })
        .map(|rule| rule.resources.len())
        .sum();
    if global_allows > 0 {
        notes.push(format!(
            "sbx's global network policy also allows {global_allows} destinations \
             besides sandbox.network.allowed_domains; `sbx policy ls {name}` lists them. \
             workmux never changes the global policy."
        ));
    }
    notes
}

/// The `sbx policy` command adding or removing a rule scoped to the sandbox.
fn policy_command(name: &str, verb: &str, resource: &str) -> SbxCommand {
    let mut args = words(&["policy", verb, "network", "--sandbox", name]);
    if verb == "rm" {
        args.extend(words(&["--resource", resource, "-f"]));
    } else {
        args.push(resource.to_string());
    }
    SbxCommand {
        args,
        env: Vec::new(),
    }
}

/// Whether sbx's diagnostics report `name` as failed.
fn check_failed(state: &SbxState, name: &str) -> bool {
    state
        .checks
        .iter()
        .any(|check| check.name == name && check.status == "fail")
}

/// Fail closed unless sbx can run sandboxes. workmux never runs `sbx login`
/// or `sbx policy init` itself.
fn check_prerequisites(state: &SbxState) -> Result<()> {
    let platform = &state.platform;
    let supported = match platform.os.as_str() {
        "macos" => platform.arch == "aarch64",
        "linux" => platform.kvm,
        "windows" => true,
        _ => false,
    };
    if !supported {
        bail!(
            "Docker Sandboxes doesn't support this host ({} on {}). sbx runs on macOS 14+ \
             on Apple silicon, Windows 11, and Ubuntu 24.04+ with KVM.\n\
             Use the container or lima sandbox backend instead.",
            platform.os,
            platform.arch
        );
    }
    if check_failed(state, "Authentication") {
        bail!(
            "sbx is not logged in. Sign in to Docker with `sbx login`, then try again.\n\
             Or disable sandbox: set 'sandbox.enabled: false' in config."
        );
    }
    if check_failed(state, "Global network policy") {
        bail!(
            "sbx's global network policy has not been initialized. Choose one with \
             `sbx policy init`, then try again; workmux never picks it for you."
        );
    }
    Ok(())
}

/// Compute the sbx commands a launch needs.
///
/// When the sandbox is missing: create it from the template with sbx's
/// generic `shell` agent (agent kits would write into the mounted config
/// directory), mounting the worktree, the main repository's git dir
/// (read-only but for what commits write), the agent's config directory,
/// the shims and `extra_mounts`, all at their host paths; then,
/// as root, link the config directory to where the agent looks for it and
/// seed Claude's onboarding state. On every launch: bind pi's and OMP's
/// binaries directory to a guest-local one, reconcile the sandbox's rules
/// with `sandbox.network`, and allow the supervisor's RPC port until the
/// exec ends.
pub fn launch_plan(
    config: &SandboxConfig,
    target: &LaunchTarget,
    agent: &str,
    state: &SbxState,
    exec: &ExecRequest,
) -> Result<LaunchPlan> {
    check_prerequisites(state)?;
    if !config.container.excluded_files().is_empty() {
        bail!(
            "sandbox.container.excluded_files is set but the docker_sbx backend \
             can't mask files: sbx mounts a host path only at that same path. \
             Secrets would remain readable inside the sandbox. Use the container \
             backend with docker or podman, or remove sandbox.container.excluded_files."
        );
    }

    config
        .network
        .validate()
        .context("Invalid sandbox network config")?;
    let extra_mounts = extra_mount_workspaces(config, target)?;

    let name = target.sandbox_name;
    let mut setup = Vec::new();
    let mut create_template = None;

    if state.sandbox.is_none() {
        let template = super::template::template_tag(&config.resolved_image(agent));
        let mut args = words(&[
            "create",
            "--name",
            name,
            "--no-progress",
            "--template",
            &template,
            "--pull",
            "never",
            "shell",
        ]);
        args.push(target.worktree_root.to_string_lossy().into_owned());
        args.extend(git_workspaces(target.git));
        let config_dir = config_dir_and_home_subpath(config, agent);
        if let Some((config_dir, _)) = &config_dir {
            args.push(config_dir.to_string_lossy().into_owned());
        }
        let opencode_config_dir = target.opencode_config_dir.filter(|_| agent == "opencode");
        if let Some(dir) = opencode_config_dir {
            args.push(format!("{}:ro", dir.display()));
        }
        args.push(format!("{}:ro", target.state_dir.display()));
        args.extend(extra_mounts);
        setup.push(SbxCommand {
            args,
            env: Vec::new(),
        });
        create_template = Some(template);

        // sbx mounts workspaces only at their host paths: link the agent's
        // config directory to where the agent looks for it.
        if let Some((config_dir, subpath)) = config_dir {
            setup.extend(link_into_home(name, &config_dir, subpath));
        }
        // OpenCode's global config (opencode.json, plugins, global MCP
        // definitions) lives apart from its data directory.
        if let Some(dir) = opencode_config_dir {
            setup.extend(link_into_home(name, dir, ".config/opencode"));
        }

        // Claude keeps its onboarding state in ~/.claude.json, outside the
        // config directory. Seed the sandbox's own copy, as Lima seeds each
        // VM's, so Claude doesn't run onboarding in every sandbox.
        if agent == "claude" {
            let mut command = words(&[
                "sh",
                "-c",
                "[ -e \"$1\" ] || { printf %s \"$2\" > \"$1\" && chown agent:agent \"$1\"; }",
                "sh",
            ]);
            command.push(format!("{GUEST_HOME}/.claude.json"));
            command.push(crate::sandbox::CLAUDE_ONBOARDING_JSON.to_string());
            setup.push(root_exec(name, command));
        }
    }

    // Pi and OMP download platform-specific fd/rg binaries into their config
    // directory, which would overwrite the workstation's own through the
    // mount. Bind a guest-local directory over it, as the container backend
    // overlays pi's bin/. A guest mount ends when sbx stops the sandbox, so
    // every launch makes it.
    if let Some((subdir, guest_dir)) = guest_binaries_dir(agent)
        && let Some((config_dir, _)) = config_dir_and_home_subpath(config, agent)
    {
        let mut command = words(&[
            "sh",
            "-c",
            "mountpoint -q \"$1\" || { install -d -o agent -g agent \"$2\" \
             && mkdir -p \"$1\" && mount --bind \"$2\" \"$1\"; }",
            "sh",
        ]);
        command.push(config_dir.join(subdir).to_string_lossy().into_owned());
        command.push(guest_dir.to_string());
        setup.push(root_exec(name, command));
    }

    // Reconcile `sandbox.network` with the sandbox's rules. Only rules
    // workmux wrote (its record) are ever removed; stale ones go first, as
    // sbx refuses a rule while its scope holds the opposite decision for
    // the same resource. A rule the user already had stays theirs.
    let sandbox_scope = format!("sandbox:{name}");
    let present = |rule: &NetworkRule| {
        state.sandbox.is_some()
            && state.rules.iter().any(|r| {
                r.scope == sandbox_scope
                    && r.decision == rule.decision.as_str()
                    && r.resources.contains(&rule.resource)
            })
    };
    let wanted = network_rules(config);
    for rule in target.network_record {
        if !wanted.contains(rule) && present(rule) {
            setup.push(policy_command(name, "rm", &rule.resource));
        }
    }
    let mut network_rules = Vec::new();
    for rule in wanted {
        if !present(&rule) {
            setup.push(policy_command(name, rule.decision.as_str(), &rule.resource));
            network_rules.push(rule);
        } else if target.network_record.contains(&rule) {
            network_rules.push(rule);
        }
    }

    // A rule scoped to the sandbox lets the guest reach the supervisor's RPC
    // server through host.docker.internal; sbx names the workstation
    // `localhost`. Only its port, and only while the supervisor runs: the
    // rule is removed when the exec ends, unless it was already there.
    let mut teardown = Vec::new();
    if let Some(port) = exec.rpc_port {
        let rpc_rule = NetworkRule {
            decision: RuleDecision::Allow,
            resource: format!("{WORKSTATION_HOST}:{port}"),
        };
        if !present(&rpc_rule) {
            setup.push(policy_command(name, "allow", &rpc_rule.resource));
            teardown.push(policy_command(name, "rm", &rpc_rule.resource));
        }
    }

    Ok(LaunchPlan {
        sandbox_name: name.to_string(),
        create_template,
        setup,
        exec: exec_command(target, exec),
        hangup: hangup_command(target, exec),
        teardown,
        notes: network_notes(config, state, name),
        network_rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AllowedDomainDetails, AllowedDomainEntry, ExtraMount, NetworkConfig, NetworkPolicy,
    };
    use std::path::PathBuf;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn config() -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            backend: Some(crate::config::SandboxBackend::DockerSbx),
            image: Some("test-image:latest".to_string()),
            agent_config_dir: Some("/home/me/.{agent}".to_string()),
            ..Default::default()
        }
    }

    fn passing_checks() -> Vec<SbxCheck> {
        [
            ("Authentication", "pass"),
            ("Global network policy", "pass"),
        ]
        .into_iter()
        .map(|(name, status)| SbxCheck {
            name: name.to_string(),
            status: status.to_string(),
        })
        .collect()
    }

    fn missing() -> SbxState {
        SbxState {
            platform: Platform {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                kvm: false,
            },
            checks: passing_checks(),
            sandbox: None,
            rules: Vec::new(),
        }
    }

    fn sandbox(status: &str) -> SbxSandboxInfo {
        SbxSandboxInfo {
            name: "wm-proj-12345678".to_string(),
            status: status.to_string(),
            created_at: None,
            last_used_at: None,
        }
    }

    struct Paths {
        worktree: PathBuf,
        cwd: PathBuf,
        git: GitMetadata,
        state: PathBuf,
        record: Vec<NetworkRule>,
    }

    fn git_metadata() -> GitMetadata {
        GitMetadata {
            common_dir: PathBuf::from("/code/proj/.git"),
            admin_dir: PathBuf::from("/code/proj/.git/worktrees/feat"),
            pointer_files: vec![PathBuf::from("/code/proj__worktrees/feat/.git")],
            ..Default::default()
        }
    }

    fn paths() -> Paths {
        Paths {
            worktree: PathBuf::from("/code/proj__worktrees/feat"),
            cwd: PathBuf::from("/code/proj__worktrees/feat/sub"),
            git: git_metadata(),
            state: PathBuf::from("/state/docker-sbx/wm-proj-12345678"),
            record: Vec::new(),
        }
    }

    fn target(p: &Paths) -> LaunchTarget<'_> {
        LaunchTarget {
            sandbox_name: "wm-proj-12345678",
            worktree_root: &p.worktree,
            pane_cwd: &p.cwd,
            git: &p.git,
            opencode_config_dir: None,
            state_dir: &p.state,
            network_record: &p.record,
        }
    }

    fn exec() -> ExecRequest {
        ExecRequest {
            command: args(&["claude --dangerously-skip-permissions"]),
            env: vec![
                ("WM_RPC_PORT".to_string(), "4242".to_string()),
                ("WM_RPC_TOKEN".to_string(), "secret".to_string()),
            ],
            session: "0123456789abcdef".to_string(),
            rpc_port: Some(4242),
            tty: true,
        }
    }

    #[test]
    fn missing_sandbox_is_created_from_the_template_with_the_shell_agent() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let tag = super::super::template::template_tag("test-image:latest");
        assert_eq!(plan.create_template.as_deref(), Some(tag.as_str()));
        assert_eq!(
            plan.setup[0].args[..10],
            args(&[
                "create",
                "--name",
                "wm-proj-12345678",
                "--no-progress",
                "--template",
                &tag,
                "--pull",
                "never",
                "shell",
                "/code/proj__worktrees/feat",
            ])
        );
        assert_eq!(
            plan.setup[0].args.last().unwrap(),
            "/state/docker-sbx/wm-proj-12345678:ro"
        );
    }

    #[test]
    fn created_sandbox_links_the_agent_config_dir_into_the_agent_home_as_root() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.setup[1].args,
            args(&[
                "exec",
                "-u",
                "root",
                "wm-proj-12345678",
                "ln",
                "-sfnT",
                "/home/me/.claude",
                "/home/agent/.claude",
            ])
        );
    }

    #[test]
    fn existing_sandbox_is_neither_created_nor_set_up_again() {
        let p = paths();
        let state = SbxState {
            sandbox: Some(sandbox("stopped")),
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(plan.create_template, None);
        assert!(
            plan.setup
                .iter()
                .all(|c| c.args[0] != "create" && !c.args.contains(&"root".to_string())),
            "{:?}",
            plan.setup
        );
    }

    fn rule(scope: &str, resource: &str) -> SbxRule {
        SbxRule {
            scope: scope.to_string(),
            decision: "allow".to_string(),
            resource_type: "network".to_string(),
            resources: vec![resource.to_string()],
        }
    }

    fn with_network(policy: NetworkPolicy, domains: Vec<AllowedDomainEntry>) -> SandboxConfig {
        SandboxConfig {
            network: NetworkConfig {
                policy: Some(policy),
                allowed_domains: Some(domains),
                max_connections: None,
            },
            ..config()
        }
    }

    fn domains(hosts: &[&str]) -> Vec<AllowedDomainEntry> {
        hosts
            .iter()
            .map(|host| AllowedDomainEntry::Plain(host.to_string()))
            .collect()
    }

    fn policy_commands(plan: &LaunchPlan) -> Vec<Vec<String>> {
        plan.setup
            .iter()
            .filter(|c| c.args[0] == "policy")
            .map(|c| c.args.clone())
            .collect()
    }

    fn policy(verb: &str, resource: &str) -> Vec<String> {
        match verb {
            "rm" => args(&[
                "policy",
                "rm",
                "network",
                "--sandbox",
                "wm-proj-12345678",
                "--resource",
                resource,
                "-f",
            ]),
            _ => args(&[
                "policy",
                verb,
                "network",
                "--sandbox",
                "wm-proj-12345678",
                resource,
            ]),
        }
    }

    #[test]
    fn rpc_rule_allows_only_the_supervisors_port_until_it_exits() {
        // A bare `localhost` rule would reach every service on the
        // workstation's loopback, not just workmux's RPC server.
        let p = paths();
        for policy_kind in [NetworkPolicy::Allow, NetworkPolicy::Deny] {
            let config = with_network(policy_kind, Vec::new());
            let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

            assert!(
                policy_commands(&plan).contains(&policy("allow", "localhost:4242")),
                "{:?}",
                plan.setup
            );
            assert_eq!(
                plan.teardown
                    .iter()
                    .map(|c| c.args.clone())
                    .collect::<Vec<_>>(),
                vec![policy("rm", "localhost:4242")]
            );
            assert!(
                plan.network_rules.is_empty(),
                "the record is sandbox.network's"
            );
        }
    }

    #[test]
    fn rpc_rule_already_there_is_neither_added_nor_removed() {
        let p = paths();
        let existing = running_with(vec![
            rule("global", "api.anthropic.com"),
            rule(SCOPE, "localhost:4242"),
        ]);
        let plan = launch_plan(&config(), &target(&p), "claude", &existing, &exec()).unwrap();
        assert!(plan.setup.is_empty(), "{:?}", plan.setup);
        assert!(plan.teardown.is_empty(), "{:?}", plan.teardown);

        // A global rule, or one for another port, isn't this supervisor's.
        for rules in [
            vec![rule("global", "localhost:4242")],
            vec![rule(SCOPE, "localhost:4243")],
        ] {
            let plan = launch_plan(
                &config(),
                &target(&p),
                "claude",
                &running_with(rules),
                &exec(),
            )
            .unwrap();
            assert_eq!(
                policy_commands(&plan),
                vec![policy("allow", "localhost:4242")]
            );
        }
    }

    #[test]
    fn launch_without_an_rpc_server_adds_no_rpc_rule() {
        // `workmux add` creating the sandbox, and `sandbox shell`.
        let p = paths();
        let request = ExecRequest {
            rpc_port: None,
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &request).unwrap();
        assert!(policy_commands(&plan).is_empty(), "{:?}", plan.setup);
        assert!(plan.teardown.is_empty());
    }

    #[test]
    fn deny_allows_port_443_to_each_allowed_domain_in_the_sandbox_only() {
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&["api.anthropic.com", "github.com"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(
            commands.contains(&policy("allow", "api.anthropic.com:443")),
            "{commands:?}"
        );
        assert!(
            commands.contains(&policy("allow", "github.com:443")),
            "{commands:?}"
        );
    }

    #[test]
    fn allow_adds_no_rules_for_allowed_domains() {
        let p = paths();
        let config = with_network(NetworkPolicy::Allow, domains(&["github.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(
            !policy_commands(&plan)
                .iter()
                .any(|c| c.last().unwrap().ends_with(":443")),
            "{:?}",
            plan.setup
        );
        assert!(plan.network_rules.is_empty());
    }

    #[test]
    fn wildcard_domains_allow_subdomains_at_any_depth_but_not_the_base() {
        // sbx's `*.d` matches one label and `**.d` any depth plus d itself;
        // workmux's `*.d` matches any depth without d.
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&["*.googleapis.com", "*.github.com", "github.com"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let commands = policy_commands(&plan);
        for expected in [
            policy("allow", "**.googleapis.com:443"),
            policy("deny", "googleapis.com:443"),
            policy("allow", "**.github.com:443"),
            policy("allow", "github.com:443"),
        ] {
            assert!(commands.contains(&expected), "{expected:?} in {commands:?}");
        }
        assert!(
            !commands.contains(&policy("deny", "github.com:443")),
            "github.com is allowed: {commands:?}"
        );
    }

    #[test]
    fn nested_wildcards_dont_deny_a_base_an_outer_wildcard_allows() {
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&["*.example.com", "*.api.example.com"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(commands.contains(&policy("deny", "example.com:443")));
        assert!(
            !commands.contains(&policy("deny", "api.example.com:443")),
            "*.example.com allows api.example.com: {commands:?}"
        );
    }

    fn recorded(decision: RuleDecision, resource: &str) -> NetworkRule {
        NetworkRule {
            decision,
            resource: resource.to_string(),
        }
    }

    fn running_with(rules: Vec<SbxRule>) -> SbxState {
        SbxState {
            sandbox: Some(sandbox("running")),
            rules,
            ..missing()
        }
    }

    const SCOPE: &str = "sandbox:wm-proj-12345678";

    #[test]
    fn reconciling_removes_only_stale_rules_workmux_wrote() {
        let mut p = paths();
        p.record = vec![
            recorded(RuleDecision::Allow, "kept.com:443"),
            recorded(RuleDecision::Allow, "dropped.com:443"),
            recorded(RuleDecision::Allow, "gone-already.com:443"),
        ];
        let state = running_with(vec![
            rule("global", "dropped.com:443"),
            rule(SCOPE, "kept.com:443"),
            rule(SCOPE, "dropped.com:443"),
            rule(SCOPE, "users-own.com:443"),
        ]);
        let config = with_network(NetworkPolicy::Deny, domains(&["kept.com", "new.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(
            commands.contains(&policy("rm", "dropped.com:443")),
            "{commands:?}"
        );
        assert!(
            commands.contains(&policy("allow", "new.com:443")),
            "{commands:?}"
        );
        for untouched in ["kept.com:443", "users-own.com:443", "gone-already.com:443"] {
            assert!(
                !commands.iter().any(|c| c.iter().any(|a| a == untouched)),
                "{untouched} in {commands:?}"
            );
        }
        assert_eq!(
            plan.network_rules,
            vec![
                recorded(RuleDecision::Allow, "kept.com:443"),
                recorded(RuleDecision::Allow, "new.com:443"),
            ]
        );
    }

    #[test]
    fn a_rule_the_user_already_had_stays_theirs() {
        // Present before workmux would have added it: never recorded, so
        // never removed when config drops the domain.
        let p = paths();
        let state = running_with(vec![rule(SCOPE, "github.com:443")]);
        let config = with_network(NetworkPolicy::Deny, domains(&["github.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert!(
            !policy_commands(&plan)
                .iter()
                .any(|c| c.contains(&"github.com:443".to_string()))
        );
        assert!(plan.network_rules.is_empty(), "{:?}", plan.network_rules);
    }

    #[test]
    fn switching_to_allow_removes_every_rule_workmux_wrote() {
        let mut p = paths();
        p.record = vec![
            recorded(RuleDecision::Allow, "**.example.com:443"),
            recorded(RuleDecision::Deny, "example.com:443"),
        ];
        let mut deny = rule(SCOPE, "example.com:443");
        deny.decision = "deny".to_string();
        let state = running_with(vec![rule(SCOPE, "**.example.com:443"), deny]);
        let config = with_network(NetworkPolicy::Allow, domains(&["*.example.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(
            commands.contains(&policy("rm", "**.example.com:443")),
            "{commands:?}"
        );
        assert!(
            commands.contains(&policy("rm", "example.com:443")),
            "{commands:?}"
        );
        assert!(plan.network_rules.is_empty());
    }

    #[test]
    fn stale_rules_are_removed_before_rules_are_added() {
        // sbx refuses a rule while the same scope holds the opposite
        // decision for its resource.
        let mut p = paths();
        p.record = vec![
            recorded(RuleDecision::Allow, "**.example.com:443"),
            recorded(RuleDecision::Deny, "example.com:443"),
        ];
        let mut deny = rule(SCOPE, "example.com:443");
        deny.decision = "deny".to_string();
        let state = running_with(vec![rule(SCOPE, "**.example.com:443"), deny]);
        let config = with_network(NetworkPolicy::Deny, domains(&["example.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let commands = policy_commands(&plan);
        let removed = commands
            .iter()
            .position(|c| *c == policy("rm", "example.com:443"))
            .expect("deny removed");
        let added = commands
            .iter()
            .position(|c| *c == policy("allow", "example.com:443"))
            .expect("allow added");
        assert!(removed < added, "{commands:?}");
    }

    #[test]
    fn ip_literals_are_refused_as_on_the_container_backend() {
        let p = paths();
        for host in ["10.0.0.1", "::1"] {
            let config = with_network(NetworkPolicy::Deny, domains(&[host]));
            let err = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap_err();
            assert!(
                format!("{err:#}").contains("IP literals not allowed"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn allow_private_ips_needs_no_more_since_sbx_reaches_private_addresses_anyway() {
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            vec![AllowedDomainEntry::Detailed(AllowedDomainDetails {
                host: "git.internal.example".to_string(),
                allow_private_ips: true,
            })],
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.network_rules,
            vec![recorded(RuleDecision::Allow, "git.internal.example:443")]
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn max_connections_gets_a_note_since_sbx_has_no_cap() {
        let p = paths();
        let mut config = with_network(NetworkPolicy::Deny, domains(&["github.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        config.network.max_connections = Some(16);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert_eq!(plan.notes.len(), 1);
        assert!(
            plan.notes[0].contains("max_connections"),
            "{:?}",
            plan.notes
        );

        // Under allow it limits nothing on any backend.
        config.network.policy = Some(NetworkPolicy::Allow);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn deny_notes_what_the_global_sbx_policy_also_allows() {
        // workmux never writes the global policy, and a sandbox allow can't
        // take back a global one.
        let p = paths();
        let mut filesystem = rule("global", "**");
        filesystem.resource_type = "filesystem:read".to_string();
        let mut fallback = rule("global", "**");
        fallback.decision = "deny".to_string();
        let mut balanced = rule("global", "github.com:443");
        balanced.resources.push("**.npmjs.org:443".to_string());
        let state = SbxState {
            rules: vec![balanced, filesystem, fallback],
            ..missing()
        };
        let config = with_network(NetworkPolicy::Deny, domains(&["api.anthropic.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(plan.notes.len(), 1, "{:?}", plan.notes);
        assert!(plan.notes[0].contains("2 destinations"), "{:?}", plan.notes);
        assert!(
            plan.notes[0].contains("sbx policy ls wm-proj-12345678"),
            "{:?}",
            plan.notes
        );

        let allow = with_network(NetworkPolicy::Allow, Vec::new());
        let plan = launch_plan(&allow, &target(&p), "claude", &state, &exec()).unwrap();
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let deny_all = SbxState {
            rules: vec![rule("global", "**")]
                .into_iter()
                .map(|mut r| {
                    r.decision = "deny".to_string();
                    r
                })
                .collect(),
            ..missing()
        };
        let plan = launch_plan(&config, &target(&p), "claude", &deny_all, &exec()).unwrap();
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn exec_runs_the_command_in_the_pane_cwd_with_env_named_not_valued() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.exec.args,
            args(&[
                "exec",
                "-it",
                "-w",
                "/code/proj__worktrees/feat/sub",
                "-e",
                "WM_RPC_PORT",
                "-e",
                "WM_RPC_TOKEN",
                "-e",
                "WM_SBX_SESSION",
                "-e",
                "PATH=/state/docker-sbx/wm-proj-12345678/shims/bin:/home/agent/.local/bin:\
                 /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "wm-proj-12345678",
                "bash",
                "-c",
                "claude --dangerously-skip-permissions",
            ])
        );
        assert!(!plan.exec.args.iter().any(|a| a.contains("secret")));
        assert_eq!(
            plan.exec.env,
            vec![
                ("WM_RPC_PORT".to_string(), "4242".to_string()),
                ("WM_RPC_TOKEN".to_string(), "secret".to_string()),
                ("WM_SBX_SESSION".to_string(), "0123456789abcdef".to_string()),
            ]
        );
    }

    #[test]
    fn several_command_arguments_reach_the_guest_without_being_split_again() {
        // `workmux sandbox shell -- bash -c 'set -x; echo "a b"'`
        let p = paths();
        let words = ExecRequest {
            command: args(&["bash", "-c", "set -x; echo \"a b\""]),
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &words).unwrap();
        assert_eq!(
            plan.exec.args[plan.exec.args.len() - 3..],
            args(&["bash", "-c", "bash -c 'set -x; echo \"a b\"'"])
        );
    }

    #[test]
    fn exec_asks_for_a_tty_only_when_stdin_is_a_terminal() {
        // sbx's `exec -it` fails with piped stdin ("context deadline
        // exceeded"), as when `workmux sandbox shell -- <cmd>` runs from a
        // script.
        let p = paths();
        let piped = ExecRequest {
            tty: false,
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &piped).unwrap();
        assert_eq!(plan.exec.args[..3], args(&["exec", "-i", "-w"]));

        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert_eq!(plan.exec.args[..3], args(&["exec", "-it", "-w"]));
    }

    #[test]
    fn hangup_ends_only_the_processes_of_the_panes_session() {
        // sbx leaves the guest command running when its `sbx exec` client
        // hangs up, so the supervisor hangs up the session's processes itself.
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.hangup.args,
            args(&[
                "exec",
                "wm-proj-12345678",
                "sh",
                "-c",
                "for p in /proc/[0-9]*; do \
                 grep -qxzF WM_SBX_SESSION=0123456789abcdef \"$p/environ\" 2>/dev/null \
                 && kill -HUP \"${p#/proc/}\"; done; true",
            ])
        );
        assert!(plan.hangup.env.is_empty());
    }

    #[test]
    fn stopped_sandbox_needs_no_start_step_since_exec_starts_it() {
        let p = paths();
        let stopped = SbxState {
            sandbox: Some(sandbox("stopped")),
            rules: vec![rule("sandbox:wm-proj-12345678", "localhost:4242")],
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &stopped, &exec()).unwrap();

        assert!(plan.setup.is_empty(), "{:?}", plan.setup);
        assert_eq!(plan.exec.args[..2], args(&["exec", "-it"]));
    }

    fn failing(check: &str) -> SbxState {
        let mut state = missing();
        for c in &mut state.checks {
            if c.name == check {
                c.status = "fail".to_string();
            }
        }
        state
    }

    fn plan_error(state: &SbxState, agent: &str) -> String {
        let p = paths();
        launch_plan(&config(), &target(&p), agent, state, &exec())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn logged_out_sbx_fails_naming_sbx_login() {
        let message = plan_error(&failing("Authentication"), "claude");
        assert!(message.contains("sbx login"), "{message}");
    }

    #[test]
    fn uninitialised_global_policy_fails_naming_sbx_policy_init() {
        let message = plan_error(&failing("Global network policy"), "claude");
        assert!(message.contains("sbx policy init"), "{message}");
    }

    #[test]
    fn unsupported_hosts_fail_naming_supported_platforms() {
        for (os, arch, kvm) in [
            ("macos", "x86_64", false),
            ("linux", "x86_64", false),
            ("freebsd", "x86_64", true),
        ] {
            let state = SbxState {
                platform: Platform {
                    os: os.to_string(),
                    arch: arch.to_string(),
                    kvm,
                },
                ..missing()
            };
            let message = plan_error(&state, "claude");
            assert!(message.contains("Apple silicon"), "{os}/{arch}: {message}");
        }

        let linux_with_kvm = SbxState {
            platform: Platform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                kvm: true,
            },
            ..missing()
        };
        let p = paths();
        assert!(launch_plan(&config(), &target(&p), "claude", &linux_with_kvm, &exec()).is_ok());
    }

    fn setup_args(plan: &LaunchPlan) -> Vec<Vec<String>> {
        plan.setup.iter().map(|c| c.args.clone()).collect()
    }

    #[test]
    fn each_agents_config_dir_is_mounted_and_linked_to_its_usual_place() {
        for (agent, home_path) in [
            ("claude", "/home/agent/.claude"),
            ("copilot", "/home/agent/.copilot"),
            ("gemini", "/home/agent/.gemini"),
            ("agy", "/home/agent/.gemini/antigravity-cli"),
            ("codex", "/home/agent/.codex"),
            ("opencode", "/home/agent/.local/share/opencode"),
            ("pi", "/home/agent/.pi/agent"),
            ("omp", "/home/agent/.omp/agent"),
        ] {
            let p = paths();
            let plan = launch_plan(&config(), &target(&p), agent, &missing(), &exec()).unwrap();
            let host = format!("/home/me/.{agent}");

            assert!(
                plan.setup[0].args.contains(&host),
                "{agent}: {:?}",
                plan.setup[0]
            );
            let link = args(&[
                "exec",
                "-u",
                "root",
                "wm-proj-12345678",
                "ln",
                "-sfnT",
                &host,
                home_path,
            ]);
            assert!(
                setup_args(&plan).contains(&link),
                "{agent}: {:?}",
                plan.setup
            );
        }
    }

    #[test]
    fn created_claude_sandbox_is_seeded_with_completed_onboarding() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let seed = args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "sh",
            "-c",
            "[ -e \"$1\" ] || { printf %s \"$2\" > \"$1\" && chown agent:agent \"$1\"; }",
            "sh",
            "/home/agent/.claude.json",
            r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
        ]);
        assert!(setup_args(&plan).contains(&seed), "{:?}", plan.setup);

        let plan = launch_plan(&config(), &target(&p), "codex", &missing(), &exec()).unwrap();
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.iter().any(|w| w.contains(".claude.json"))),
            "{:?}",
            plan.setup
        );
    }

    #[test]
    fn opencode_global_config_dir_is_mounted_read_only_and_linked() {
        let p = paths();
        let opencode_config = PathBuf::from("/home/me/.config/opencode");
        let target = LaunchTarget {
            opencode_config_dir: Some(&opencode_config),
            ..target(&p)
        };
        let plan = launch_plan(&config(), &target, "opencode", &missing(), &exec()).unwrap();

        assert!(
            plan.setup[0]
                .args
                .contains(&"/home/me/.config/opencode:ro".to_string()),
            "{:?}",
            plan.setup[0]
        );
        let setup = setup_args(&plan);
        assert!(
            setup.contains(&args(&[
                "exec",
                "-u",
                "root",
                "wm-proj-12345678",
                "ln",
                "-sfnT",
                "/home/me/.config/opencode",
                "/home/agent/.config/opencode",
            ])),
            "{setup:?}"
        );

        // Only OpenCode gets it.
        let plan = launch_plan(&config(), &target, "codex", &missing(), &exec()).unwrap();
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.iter().any(|w| w.contains(".config/opencode"))),
            "{:?}",
            plan.setup
        );
    }

    fn bind_guest_dir(host: &str, guest: &str) -> Vec<String> {
        args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "sh",
            "-c",
            "mountpoint -q \"$1\" || { install -d -o agent -g agent \"$2\" \
             && mkdir -p \"$1\" && mount --bind \"$2\" \"$1\"; }",
            "sh",
            host,
            guest,
        ])
    }

    #[test]
    fn pi_and_omp_download_their_binaries_into_a_guest_local_dir_on_every_launch() {
        // They download Linux fd/rg into the mounted config dir, over the
        // workstation's own binaries. A guest bind mount covers that dir; it
        // doesn't survive sbx stopping the sandbox, so every launch makes it.
        let p = paths();
        let existing = SbxState {
            sandbox: Some(sandbox("stopped")),
            rules: vec![rule("sandbox:wm-proj-12345678", "localhost:4242")],
            ..missing()
        };
        for (agent, host, guest) in [
            ("pi", "/home/me/.pi/bin", "/var/lib/workmux/pi-agent-bin"),
            (
                "omp",
                "/home/me/.omp/tools",
                "/var/lib/workmux/omp-agent-tools",
            ),
        ] {
            for state in [missing(), existing.clone()] {
                let plan = launch_plan(&config(), &target(&p), agent, &state, &exec()).unwrap();
                assert!(
                    setup_args(&plan).contains(&bind_guest_dir(host, guest)),
                    "{agent}: {:?}",
                    plan.setup
                );
            }
        }

        let plan = launch_plan(&config(), &target(&p), "codex", &existing, &exec()).unwrap();
        assert!(plan.setup.is_empty(), "{:?}", plan.setup);
    }

    #[test]
    fn nested_links_get_parent_dirs_the_agent_owns_first() {
        // Root creates the link; parents it made would be root's, and the
        // agent couldn't write elsewhere under them (~/.local/state).
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "opencode", &missing(), &exec()).unwrap();
        let setup = setup_args(&plan);
        let parents = args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "install",
            "-d",
            "-o",
            "agent",
            "-g",
            "agent",
            "/home/agent/.local",
            "/home/agent/.local/share",
        ]);
        let parents_at = setup.iter().position(|a| *a == parents).expect("parents");
        let link_at = setup
            .iter()
            .position(|a| a.contains(&"/home/agent/.local/share/opencode".to_string()))
            .expect("link");
        assert!(parents_at < link_at, "{setup:?}");

        let plan = launch_plan(&config(), &target(&p), "codex", &missing(), &exec()).unwrap();
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.contains(&"install".to_string())),
            "{:?}",
            plan.setup
        );
    }

    #[test]
    fn agents_without_a_config_dir_get_no_mount_or_link() {
        let p = paths();
        let config = SandboxConfig {
            agent_config_dir: None,
            ..config()
        };
        let plan = launch_plan(&config, &target(&p), "default", &missing(), &exec()).unwrap();

        assert!(
            !workspaces(&plan).iter().any(|w| w.starts_with("/home/me")),
            "{:?}",
            plan.setup[0]
        );
        assert!(
            !plan
                .setup
                .iter()
                .any(|c| c.args.contains(&"ln".to_string())),
            "{:?}",
            plan.setup
        );
    }

    fn workspaces(plan: &LaunchPlan) -> Vec<String> {
        plan.setup[0]
            .args
            .iter()
            .skip_while(|a| *a != "shell")
            .skip(1)
            .cloned()
            .collect()
    }

    #[test]
    fn main_git_dir_is_read_only_but_for_what_commits_write() {
        // As on the container backend: hooks, config and the rest of the git
        // dir are read-only; objects, refs, logs and the worktree's own admin
        // dir stay writable, with the admin dir's pointers read-only again.
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            workspaces(&plan),
            args(&[
                "/code/proj__worktrees/feat",
                "/code/proj/.git:ro",
                "/code/proj/.git/objects",
                "/code/proj/.git/refs",
                "/code/proj/.git/logs",
                "/code/proj/.git/worktrees/feat",
                "/code/proj/.git/objects/info:ro",
                "/code/proj/.git/worktrees/feat/config.worktree:ro",
                "/code/proj/.git/worktrees/feat/gitdir:ro",
                "/code/proj/.git/worktrees/feat/commondir:ro",
                "/code/proj__worktrees/feat/.git:ro",
                "/home/me/.claude",
                "/state/docker-sbx/wm-proj-12345678:ro",
            ])
        );
    }

    #[test]
    fn submodule_git_dirs_are_writable_with_their_control_files_read_only() {
        let mut p = paths();
        p.git.module_roots = vec![PathBuf::from("/code/proj/.git/modules/vendor")];
        p.git
            .pointer_files
            .push(PathBuf::from("/code/proj__worktrees/feat/vendor/.git"));
        p.git.config_paths = vec![PathBuf::from("/code/proj__worktrees/feat/extra.gitconfig")];
        p.git.rr_cache = true;
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        let workspaces = workspaces(&plan);

        for expected in [
            "/code/proj/.git/rr-cache",
            "/code/proj/.git/modules/vendor",
            "/code/proj/.git/modules/vendor/hooks:ro",
            "/code/proj/.git/modules/vendor/info:ro",
            "/code/proj/.git/modules/vendor/objects/info:ro",
            "/code/proj/.git/modules/vendor/modules:ro",
            "/code/proj/.git/modules/vendor/worktrees:ro",
            "/code/proj/.git/modules/vendor/config:ro",
            "/code/proj/.git/modules/vendor/config.worktree:ro",
            "/code/proj__worktrees/feat/vendor/.git:ro",
            "/code/proj__worktrees/feat/extra.gitconfig:ro",
        ] {
            assert!(
                workspaces.contains(&expected.to_string()),
                "{expected}: {workspaces:?}"
            );
        }
    }

    fn config_error(config: &SandboxConfig) -> String {
        let p = paths();
        launch_plan(config, &target(&p), "claude", &missing(), &exec())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn excluded_files_is_an_error_since_sbx_cant_hide_a_file() {
        let mut config = config();
        config.container.excluded_files = Some(vec![".env".to_string()]);

        let message = config_error(&config);
        assert!(message.contains("excluded_files"), "{message}");
        assert!(message.contains("readable"), "{message}");
    }

    fn with_extra_mounts(mounts: Vec<ExtraMount>) -> SandboxConfig {
        SandboxConfig {
            extra_mounts: Some(mounts),
            ..config()
        }
    }

    #[test]
    fn extra_mounts_are_workspaces_read_only_unless_writable() {
        let config = with_extra_mounts(vec![
            ExtraMount::Path("/data/notes".to_string()),
            ExtraMount::Spec {
                host_path: "/data/scratch".to_string(),
                guest_path: Some("/data/scratch".to_string()),
                writable: Some(true),
            },
            ExtraMount::Spec {
                host_path: "/data/reference".to_string(),
                guest_path: None,
                writable: None,
            },
        ]);
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let workspaces = workspaces(&plan);
        assert_eq!(
            workspaces[workspaces.len() - 3..],
            args(&["/data/notes:ro", "/data/scratch", "/data/reference:ro"])
        );
    }

    #[test]
    fn extra_mount_at_a_different_guest_path_is_an_error() {
        let config = with_extra_mounts(vec![ExtraMount::Spec {
            host_path: "/data/notes".to_string(),
            guest_path: Some("/mnt/notes".to_string()),
            writable: None,
        }]);

        let message = config_error(&config);
        assert!(message.contains("/mnt/notes"), "{message}");
        assert!(message.contains("guest_path"), "{message}");
    }

    #[test]
    fn writable_extra_mount_of_a_file_is_an_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let host_path = file.path().to_string_lossy().into_owned();
        let config = with_extra_mounts(vec![ExtraMount::Spec {
            host_path: host_path.clone(),
            guest_path: None,
            writable: Some(true),
        }]);

        let message = config_error(&config);
        assert!(message.contains(&host_path), "{message}");
        assert!(message.contains("read-only"), "{message}");

        // Read-only, the file mounts.
        let config = with_extra_mounts(vec![ExtraMount::Path(host_path.clone())]);
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(workspaces(&plan).contains(&format!("{host_path}:ro")));
    }

    #[test]
    fn writable_extra_mount_cannot_overlap_git_metadata() {
        for host_path in [
            "/code/proj/.git/hooks",
            "/code",
            "/code/proj__worktrees/feat/.git",
        ] {
            let config = with_extra_mounts(vec![ExtraMount::Spec {
                host_path: host_path.to_string(),
                guest_path: None,
                writable: Some(true),
            }]);

            let message = config_error(&config);
            assert!(
                message.contains("protected Git metadata"),
                "{host_path}: {message}"
            );
        }

        // Read-only, it can.
        let config = with_extra_mounts(vec![ExtraMount::Path("/code".to_string())]);
        let p = paths();
        assert!(launch_plan(&config, &target(&p), "claude", &missing(), &exec()).is_ok());
    }
}
