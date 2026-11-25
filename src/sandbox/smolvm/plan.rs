//! The launch plan: the pure decision of what a sandboxed pane runs.
//!
//! Callers gather the host's state (paths, uid, the RPC endpoint), call
//! [`plan_launch`], run the invocations it returns, and report its notes.
//! Nothing here touches the filesystem or runs a process, so every choice
//! the backend makes is testable as a table of inputs and argument lists.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use super::git::GitLayout;
use super::machines::LABEL_PACK;
use super::network::{PrivateHosts, machine_network};
use super::pack::{GUEST_TOOLCHAIN_CACHE, NIX_PROFILE_BIN, PackInput, PackPlan, plan_pack};
use crate::config::SandboxConfig;
use crate::sandbox::agent_config_target;
use crate::sandbox::toolchain::{self, DetectedToolchain};

/// One `smolvm` invocation. Its `Debug` output names `env`'s variables but
/// never their values, which may be secrets (the RPC token).
#[derive(Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Arguments after `smolvm` (global options such as `--state-dir` are the
    /// runner's).
    pub args: Vec<String>,
    /// Variables set on the `smolvm` process. smolvm reads these by name
    /// (`-e NAME`), so their values never appear in `args`.
    pub env: Vec<(String, String)>,
}

impl std::fmt::Debug for Invocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&str> = self.env.iter().map(|(key, _)| key.as_str()).collect();
        f.debug_struct("Invocation")
            .field("args", &self.args)
            .field("env", &env)
            .finish()
    }
}

/// What a preparation step does, so the runner can apply its failure
/// policy. The project pack, when it must be built first, is built after
/// the `DeleteOrphan` steps (see [`LaunchPlan::pack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Delete a machine whose owner is gone. Best effort: a failure is
    /// logged and the launch goes on.
    DeleteOrphan,
    /// Create the pane's machine. The runner owns the machine (and runs
    /// [`LaunchPlan::delete`]) from this step on.
    CreateMachine,
    StartMachine,
    /// Mount a directory into the running machine (shims, `extra_mounts`):
    /// hot mounts don't count against the mounts smolvm boots with.
    MountDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    pub invocation: Invocation,
}

/// An ordered launch: `prepare`, then `exec` in the foreground, then
/// `delete`, whatever the exec's exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub machine: String,
    /// The project pack the machine is created from, and when to build it:
    /// before `CreateMachine` if missing.
    pub pack: PackPlan,
    pub prepare: Vec<Step>,
    pub exec: Invocation,
    pub delete: Invocation,
    /// Notes for `tracing`, never the terminal.
    pub notes: Vec<String>,
}

/// The workmux RPC endpoint the guest dials.
#[derive(Debug, Clone)]
pub struct RpcEndpoint {
    pub host: String,
    pub port: u16,
    pub token: String,
}

/// Everything a pane launch depends on, gathered by [`super::launch_input`].
#[derive(Debug, Clone)]
pub struct LaunchInput<'a> {
    pub sandbox: &'a SandboxConfig,
    /// Canonical agent name (`claude`, `codex`, ...).
    pub agent: &'a str,
    /// The worktree root, mounted read-write at its own path.
    pub worktree: &'a Path,
    /// The pane's working directory, inside `worktree`.
    pub pane_cwd: &'a Path,
    /// The pane's command, run by `sh -c` in the guest.
    pub command: &'a str,
    pub machine: String,
    /// The machine's labels (see [`super::machines::MachineIdentity::labels`]).
    pub labels: Vec<(String, String)>,
    /// workmux's machines whose owner is gone, deleted before creating.
    pub orphans: Vec<String>,
    /// The workstation's user, which the agent runs as.
    pub user: GuestUser,
    /// The agent's host config directory (`~/.claude`, ...), if it has one.
    pub agent_config_dir: Option<PathBuf>,
    /// Claude's sandbox config directory (`~/.claude-sandbox-config`), which
    /// holds the `claude.json` the guest's `~/.claude.json` links to.
    pub claude_config_dir: Option<PathBuf>,
    /// workmux's RPC server, if the guest may call it (not for `sandbox
    /// shell`).
    pub rpc: Option<RpcEndpoint>,
    /// Terminal variables (`TERM`, `COLORTERM`) to pass to the agent.
    pub terminal_env: Vec<(String, String)>,
    /// The toolchain of the pane's own worktree, which wraps its command.
    pub toolchain: DetectedToolchain,
    /// The project's pack index and the default branch's toolchain.
    pub pack: PackInput,
    /// The main repository's git metadata the worktree needs.
    pub git: GitLayout,
    /// The addresses of `allowed_domains` hosts with `allow_private_ips`,
    /// resolved for this pane ([`super::network::resolve_private_hosts`]).
    pub private_hosts: PrivateHosts,
    /// Start the machine with smolvm's seccomp and Landlock confinement of
    /// its VMM enforced (Linux hosts).
    pub confine_vmm: bool,
    /// The host's shim directory (`host_commands`, sound, clipboard),
    /// mounted read-only ahead of `PATH` once the machine runs.
    pub shims: Option<PathBuf>,
    /// `extra_mounts`, resolved and checked to be directories
    /// ([`super::extra_mounts`]).
    pub extra_mounts: Vec<Mount>,
    /// The host's git identity as `GIT_CONFIG_*` variables.
    pub git_identity: Vec<(String, String)>,
}

