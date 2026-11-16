//! Docker Sandboxes (`sbx`) backend for sandbox.
//!
//! Runs each agent of each worktree in its own sbx microVM sandbox. The
//! worktree, the main repository's git dir (read-only except for what commits
//! write) and the agent's config directory are mounted at their workstation
//! paths, and commands run through `sbx exec`.

mod acknowledgement;
mod git;
mod hooks;
mod instance;
mod mask;
mod plan;
mod template;

pub(crate) use instance::sbx_state_dir_path;
pub use instance::{
    SbxInstance, SbxSandboxInfo, ensure_sandbox, new_session, prepare_launch,
    prepare_sandbox_removal, remove_sandbox, run_exec,
};
pub use plan::{ExecRequest, launch_plan};
pub use template::{dockerfile_layer, ensure_template, template_loaded_for};

/// Prefix for all workmux-managed sbx sandbox names.
pub const SANDBOX_PREFIX: &str = "wm-";

use anyhow::{Context, Result};

use crate::config::Config;
use std::path::Path;
use tracing::debug;

/// Longest sandbox name sbx accepts.
const MAX_NAME_LEN: usize = 63;

/// Wrap a pane's command to run in the sbx sandbox of `agent`, the pane agent.
///
/// `supervisor` replaces the default `workmux sandbox run` words, already
/// shell-quoted, for callers that must pin a specific binary.
pub fn wrap_for_docker_sbx(
    command: &str,
    supervisor: Option<&str>,
    agent: &str,
    worktree_root: &Path,
    pane_cwd: &Path,
) -> Result<String> {
    super::container::wrap_for_supervisor(command, supervisor, worktree_root, pane_cwd, Some(agent))
}

/// Reject `--fork` for sbx sandboxes under `agent: kit`.
///
/// The fork reads the workstation's agent config, which a kit sandbox doesn't
/// mount. `sandboxed` is whether `--sandbox` turns the sandbox on.
pub fn check_fork(config: &crate::config::SandboxConfig, sandboxed: bool) -> Result<()> {
    use crate::config::{DockerSbxAgent, SandboxBackend};
    if (config.is_enabled() || sandboxed)
        && config.backend() == SandboxBackend::DockerSbx
        && config.docker_sbx.agent() == DockerSbxAgent::Kit
    {
        anyhow::bail!(
            "--fork forks a conversation from the workstation's agent config, which sbx \
             sandboxes under sandbox.docker_sbx.agent: kit don't have. Set \
             `sandbox.docker_sbx.agent: shell` in your global config to mount it."
        );
    }
    Ok(())
}

/// Resolve the worktree's selected agent from `selector` (`-a`) or the config.
pub fn selected_agent(config: &Config, selector: Option<&str>) -> String {
    crate::multiplexer::agent::resolve_selected_agent(config, selector)
        .map(|agent| agent.name().to_string())
        .unwrap_or_else(|| default_agent_type(config).to_string())
}

/// Resolve the pane agent, whose sandbox a pane runs in.
///
/// An agent is a named agent, or an agent type by its own name. A plain pane
/// uses the worktree's selected agent.
pub fn pane_agent(
    config: &Config,
    pane_agent: Option<&crate::multiplexer::agent::SelectedAgent>,
    selector: Option<&str>,
) -> String {
    match pane_agent {
        Some(agent) => agent.name().to_string(),
        None => selected_agent(config, selector),
    }
}

/// Resolve the agent type of `agent`.
fn agent_type(config: &Config, agent: &str) -> &'static str {
    crate::multiplexer::agent::resolve_selected_agent(config, Some(agent))
        .map(|agent| agent.kind())
        .unwrap_or_else(|| default_agent_type(config))
}

fn default_agent_type(config: &Config) -> &'static str {
    crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name()
}

/// Longest agent part of a sandbox name, leaving the project name room.
const MAX_AGENT_LEN: usize = 24;

/// Length of the hash added to an agent name that had to be changed to fit.
const AGENT_HASH_LEN: usize = 6;

/// Generate the sandbox name `wm-<project>-<hash8>-<agent>` for a worktree.
///
/// The hash is of the worktree path; the project name is only there to make
/// `sbx ls` readable. Names stay within sbx's rules: lowercase letters, digits
/// and `-`, at most 63 characters.
pub fn sandbox_name(worktree_root: &Path, agent: &str) -> Result<String> {
    let identity = crate::git::RepositoryIdentity::discover(worktree_root)
        .context("Failed to find the worktree's repository")?;
    let project_root = identity
        .common_dir
        .parent()
        .unwrap_or(&identity.common_dir)
        .to_path_buf();
    Ok(sandbox_name_in(&project_root, worktree_root, agent))
}

