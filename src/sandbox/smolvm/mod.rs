//! smolvm backend: each sandboxed pane runs in its own smol machine.
//!
//! Laid out like the Lima backend; its lifecycle is the container backend's
//! (create when the pane starts, delete when it exits). [`plan::plan_launch`]
//! decides a launch, and [`runner::Runner`] carries it out.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::multiplexer::agent::SelectedAgent;
use crate::multiplexer::util::ResolvedCommand;

pub(crate) mod git;
pub(crate) mod machines;
pub(crate) mod network;
pub(crate) mod pack;
pub(crate) mod plan;
pub(crate) mod prerequisites;
pub(crate) mod runner;

/// Launch a pane's (or a shell's) machine: delete workmux's machines whose
/// owner is gone, then create, start, exec and delete per [`plan::plan_launch`].
/// Returns the exec's exit code.
pub fn launch(runner: &runner::Runner, mut input: plan::LaunchInput) -> Result<i32> {
    input.orphans = match runner.list() {
        Ok(listed) => machines::orphans(&listed, machines::Owner::is_alive)
            .into_iter()
            .map(|machine| machine.name.clone())
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "could not list machines; skipping the orphan sweep");
            Vec::new()
        }
    };
    let pack_dir = input.pack.dir.clone();
    let plan = plan::plan_launch(&input)?;
    let result = runner.run(&plan);
    // This machine is gone: a pack it kept from being swept may go now.
    sweep_superseded_packs(runner, &pack_dir);
    result
}

/// A pane's way back to the host: workmux's RPC endpoint and the
/// host-exec shims that call it.
pub struct HostExec {
    pub rpc: plan::RpcEndpoint,
    pub shims: PathBuf,
}

/// Gather a pane's (or a shell's) launch from the host: the agent's config
/// directories, the machine's name and labels, the project pack, git layout
/// and mounts. `label_agent` names the machine (`shell` for `sandbox
/// shell`); `agent` picks its image and mounts. `host_exec` is unset for a
/// shell.
pub fn launch_input<'a>(
    config: &'a crate::config::Config,
    agent: &'static str,
    label_agent: &str,
    worktree_root: &'a Path,
    pane_cwd: &'a Path,
    command: &'a str,
    host_exec: Option<HostExec>,
) -> Result<plan::LaunchInput<'a>> {
    let (rpc, shims) = host_exec.map(|h| (h.rpc, h.shims)).unzip();
    let sandbox = &config.sandbox;
    let claude_config_dir = if agent == "claude" {
        Some(crate::sandbox::ensure_sandbox_config_dirs()?.config_dir)
    } else {
        None
    };
    let agent_config_dir = sandbox
        .resolved_agent_config_dir(agent)
        .filter(|_| crate::sandbox::agent_config_target(agent).is_some());
    if let Some(dir) = &agent_config_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }

    let project_root = project_root(worktree_root);
    let identity = machines::MachineIdentity::new(&project_root, worktree_root, label_agent);
    let git = git::gather(worktree_root)?;
    git::git_isolation_acknowledgement(sandbox, &crate::xdg::state_dir()?, &identity.project_key())
        .acknowledge()?;
    let owner = machines::Owner::current()?;
    let pack = pack::gather(
        sandbox,
        &project_root,
        config.main_branch.as_deref(),
        &owner,
    )?;

    Ok(plan::LaunchInput {
        sandbox,
        agent,
        worktree: worktree_root,
        pane_cwd,
        command,
        machine: identity.name(&machines::random_suffix()),
        labels: identity.labels(&owner),
        orphans: Vec::new(),
        user: plan::GuestUser::current(),
        agent_config_dir,
        claude_config_dir,
        rpc,
        terminal_env: terminal_env(),
        toolchain: crate::sandbox::toolchain::resolve_toolchain(
            &sandbox.toolchain(),
            worktree_root,
        ),
        pack,
        git,
        confine_vmm: CONFINE_VMM,
        shims,
        extra_mounts: extra_mounts(sandbox)?,
        git_identity: crate::command::sandbox_run::git_user_config_envs(worktree_root),
    })
}

/// `workmux add`, before the window opens: build the project's missing or
/// stale packs for `agents` (canonical names), under their locks, with
/// progress on the terminal, so a slow first build and its errors show up
/// before an empty pane.
pub fn prepare_packs(
    config: &crate::config::Config,
    worktree: &Path,
    agents: &[&str],
) -> Result<()> {
    prerequisites::ensure_available()?;
    let project_root = project_root(worktree);
    // Ask before the window opens, in the user's terminal, not in a pane.
    let identity = machines::MachineIdentity::new(&project_root, worktree, machines::PACK_AGENT);
    git::gather(worktree)?;
    // A file in extra_mounts fails here, not in an empty pane.
    extra_mounts(&config.sandbox)?;
    git::git_isolation_acknowledgement(
        &config.sandbox,
        &crate::xdg::state_dir()?,
        &identity.project_key(),
    )
    .acknowledge()?;
    build_missing_packs(config, &project_root, agents)
}