/// The uid and gid the agent runs as in the guest: the workstation user's,
/// so files it writes in the worktree belong to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestUser {
    pub uid: u32,
    pub gid: u32,
}

impl GuestUser {
    /// The user this process runs as.
    pub fn current() -> Self {
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self { uid, gid }
    }
}

impl std::fmt::Display for GuestUser {
    /// `uid:gid`, as `machine exec -u` and `chown` take it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// A directory shared into a machine: at boot (`machine create -v`), or
/// into the running machine (`machine mount add`), which doesn't count
/// against the mounts smolvm boots with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: PathBuf,
    pub read_only: bool,
}

/// The guest's `HOME`, as on the container backend: the sandbox images keep
/// their per-user setup (Claude's `~/.local/bin`) under it.
const GUEST_HOME: &str = "/tmp";
const GUEST_PATH: &str = "/tmp/.local/bin:/usr/local/bin:/usr/bin:/bin";
/// Claude's sandbox config directory in the guest; `~/.claude.json` links to
/// the `claude.json` in it (smolvm mounts directories only).
const GUEST_CLAUDE_CONFIG_DIR: &str = "/tmp/.claude-sandbox-config";
/// Where the shim directory is mounted, ahead of `PATH` (as on containers).
const GUEST_SHIMS_DIR: &str = "/tmp/.workmux-shims/bin";

/// Plan a pane's launch: create its machine from the project's pack, start
/// it, mount what comes after boot, exec the command, delete the machine.
pub fn plan_launch(input: &LaunchInput) -> Result<LaunchPlan> {
    check_pane_cwd(input)?;
    // As the container backend refuses it on a runtime without file
    // mounts: ignoring it would leave the files it masks readable.
    if !input.sandbox.container.excluded_files().is_empty() {
        bail!(
            "sandbox.container.excluded_files is set, but smolvm mounts directories \
             only and can't mask a file. Secrets would remain readable inside the \
             sandbox. Use docker or podman, or remove sandbox.container.excluded_files."
        );
    }
    let pack = plan_pack(
        input.sandbox,
        input.agent,
        &input.pack,
        input.user,
        input.confine_vmm,
    );
    // A pack doesn't carry virtio-net, so every machine asks for it.
    let network = machine_network(&input.sandbox.network, &input.private_hosts)?;
    let create = create_args(input, &pack, network.flags)?;
    let exec = exec_invocation(input);
    let hot_mounts = hot_mounts(input)?;
    let start = start_invocation(input);
    let prepare = prepare_steps(input, create, start, &hot_mounts);
    Ok(LaunchPlan {
        machine: input.machine.clone(),
        pack,
        prepare,
        exec,
        delete: delete_machine(&input.machine),
        notes: network.notes,
    })
}

fn check_pane_cwd(input: &LaunchInput) -> Result<()> {
    if !input.pane_cwd.starts_with(input.worktree) {
        bail!(
            "Working directory {} is not under worktree root {}",
            input.pane_cwd.display(),
            input.worktree.display()
        );
    }
    Ok(())
}