/// Generate the sandbox name without reading the worktree's git metadata.
///
/// Removal needs this, since the metadata may already be gone.
pub fn sandbox_name_in(project_root: &Path, worktree_root: &Path, agent: &str) -> String {
    let name = sandbox_name_for(project_root, &canonical(worktree_root), agent);
    debug!(sandbox = %name, agent, "resolved sbx sandbox name");
    name
}

fn canonical(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn sandbox_name_for(project_root: &Path, worktree_root: &Path, agent: &str) -> String {
    let agent = agent_slug(agent);
    // Budget: "-" (1) + agent
    let base = worktree_name_for(project_root, worktree_root, MAX_NAME_LEN - 1 - agent.len());
    format!("{base}-{agent}")
}

/// Generate `wm-<project>-<hash8>`, at most `max_len` characters.
fn worktree_name_for(project_root: &Path, worktree_root: &Path, max_len: usize) -> String {
    use crate::sandbox::{hash_key, sanitize_name};

    let hash = hash_key(&worktree_root.to_string_lossy(), 8);
    let project_dir_name = project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Budget: "wm-" (3) + project + "-" (1) + hash (8)
    let budget = max_len - SANDBOX_PREFIX.len() - 1 - hash.len();
    let sanitized = sanitize_name(&project_dir_name, budget);

    if sanitized.is_empty() {
        format!("{}{}", SANDBOX_PREFIX, hash)
    } else {
        format!("{}{}-{}", SANDBOX_PREFIX, sanitized, hash)
    }
}

/// Build the agent part of a sandbox name.
///
/// A name sbx rejects is slugified and gets a hash, so agents whose names
/// slugify alike keep separate sandboxes.
fn agent_slug(agent: &str) -> String {
    use crate::sandbox::{hash_key, sanitize_name};

    let slug = sanitize_name(agent, MAX_AGENT_LEN);
    if !slug.is_empty() && slug == agent {
        return slug;
    }
    let hash = hash_key(agent, AGENT_HASH_LEN);
    let head = sanitize_name(agent, MAX_AGENT_LEN - 1 - AGENT_HASH_LEN);
    if head.is_empty() {
        hash
    } else {
        format!("{head}-{hash}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_kit_fork_is_an_error_naming_agent_shell() {
        use crate::config::{DockerSbxAgent, DockerSbxConfig, SandboxBackend, SandboxConfig};
        let sbx = |agent: DockerSbxAgent| SandboxConfig {
            enabled: Some(true),
            backend: Some(SandboxBackend::DockerSbx),
            docker_sbx: DockerSbxConfig {
                agent: Some(agent),
                ..Default::default()
            },
            ..Default::default()
        };

        let err = check_fork(&sbx(DockerSbxAgent::Kit), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--fork")
                && err.contains("workstation")
                && err.contains("sandbox.docker_sbx.agent: shell"),
            "{err}"
        );
        // `--sandbox` turns the sandbox on for the one launch.
        let off = SandboxConfig {
            enabled: Some(false),
            ..sbx(DockerSbxAgent::Kit)
        };
        assert!(check_fork(&off, true).is_err());

        assert!(check_fork(&sbx(DockerSbxAgent::Shell), false).is_ok());
        assert!(check_fork(&off, false).is_ok());
        let container = SandboxConfig {
            backend: Some(SandboxBackend::Container),
            ..sbx(DockerSbxAgent::Kit)
        };
        assert!(check_fork(&container, false).is_ok());
    }

    fn within_sbx_rules(name: &str) -> bool {
        name.len() <= MAX_NAME_LEN
            && name.len() >= 2
            && name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    }

    #[test]
    fn sandbox_name_names_the_project_and_hashes_the_worktree() {
        let name = sandbox_name_for(
            Path::new("/code/My_Project"),
            Path::new("/code/My_Project__worktrees/feature"),
            "claude",
        );
        assert!(name.starts_with("wm-my-project-"), "{name}");
        assert!(name.ends_with("-claude"), "{name}");
        assert_eq!(name.len(), "wm-my-project-".len() + 8 + "-claude".len());
        assert!(within_sbx_rules(&name), "{name}");
    }

    #[test]
    fn each_worktree_gets_its_own_sandbox() {
        let project = Path::new("/code/proj");
        assert_ne!(
            sandbox_name_for(project, Path::new("/code/proj__worktrees/a"), "claude"),
            sandbox_name_for(project, Path::new("/code/proj__worktrees/b"), "claude")
        );
        assert_eq!(
            sandbox_name_for(project, Path::new("/code/proj__worktrees/a"), "claude"),
            sandbox_name_for(project, Path::new("/code/proj__worktrees/a"), "claude")
        );
    }

    #[test]
    fn worktree_names_its_sandbox_without_git_as_git_did() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("My_Project");
        std::fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&main)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let worktree = temp.path().join("My_Project__worktrees/feat");
        git(&["worktree", "add", "-qb", "feat", worktree.to_str().unwrap()]);

        // Names given before stay: the project root git reports.
        let project_root = crate::sandbox::lima::determine_project_root(&worktree).unwrap();
        let before = sandbox_name_in(&project_root, &worktree, "codex");
        assert_eq!(sandbox_name(&worktree, "codex").unwrap(), before);
        assert!(before.starts_with("wm-my-project-"), "{before}");
        // Removal names it from the project root it already knows.
        assert_eq!(sandbox_name_in(&main, &worktree, "codex"), before);
    }

    #[test]
    fn each_agent_of_a_worktree_gets_its_own_sandbox() {
        let project = Path::new("/code/proj");
        let worktree = Path::new("/code/proj__worktrees/a");
        let base = worktree_name_for(project, worktree, MAX_NAME_LEN);
        assert_eq!(
            sandbox_name_for(project, worktree, "claude"),
            format!("{base}-claude")
        );
        assert_eq!(
            sandbox_name_for(project, worktree, "cc-personal"),
            format!("{base}-cc-personal")
        );
        assert_ne!(
            sandbox_name_for(project, worktree, "claude"),
            sandbox_name_for(project, worktree, "codex")
        );
    }

    #[test]
    fn agent_names_sbx_would_reject_are_slugified_with_a_hash() {
        let project = Path::new("/code/proj");
        let worktree = Path::new("/code/proj__worktrees/a");
        let base = worktree_name_for(project, worktree, MAX_NAME_LEN);
        let slugged = sandbox_name_for(project, worktree, "CC_Personal");
        assert!(
            slugged.starts_with(&format!("{base}-cc-personal-")),
            "{slugged}"
        );
        assert!(within_sbx_rules(&slugged), "{slugged}");
        // Names that slugify alike still get their own sandboxes.
        assert_ne!(slugged, sandbox_name_for(project, worktree, "cc-personal"));
        assert_ne!(slugged, sandbox_name_for(project, worktree, "cc personal"));
    }

    #[test]
    fn long_agent_names_are_shortened_with_a_hash_to_fit() {
        let long = format!("/code/{}", "p".repeat(80));
        let project = Path::new(&long);
        let worktree = Path::new("/code/wt");
        let a = sandbox_name_for(project, worktree, &"agent".repeat(20));
        let b = sandbox_name_for(project, worktree, &format!("{}x", "agent".repeat(20)));
        assert!(within_sbx_rules(&a), "{a}");
        assert!(within_sbx_rules(&b), "{b}");
        assert_ne!(a, b);
        assert!(a.starts_with("wm-ppp"), "{a}");
        assert!(a.contains("-agentagent"), "{a}");
        for agent in ["___", "", "é"] {
            let name = sandbox_name_for(project, worktree, agent);
            assert!(within_sbx_rules(&name), "{agent:?}: {name}");
        }
    }

    #[test]
    fn wrap_runs_the_supervisor_for_the_panes_agent() {
        let result = wrap_for_docker_sbx(
            "codex",
            None,
            "codex",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
        )
        .unwrap();
        assert_eq!(
            result,
            " workmux sandbox run '/tmp/project' --agent 'codex' -- 'codex'"
        );
    }

    #[test]
    fn wrap_quotes_the_agent_and_command() {
        let result = wrap_for_docker_sbx(
            " claude -- \"$(cat PROMPT.md)\" 'x'",
            None,
            "it's mine",
            Path::new("/tmp/project"),
            Path::new("/tmp/project/backend"),
        )
        .unwrap();
        assert_eq!(
            result,
            " workmux sandbox run '/tmp/project/backend' --worktree-root '/tmp/project' \
             --agent 'it'\\''s mine' -- 'claude -- \"$(cat PROMPT.md)\" '\\''x'\\'''"
        );
    }

    #[test]
    fn wrap_with_pinned_supervisor() {
        let result = wrap_for_docker_sbx(
            "claude",
            Some("'/n/bin/workmux-x' sandbox run"),
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
        )
        .unwrap();
        assert!(
            result.starts_with(" '/n/bin/workmux-x' sandbox run '/tmp/project' --agent"),
            "{result}"
        );
    }

    #[test]
    fn long_or_odd_project_names_stay_within_sbx_rules() {
        let long = format!("/code/{}", "x_".repeat(60));
        for project in ["/code/___", "/", long.as_str()] {
            let name = sandbox_name_for(Path::new(project), Path::new("/code/wt"), "claude");
            assert!(within_sbx_rules(&name), "{name}");
        }
    }
}
