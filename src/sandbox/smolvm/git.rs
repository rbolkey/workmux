//! The git metadata of a smol machine.
//!
//! smolvm enforces read-only only on a top-level share: a `:ro` share nested
//! inside a writable one is guest-enforced, and guest root can unmount it.
//! So the main repository's `.git` is one read-only share, and only the
//! directories git writes from a linked worktree are writable shares inside
//! it. Its hooks and config stay read-only, even to guest root.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::sandbox::container::module_git_roots;

/// The main repository's git metadata, as a worktree's machine mounts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLayout {
    /// The main repository's `.git`, mounted read-only.
    pub common_dir: PathBuf,
    /// Directories inside `common_dir` mounted writable: `objects`, `refs`,
    /// `logs`, `rr-cache` and the worktree's admin directory, plus the same
    /// data directories of submodule git roots. Only ones that exist.
    pub writable: Vec<PathBuf>,
}

/// The data directories of a git dir that git writes from a linked worktree.
const DATA_DIRS: [&str; 4] = ["objects", "refs", "logs", "rr-cache"];

/// Find the git metadata `worktree` needs.
pub fn gather(worktree: &Path) -> Result<GitLayout> {
    let identity = crate::git::RepositoryIdentity::discover(worktree)
        .context("Refusing to sandbox a worktree with invalid Git metadata")?;
    if identity.is_bare || identity.admin_dir == identity.common_dir {
        bail!(
            "smolvm sandboxes require a linked Git worktree: {} is the main worktree, \
             whose .git directory would be writable to the agent",
            identity.worktree.display()
        );
    }
    let common_dir = identity.common_dir;

    let existing = |root: &Path| -> Vec<PathBuf> {
        DATA_DIRS
            .iter()
            .map(|dir| root.join(dir))
            .filter(|dir| dir.is_dir())
            .collect()
    };
    let mut writable = existing(&common_dir);
    writable.push(identity.admin_dir);
    // Submodule git roots keep their config and hooks read-only, as the main
    // repository's.
    for root in module_git_roots(&common_dir)? {
        writable.extend(existing(&root));
    }

    Ok(GitLayout {
        common_dir,
        writable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repository `main` with one commit and a linked worktree `wt`.
    fn linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let main = root.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&["init", "-q"], &main);
        git(
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
            &main,
        );
        git(&["worktree", "add", "-q", "-b", "topic", "../wt"], &main);
        (temp, main, root.join("wt"))
    }

    #[test]
    fn test_a_linked_worktree_writes_only_git_data_and_its_admin_dir() {
        let (_temp, main, worktree) = linked_worktree();
        let common = main.join(".git");
        std::fs::create_dir_all(common.join("rr-cache")).unwrap();

        let layout = gather(&worktree).unwrap();

        assert_eq!(layout.common_dir, common);
        assert_eq!(
            layout.writable,
            ["objects", "refs", "logs", "rr-cache", "worktrees/wt"]
                .iter()
                .map(|child| common.join(child))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_submodule_git_roots_get_their_data_dirs_but_not_their_config_or_hooks() {
        let (_temp, main, worktree) = linked_worktree();
        let module = main.join(".git/modules/lib");
        for dir in ["objects", "refs", "hooks"] {
            std::fs::create_dir_all(module.join(dir)).unwrap();
        }
        std::fs::write(module.join("config"), "").unwrap();

        let layout = gather(&worktree).unwrap();

        assert!(layout.writable.contains(&module.join("objects")));
        assert!(layout.writable.contains(&module.join("refs")));
        assert!(
            !layout
                .writable
                .iter()
                .any(|dir| dir == &module || dir.ends_with("hooks")),
            "{:?}",
            layout.writable
        );
    }

    #[test]
    fn test_the_main_worktree_is_refused() {
        let (_temp, main, _worktree) = linked_worktree();
        let error = gather(&main).unwrap_err().to_string();
        assert!(error.contains("linked Git worktree"), "{error}");
    }
}