/// The main worktree of `worktree`'s repository, or `worktree` itself.
pub fn project_root(worktree: &Path) -> PathBuf {
    crate::sandbox::lima::determine_project_root(worktree)
        .unwrap_or_else(|_| worktree.to_path_buf())
}

/// Build the project's missing packs for `agents` under their locks, then
/// sweep its superseded ones.
fn build_missing_packs(
    config: &crate::config::Config,
    project_root: &Path,
    agents: &[&str],
) -> Result<()> {
    let runner = runner::Runner::from_env();
    let input = pack::gather(
        &config.sandbox,
        project_root,
        config.main_branch.as_deref(),
        &machines::Owner::current()?,
    )?;
    let user = plan::GuestUser::current();
    for agent in agents {
        let plan = pack::plan_pack(&config.sandbox, agent, &input, user, CONFINE_VMM);
        if plan.when == pack::PackWhen::BuildFirst {
            runner.build_pack(&plan.build)?;
        }
    }
    sweep_superseded_packs(&runner, &input.dir);
    Ok(())
}

/// Whether machines start with smolvm's VMM confinement: on Linux hosts.
pub const CONFINE_VMM: bool = cfg!(target_os = "linux");

/// The terminal variables (`TERM`, `COLORTERM`) set here, for the guest.
pub fn terminal_env() -> Vec<(String, String)> {
    ["TERM", "COLORTERM"]
        .into_iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

/// Delete the project's superseded packs that no machine was created from
/// (or that no machine still left was). Best effort: failures are logged.
pub fn sweep_superseded_packs(runner: &runner::Runner, dir: &Path) {
    let superseded = pack::superseded_packs(dir);
    if superseded.is_empty() {
        return;
    }
    let listed = match runner.list() {
        Ok(listed) => listed,
        Err(error) => {
            tracing::warn!(%error, "could not list machines; keeping superseded packs");
            return;
        }
    };
    for name in superseded {
        let in_use = listed
            .iter()
            .any(|machine| machine.label(machines::LABEL_PACK) == Some(name.as_str()));
        if in_use {
            continue;
        }
        match pack::remove_pack(dir, &name) {
            Ok(()) => tracing::info!(pack = %name, "deleted superseded pack"),
            Err(error) => tracing::warn!(pack = %name, %error, "failed to delete pack"),
        }
    }
}

/// Delete every workmux machine labelled with `worktree`, running or not,
/// best effort: failures are logged, never returned.
pub fn delete_worktree_machines(runner: &runner::Runner, worktree: &Path) {
    let listed = match runner.list() {
        Ok(listed) => listed,
        Err(error) => {
            tracing::warn!(%error, "could not list smol machines to delete");
            return;
        }
    };
    let hash = machines::path_hash(worktree);
    for machine in listed.iter().filter(|m| m.serves_worktree(&hash)) {
        match runner.delete(&machine.name) {
            Ok(()) => tracing::info!(machine = %machine.name, "deleted the worktree's machine"),
            Err(error) => {
                tracing::warn!(machine = %machine.name, %error, "failed to delete machine")
            }
        }
    }
}

/// On removing a worktree (`remove`, `merge`, `add`'s rollback): delete
/// its machines when the smolvm backend is selected and smolvm is on
/// `PATH`. Best effort.
pub fn cleanup_worktree(config: &crate::config::Config, worktree: &Path) {
    if !config.sandbox.uses_smolvm() || which::which("smolvm").is_err() {
        return;
    }
    delete_worktree_machines(&runner::Runner::from_env(), worktree);
}

/// The guest's name for workmux's RPC host: smolvm's virtio-net gateway,
/// relayed to host loopback.
pub const RPC_HOST: &str = "host.smolvm.internal";

/// Wrap a sandboxed pane's command to run in a smol machine via the
/// sandbox supervisor. Checks smolvm's prerequisites first, so a missing
/// or old smolvm fails before the window opens.
///
/// The pane names its agent ([`pane_agent`]) to the supervisor, which
/// picks the machine's image and config directory with it
/// ([`machine_agent`]).
pub fn wrap_for_smolvm(
    command: &str,
    resolved: &ResolvedCommand,
    config: &crate::config::Config,
    task_agent: Option<&str>,
    supervisor: Option<&str>,
    worktree_root: &Path,
    pane_cwd: &Path,
) -> Result<String> {
    let agent = pane_agent(resolved, config, task_agent).map(|selected| selected.kind());
    prerequisites::ensure_available()?;
    supervisor_command(command, supervisor, worktree_root, pane_cwd, agent)
}

/// The agent a sandboxed pane's machine runs: the pane's own, or for a
/// plain pane (sandboxed under `target: all`) the selected agent (`-a`,
/// else the configured one).
pub fn pane_agent(
    resolved: &ResolvedCommand,
    config: &crate::config::Config,
    task_agent: Option<&str>,
) -> Option<SelectedAgent> {
    resolved
        .selected_agent
        .clone()
        .or_else(|| crate::multiplexer::agent::resolve_selected_agent(config, task_agent))
}

/// The agent a pane's machine is for, which picks its image, pack and
/// config directory: the agent the pane names (`--agent`), else the
/// configured one.
pub fn machine_agent(config: &crate::config::Config, pane_agent: Option<&str>) -> &'static str {
    use crate::multiplexer::agent::{resolve_profile, resolve_profile_with_type};
    match pane_agent {
        Some(name) => resolve_profile(Some(name)),
        None => resolve_profile_with_type(config.agent.as_deref(), config.agent_type.as_deref()),
    }
    .name()
}