/// Whether the guest's `~/.claude.json` links to the mounted sandbox
/// config's `claude.json`.
fn links_claude_json(input: &LaunchInput) -> bool {
    input.agent == "claude" && input.claude_config_dir.is_some()
}

/// `machine create`: labels, the pack, the network, resources and the
/// boot mounts.
fn create_args(
    input: &LaunchInput,
    pack: &PackPlan,
    network_flags: Vec<String>,
) -> Result<Vec<String>> {
    let sandbox = input.sandbox;
    let mut create = words(&["machine", "create", "--name", &input.machine]);
    for (key, value) in &input.labels {
        create.extend(["--label".to_string(), format!("{key}={value}")]);
    }
    create.extend([
        "--label".to_string(),
        format!("{LABEL_PACK}={}", pack.build.name),
        "--from".to_string(),
        pack.build.sidecar.to_string_lossy().into_owned(),
    ]);
    create.extend(network_flags);
    if let Some(cpus) = sandbox.smolvm.cpus() {
        create.extend(["--cpus".to_string(), cpus.to_string()]);
    }
    if let Some(mib) = sandbox.smolvm.memory_mib() {
        create.extend(["--mem".to_string(), mib.to_string()]);
    }
    for mount in boot_mounts(input)? {
        create.extend(["-v".to_string(), mount.volume()]);
    }
    Ok(create)
}

/// The mounts the machine boots with, checked against smolvm's limits: the
/// worktree, the git metadata and the agent's config directories.
fn boot_mounts(input: &LaunchInput) -> Result<Vec<Mount>> {
    // At workstation paths, unless noted. The read-only `.git` comes before
    // the writable shares inside it.
    let mut mounts = vec![
        Mount::at(input.worktree, false),
        Mount::at(&input.git.common_dir, true),
    ];
    mounts.extend(input.git.writable.iter().map(|dir| Mount::at(dir, false)));
    if let (Some(dir), Some(target)) = (&input.agent_config_dir, agent_config_target(input.agent)) {
        mounts.push(Mount::new(dir, target, false));
    }
    if links_claude_json(input)
        && let Some(dir) = &input.claude_config_dir
    {
        mounts.push(Mount::new(dir, GUEST_CLAUDE_CONFIG_DIR, false));
    }
    check_mounts(&mounts)?;
    Ok(mounts)
}

/// The mounts added once the machine runs: shims and `extra_mounts`.
fn hot_mounts(input: &LaunchInput) -> Result<Vec<Mount>> {
    let mut hot_mounts = Vec::new();
    if let Some(shims) = &input.shims {
        hot_mounts.push(Mount::new(shims, GUEST_SHIMS_DIR, true));
    }
    let dot_git = input.worktree.join(".git");
    for mount in &input.extra_mounts {
        if !mount.read_only {
            let guest = &mount.guest;
            let protected = [input.git.common_dir.as_path(), dot_git.as_path()];
            if protected
                .iter()
                .any(|path| guest.starts_with(path) || path.starts_with(guest))
            {
                bail!(
                    "Writable extra mount at {} overlaps protected Git metadata",
                    guest.display()
                );
            }
        }
        hot_mounts.push(mount.clone());
    }
    for mount in &hot_mounts {
        check_mount_paths(mount)?;
    }
    Ok(hot_mounts)
}

/// The guest's `PATH`: the shims, then Nix's profile when a toolchain
/// wraps the command, ahead of the image's.
fn guest_path(input: &LaunchInput) -> String {
    let mut path = if input.toolchain == DetectedToolchain::None {
        GUEST_PATH.to_string()
    } else {
        format!("{NIX_PROFILE_BIN}:{GUEST_PATH}")
    };
    if input.shims.is_some() {
        path = format!("{GUEST_SHIMS_DIR}:{path}");
    }
    path
}

