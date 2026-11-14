//! Docker Sandboxes (`sbx`) backend for sandbox.
//!
//! Runs each worktree's agents in its own sbx microVM sandbox. The worktree,
//! the main repository's git dir and the agent's config directory are
//! mounted at their workstation paths; the agent command runs through
//! `sbx exec`, as on the container backend.

mod instance;
mod plan;
mod template;
mod wrap;

pub(crate) use instance::sbx_state_dir_path;
pub use instance::{
    SbxInstance, SbxSandboxInfo, ensure_sandbox, new_session, prepare_launch,
    remove_sandbox_for_worktree, run_exec,
};
pub use plan::{ExecRequest, launch_plan};
pub use wrap::wrap_for_docker_sbx;

/// Prefix for all workmux-managed sbx sandbox names.
pub const SANDBOX_PREFIX: &str = "wm-";

use anyhow::Result;
use std::path::Path;
use tracing::debug;

/// Longest sandbox name sbx accepts.
const MAX_NAME_LEN: usize = 63;

/// Generate the sandbox name for a worktree: `wm-<project>-<hash8>`.
///
/// Each worktree gets its own sandbox, so the hash is of the worktree path;
/// the project directory name is there for recognising sandboxes in `sbx ls`.
/// Names stay within sbx's rules: lowercase letters, digits and `-`, at most
/// 63 characters.
pub fn sandbox_name(worktree_root: &Path) -> Result<String> {
    let project_root = crate::sandbox::lima::determine_project_root(worktree_root)?;
    let canonical = worktree_root
        .canonicalize()
        .unwrap_or_else(|_| worktree_root.to_path_buf());
    let name = sandbox_name_for(&project_root, &canonical);
    debug!(sandbox = %name, "resolved sbx sandbox name");
    Ok(name)
}

fn sandbox_name_for(project_root: &Path, worktree_root: &Path) -> String {
    use crate::sandbox::lima::{hash_key, sanitize_name};

    let hash = hash_key(&worktree_root.to_string_lossy(), 8);
    let project_dir_name = project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Budget: "wm-" (3) + project + "-" (1) + hash (8)
    let budget = MAX_NAME_LEN - SANDBOX_PREFIX.len() - 1 - hash.len();
    let sanitized = sanitize_name(&project_dir_name, budget);

    if sanitized.is_empty() {
        format!("{}{}", SANDBOX_PREFIX, hash)
    } else {
        format!("{}{}-{}", SANDBOX_PREFIX, sanitized, hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        assert!(name.starts_with("wm-my-project-"), "{name}");
        assert_eq!(name.len(), "wm-my-project-".len() + 8);
        assert!(within_sbx_rules(&name), "{name}");
    }

    #[test]
    fn each_worktree_gets_its_own_sandbox() {
        let project = Path::new("/code/proj");
        assert_ne!(
            sandbox_name_for(project, Path::new("/code/proj__worktrees/a")),
            sandbox_name_for(project, Path::new("/code/proj__worktrees/b"))
        );
        assert_eq!(
            sandbox_name_for(project, Path::new("/code/proj__worktrees/a")),
            sandbox_name_for(project, Path::new("/code/proj__worktrees/a"))
        );
    }

    #[test]
    fn long_or_odd_project_names_stay_within_sbx_rules() {
        let long = format!("/code/{}", "x_".repeat(60));
        for project in ["/code/___", "/", long.as_str()] {
            let name = sandbox_name_for(Path::new(project), Path::new("/code/wt"));
            assert!(within_sbx_rules(&name), "{name}");
        }
    }
}