/// The supervisor command line for a pane, as on the container backend plus
/// the pane's agent.
fn supervisor_command(
    command: &str,
    supervisor: Option<&str>,
    worktree_root: &Path,
    pane_cwd: &Path,
    agent: Option<&str>,
) -> Result<String> {
    let options: Vec<(&str, &str)> = agent.map(|agent| ("--agent", agent)).into_iter().collect();
    crate::sandbox::container::supervisor_line(
        command,
        supervisor,
        &options,
        worktree_root,
        pane_cwd,
    )
}

/// `sandbox.extra_mounts`, resolved, for mounting into a running machine.
/// smolvm shares directories only (virtiofs), so a file is an error, as is a
/// path that doesn't exist.
pub fn extra_mounts(sandbox: &crate::config::SandboxConfig) -> Result<Vec<plan::Mount>> {
    sandbox
        .extra_mounts()
        .iter()
        .map(|mount| {
            let (host, guest, read_only) = mount.resolve()?;
            match std::fs::metadata(&host) {
                Ok(meta) if meta.is_dir() => Ok(plan::Mount {
                    host,
                    guest,
                    read_only,
                }),
                Ok(_) => bail!(
                    "extra_mounts: {} is not a directory; the smolvm backend mounts \
                     directories only. Mount the directory that holds it instead.",
                    host.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    bail!("extra_mounts: {} does not exist", host.display())
                }
                Err(error) => {
                    Err(error).with_context(|| format!("extra_mounts: {}", host.display()))
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox_with_extra_mounts(yaml: &str) -> crate::config::SandboxConfig {
        serde_yaml::from_str(&format!("extra_mounts:\n{yaml}")).unwrap()
    }

    #[test]
    fn test_extra_mounts_are_directories_read_only_unless_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let notes = tmp.path().join("notes");
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let sandbox = sandbox_with_extra_mounts(&format!(
            "  - {}\n  - host_path: {}\n    guest_path: /data/out\n    writable: true\n",
            notes.display(),
            out.display()
        ));
        assert_eq!(
            extra_mounts(&sandbox).unwrap(),
            [
                plan::Mount {
                    host: notes.clone(),
                    guest: notes.clone(),
                    read_only: true,
                },
                plan::Mount {
                    host: out.clone(),
                    guest: "/data/out".into(),
                    read_only: false,
                },
            ]
        );
    }

    #[test]
    fn test_an_extra_mount_of_a_file_or_a_missing_path_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("token.txt");
        std::fs::write(&file, "x").unwrap();
        let sandbox = sandbox_with_extra_mounts(&format!("  - {}\n", file.display()));
        let error = extra_mounts(&sandbox).unwrap_err().to_string();
        assert!(error.contains("token.txt"), "{error}");
        assert!(error.contains("directories only"), "{error}");

        let missing = tmp.path().join("gone");
        let sandbox = sandbox_with_extra_mounts(&format!("  - {}\n", missing.display()));
        let error = extra_mounts(&sandbox).unwrap_err().to_string();
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn test_a_plain_pane_gets_the_selected_agents_machine() {
        let config: crate::config::Config = serde_yaml::from_str(
            "agent: cc-work\n\
             agents:\n  cc-work:\n    type: claude\n    env:\n      \
             LITERAL: plain\n\
             sandbox:\n  target: all\n",
        )
        .unwrap();
        let resolved = crate::multiplexer::util::resolve_pane_command_with_config(
            Some("npm test"),
            true,
            None,
            Path::new("/wt"),
            &config,
            None,
            "bash",
        )
        .unwrap();
        assert!(resolved.selected_agent.is_none());
        let agent = pane_agent(&resolved, &config, None).unwrap();
        assert_eq!(agent.kind(), "claude");
        assert_eq!(resolved.command, "npm test");
    }

    #[test]
    fn test_an_agent_pane_names_its_agent_to_the_supervisor() {
        let line = supervisor_command(
            "codex --yolo",
            None,
            Path::new("/wt"),
            Path::new("/wt"),
            Some("codex"),
        )
        .unwrap();
        assert_eq!(
            line,
            " workmux sandbox run --agent 'codex' '/wt' -- 'codex --yolo'"
        );
    }

    #[test]
    fn test_a_line_naming_no_agent_leaves_it_to_the_config() {
        let line = supervisor_command(
            "npm test",
            Some("/opt/wm sandbox run"),
            Path::new("/wt"),
            Path::new("/wt/web"),
            None,
        )
        .unwrap();
        assert_eq!(
            line,
            " /opt/wm sandbox run '/wt/web' --worktree-root '/wt' -- 'npm test'"
        );
    }
}