/// The pane's exec, with its secrets passed by name from smolvm's
/// environment.
fn exec_invocation(input: &LaunchInput) -> Invocation {
    let sandbox = input.sandbox;
    let mut guest_env = vec![
        format!("HOME={GUEST_HOME}"),
        format!("PATH={}", guest_path(input)),
    ];
    if input.toolchain != DetectedToolchain::None
        && input.pack.toolchain.kind != DetectedToolchain::None
    {
        // The pack's warm-up cached the default branch's inputs there. A
        // pack without a toolchain has no such directory, and the guest
        // user can't make it, so the caches stay under `HOME`.
        guest_env.push(format!("XDG_CACHE_HOME={GUEST_TOOLCHAIN_CACHE}"));
    }
    if input.agent == "codex"
        && let Some(target) = agent_config_target(input.agent)
    {
        // Codex refuses helper binaries under a temporary CODEX_HOME.
        guest_env.push(format!("CODEX_HOME={target}"));
    }
    if let Some(rpc) = &input.rpc {
        guest_env.extend([
            "WM_SANDBOX_GUEST=1".to_string(),
            format!("WM_RPC_HOST={}", rpc.host),
            format!("WM_RPC_PORT={}", rpc.port),
            // By name: smolvm takes the value from its own environment.
            "WM_RPC_TOKEN".to_string(),
        ]);
    }
    // `sandbox.env` by name, as the token: smolvm reads the values from its
    // own environment, so they stay out of argv and the logs.
    let mut sandbox_env = sandbox.env_vars();
    sandbox_env.sort();
    guest_env.extend(sandbox_env.iter().map(|(key, _)| key.to_string()));
    guest_env.extend(
        input
            .git_identity
            .iter()
            .chain(&input.terminal_env)
            .map(|(key, value)| format!("{key}={value}")),
    );
    // The pane's own worktree's shell; the pack holds a warm cache of the
    // default branch's.
    let command = toolchain::wrap_command(input.command, &input.toolchain);
    let command = if links_claude_json(input) {
        format!("ln -sf {GUEST_CLAUDE_CONFIG_DIR}/claude.json {GUEST_HOME}/.claude.json; {command}")
    } else {
        command
    };
    Invocation {
        args: exec_args(
            &input.machine,
            input.user,
            input.pane_cwd,
            guest_env,
            &command,
        ),
        env: input
            .rpc
            .iter()
            .map(|rpc| ("WM_RPC_TOKEN".to_string(), rpc.token.clone()))
            .chain(
                sandbox_env
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            )
            .collect(),
    }
}

/// `machine start`, with the VMM confinement in its environment.
fn start_invocation(input: &LaunchInput) -> Invocation {
    Invocation {
        args: words(&["machine", "start", "--name", &input.machine]),
        env: confinement_env(input.confine_vmm),
    }
}

/// `machine start`'s environment for smolvm's seccomp and Landlock
/// confinement of its VMM, when `confine_vmm` (Linux hosts; macOS has
/// neither).
pub(super) fn confinement_env(confine_vmm: bool) -> Vec<(String, String)> {
    if !confine_vmm {
        return Vec::new();
    }
    ["SMOLVM_SECCOMP", "SMOLVM_LANDLOCK"]
        .map(|var| (var.to_string(), "enforce".to_string()))
        .to_vec()
}

/// The steps before the exec: delete the orphans, create and start the
/// machine, then the hot mounts.
fn prepare_steps(
    input: &LaunchInput,
    create: Vec<String>,
    start: Invocation,
    hot_mounts: &[Mount],
) -> Vec<Step> {
    let machine = &input.machine;
    let mut prepare: Vec<Step> = input
        .orphans
        .iter()
        .map(|orphan| Step {
            kind: StepKind::DeleteOrphan,
            invocation: delete_machine(orphan),
        })
        .collect();
    prepare.extend([
        Step {
            kind: StepKind::CreateMachine,
            invocation: Invocation::new(create),
        },
        Step {
            kind: StepKind::StartMachine,
            invocation: start,
        },
    ]);
    prepare.extend(hot_mounts.iter().map(|mount| Step {
        kind: StepKind::MountDirectory,
        invocation: mount_add(machine, mount),
    }));
    prepare
}

