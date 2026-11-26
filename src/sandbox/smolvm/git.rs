//! The git metadata boundary of a smol machine.
//!
//! smolvm enforces read-only only on a top-level share: a `:ro` share nested
//! inside a writable one is guest-enforced, and guest root can unmount it.
//! So the main repository's `.git` is one read-only share, and only the
//! directories git writes from a linked worktree are writable shares inside
//! it. Its hooks and config stay read-only, even to guest root.
//!
//! What smolvm can't protect: the worktree's `.git` file sits in the
//! writable worktree share (smolvm mounts directories only); the worktree's
//! admin directory (`.git/worktrees/<name>`, with its submodule git dirs)
//! holds files git writes, so it is writable as a whole; and
//! `objects/info/alternates` is inside the writable `objects` share. Each
//! can redirect host git, which the user accepts once per project.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::SandboxConfig;
use crate::sandbox::acknowledgement::GitIsolationAcknowledgement;
use crate::sandbox::container::{config_include_paths, module_git_roots, validate_git_hooks};

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

/// Find the git metadata `worktree` needs, refusing layouts whose host-run
/// policy the agent could change through a writable share.
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
    validate_git_hooks(&common_dir)?;

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
        validate_git_hooks(&root)?;
        writable.extend(existing(&root));
    }

    // Files the config includes or runs must not be in a writable share.
    let mut writable_roots = vec![identity.worktree.clone(), worktree.to_path_buf()];
    writable_roots.extend(writable.iter().cloned());
    for path in config_include_paths(&common_dir.join("config"))? {
        if writable_roots.iter().any(|root| path.starts_with(root)) {
            bail!(
                "The repository's Git config includes or runs {}, which a smol machine \
                 would mount writable to the agent; move it out of the worktree and the \
                 writable Git directories",
                path.display()
            );
        }
    }

    Ok(GitLayout {
        common_dir,
        writable,
    })
}

const WARNING: &str = "warning: smolvm can't fully protect a worktree's Git metadata. The main \
repository's .git, with its hooks and config, is read-only to the agent, even as guest root. Writable \
to it: the worktree's .git file; its whole admin dir (.git/worktrees/<name>: gitdir, commondir, \
config.worktree, HEAD, index); its submodule git dirs; and objects/info/alternates in the writable \
objects share. These can redirect or reconfigure Git commands you run in the worktree on the host. \
Docker, Podman, and Apple Container protect them.";

const REFUSAL: &str = "smolvm requires explicit acceptance of reduced Git metadata isolation, \
once per project. Accept at the prompt in a terminal, or set \
sandbox.smolvm.accept_reduced_git_metadata_isolation: true in your global config \
(~/.config/workmux/config.yaml) for unattended use, or use Docker, Podman, or Apple Container.";

/// The once-per-project acceptance of the gap smolvm can't close.
/// `project_key` is a file name: pass a slug such as
/// [`super::machines::MachineIdentity::project_key`].
pub(crate) fn git_isolation_acknowledgement<'a>(
    sandbox: &SandboxConfig,
    state_root: &Path,
    project_key: &'a str,
) -> GitIsolationAcknowledgement<'a> {
    GitIsolationAcknowledgement {
        backend: "smolvm",
        state_dir: state_root
            .join("smolvm-acknowledgements")
            .join("git-metadata-isolation-v1"),
        scope_key: project_key,
        warning: WARNING,
        refusal: REFUSAL,
        accepted_by_config: sandbox.smolvm.accepts_reduced_git_metadata_isolation(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;
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
    fn test_the_prompt_states_the_gap_once_per_project() {
        let state_root = tempfile::tempdir().unwrap();
        let ack = |sandbox: &SandboxConfig| -> (Result<()>, String) {
            let mut stderr = Vec::new();
            let result = git_isolation_acknowledgement(sandbox, state_root.path(), "proj-0a1b2c3d")
                .acknowledge_with(None, &mut stderr);
            (result, String::from_utf8(stderr).unwrap())
        };

        // Unattended and not accepted: the gap is stated, then refused,
        // pointing at the global setting.
        let (result, warning) = ack(&SandboxConfig::default());
        for gap in [
            ".git file",
            "whole admin dir",
            "gitdir, commondir, config.worktree, HEAD, index",
            "submodule git dirs",
            "objects/info/alternates",
            "redirect",
        ] {
            assert!(warning.contains(gap), "{gap}: {warning}");
        }
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("sandbox.smolvm.accept_reduced_git_metadata_isolation"),
            "{error}"
        );
        assert!(error.contains("global"), "{error}");

        let mut accepting = SandboxConfig::default();
        accepting.smolvm.accept_reduced_git_metadata_isolation = Some(true);
        ack(&accepting).0.unwrap();
        let stored = state_root
            .path()
            .join("smolvm-acknowledgements/git-metadata-isolation-v1/proj-0a1b2c3d");
        assert_eq!(std::fs::read_to_string(stored).unwrap(), "accepted-v1\n");

        // Accepted once for the project: no warning, no refusal.
        let (result, warning) = ack(&SandboxConfig::default());
        result.unwrap();
        assert!(warning.is_empty(), "{warning}");
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

    #[test]
    fn test_config_that_runs_a_program_from_a_writable_mount_is_refused() {
        let (_temp, main, worktree) = linked_worktree();
        std::fs::write(worktree.join("monitor.sh"), "").unwrap();
        git(
            &[
                "config",
                "core.fsmonitor",
                &worktree.join("monitor.sh").to_string_lossy(),
            ],
            &main,
        );
        let error = format!("{:#}", gather(&worktree).unwrap_err());
        assert!(error.contains("monitor.sh"), "{error}");
        assert!(error.contains("writable"), "{error}");
    }

    #[test]
    fn test_symlinked_hooks_are_refused() {
        let (_temp, main, worktree) = linked_worktree();
        std::os::unix::fs::symlink("/bin/true", main.join(".git/hooks/pre-commit")).unwrap();
        let error = gather(&worktree).unwrap_err().to_string();
        assert!(error.contains("symbolic links"), "{error}");
    }

    #[test]
    fn test_symlinked_submodule_hooks_are_refused() {
        let (_temp, main, worktree) = linked_worktree();
        let module = main.join(".git/modules/lib");
        for dir in ["objects", "refs", "hooks"] {
            std::fs::create_dir_all(module.join(dir)).unwrap();
        }
        std::fs::write(module.join("config"), "").unwrap();
        std::os::unix::fs::symlink("/bin/true", module.join("hooks/pre-commit")).unwrap();
        let error = gather(&worktree).unwrap_err().to_string();
        assert!(error.contains("symbolic links"), "{error}");
    }
}