/// smolvm boots a virtio-net machine with at most this many directory
/// mounts (measured on 1.36.1, with `--from` a pack or `--image`: 11 boot,
/// 12 exit before the agent is ready). Without virtio-net it boots 18.
const MAX_BOOT_MOUNTS: usize = 11;
/// Refuse mounts smolvm would fail to boot with, or misparse, naming the
/// limit rather than leaving smolvm's boot error to explain it.
fn check_mounts(mounts: &[Mount]) -> Result<()> {
    for mount in mounts {
        check_mount_paths(mount)?;
    }
    if mounts.len() > MAX_BOOT_MOUNTS {
        bail!(
            "This smol machine needs {} mounts, but smolvm boots with at most \
             {MAX_BOOT_MOUNTS} mounts. Submodules add mounts (shims and extra_mounts \
             are mounted after boot and don't count); remove some, or use another \
             sandbox.backend.",
            mounts.len()
        );
    }
    Ok(())
}

/// Refuse a path smolvm's `-v HOST:GUEST` would misparse.
fn check_mount_paths(mount: &Mount) -> Result<()> {
    for path in [&mount.host, &mount.guest] {
        let path = path.to_string_lossy();
        if path.contains(':') {
            bail!(
                "Cannot mount {path} into a smol machine: smolvm's -v HOST:GUEST \
                 cannot carry a path containing ':'"
            );
        }
    }
    Ok(())
}

/// `machine exec` as root of a script that gives `user` a passwd entry
/// (home `/tmp`, as the pane's `HOME`) unless the image has one. Nix
/// ignores a `HOME` its user doesn't own (`/tmp` is root's) and then needs
/// the entry ("cannot determine user's home directory").
pub(super) fn add_user(machine: &str, user: GuestUser) -> Vec<String> {
    let GuestUser { uid, gid } = user;
    let script = format!(
        "getent passwd {uid} >/dev/null || \
         echo 'workmux-{uid}:x:{uid}:{gid}:workmux:{GUEST_HOME}:/bin/sh' >> /etc/passwd"
    );
    let mut args = words(&["machine", "exec", "--name", machine, "-u", "0:0"]);
    args.extend(["--", "sh", "-c"].map(String::from));
    args.push(script);
    args
}

/// `machine exec -it` of `sh -c <command>` as `user` in `cwd`.
fn exec_args(
    machine: &str,
    user: GuestUser,
    cwd: &Path,
    guest_env: Vec<String>,
    command: &str,
) -> Vec<String> {
    let mut exec = words(&["machine", "exec", "--name", machine, "-it"]);
    exec.extend(["-u".to_string(), user.to_string()]);
    exec.extend(["-w".to_string(), cwd.to_string_lossy().into_owned()]);
    for var in guest_env {
        exec.extend(["-e".to_string(), var]);
    }
    exec.extend(["--", "sh", "-c"].map(String::from));
    exec.push(command.to_string());
    exec
}

/// `machine mount add` of `mount` into the running `machine`.
fn mount_add(machine: &str, mount: &Mount) -> Invocation {
    let mut args = words(&["machine", "mount", "add", "--name", machine, "-v"]);
    args.push(mount.volume());
    Invocation::new(args)
}

impl Mount {
    pub(super) fn new(host: &Path, guest: impl Into<PathBuf>, read_only: bool) -> Self {
        Self {
            host: host.to_path_buf(),
            guest: guest.into(),
            read_only,
        }
    }

    /// A share at its workstation path.
    fn at(path: &Path, read_only: bool) -> Self {
        Self::new(path, path, read_only)
    }

    /// smolvm's `-v HOST:GUEST[:ro]`.
    pub(super) fn volume(&self) -> String {
        let suffix = if self.read_only { ":ro" } else { "" };
        format!("{}:{}{suffix}", self.host.display(), self.guest.display())
    }
}

/// `machine delete -f`, for a pane's machine or any other workmux machine.
pub fn delete_machine(name: &str) -> Invocation {
    Invocation::new(words(&["machine", "delete", "--name", name, "-f"]))
}

impl Invocation {
    pub fn new(args: Vec<String>) -> Self {
        Self {
            args,
            env: Vec::new(),
        }
    }
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

#[cfg(test)]
pub(crate) mod tests;
