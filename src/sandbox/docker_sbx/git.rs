//! The git metadata a worktree's sandbox mounts, as found on the host.
//!
//! sbx mounts only paths that exist, so the files and directories the plan
//! protects are created here first.

use anyhow::{Context, Result, bail};
use std::path::Path;

use super::plan::GitMetadata;
use crate::sandbox::git_boundary::{
    POLICY_DIRS, ProgramPaths, collect_worktree_git_pointers, config_include_paths,
    ensure_policy_dir, ensure_policy_file, module_git_roots, validate_git_hooks,
};

/// Find a linked worktree's git metadata and create the paths the plan
/// protects. Fails for the main worktree, as the container backend does.
pub fn git_metadata(worktree: &Path) -> Result<GitMetadata> {
    let identity = crate::git::RepositoryIdentity::discover(worktree)
        .context("Refusing to sandbox a worktree with invalid Git metadata")?;
    if identity.is_bare || identity.admin_dir == identity.common_dir {
        bail!("Docker Sandboxes require a linked Git worktree");
    }
    validate_git_hooks(&identity.common_dir)?;

    let common = &identity.common_dir;
    for dir in ["logs", "objects/info"] {
        std::fs::create_dir_all(common.join(dir))?;
    }
    ensure_policy_file(&identity.admin_dir.join("config.worktree"))?;

    let module_roots = module_git_roots(common)?;
    for root in &module_roots {
        for dir in POLICY_DIRS {
            ensure_policy_dir(&root.join(dir))?;
        }
        for name in ["config", "config.worktree"] {
            ensure_policy_file(&root.join(name))?;
        }
    }

    let config = common.join("config");
    let config_paths = config_include_paths(&config, ProgramPaths::InWorktree(&identity.worktree))?
        .into_iter()
        .filter(|path| path.starts_with(&identity.worktree) || path.starts_with(common))
        .collect();

    Ok(GitMetadata {
        pointer_files: collect_worktree_git_pointers(&identity.worktree)?,
        module_roots,
        config_paths,
        rr_cache: common.join("rr-cache").is_dir(),
        common_dir: identity.common_dir,
        admin_dir: identity.admin_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(args: &[&str], cwd: &Path) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap()
                .success()
        );
    }

    /// A main repository with a linked worktree beside it.
    fn linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&["init", "-q"], &main);
        git(&["config", "user.name", "Test"], &main);
        git(&["config", "user.email", "test@example.com"], &main);
        git(&["commit", "-q", "--allow-empty", "-m", "base"], &main);
        let worktree = temp.path().join("worktree");
        git(
            &[
                "worktree",
                "add",
                "-qb",
                "topic",
                worktree.to_str().unwrap(),
            ],
            &main,
        );
        (
            temp,
            main.canonicalize().unwrap(),
            worktree.canonicalize().unwrap(),
        )
    }

    #[test]
    fn linked_worktree_metadata_names_its_git_dirs_and_creates_policy_paths() {
        let (_temp, main, worktree) = linked_worktree();
        let git = git_metadata(&worktree).unwrap();

        let common = main.join(".git");
        assert_eq!(git.common_dir, common);
        assert_eq!(git.admin_dir, common.join("worktrees/worktree"));
        assert_eq!(git.pointer_files, vec![worktree.join(".git")]);
        assert!(git.module_roots.is_empty());
        assert!(!git.rr_cache);
        // sbx mounts only existing paths.
        assert!(common.join("logs").is_dir());
        assert!(common.join("objects/info").is_dir());
        assert!(git.admin_dir.join("config.worktree").is_file());
    }

    #[test]
    fn config_includes_and_programs_in_the_worktree_are_protected() {
        let (temp, main, worktree) = linked_worktree();
        std::fs::write(worktree.join("extra.gitconfig"), "").unwrap();
        std::fs::write(worktree.join("fsmonitor.sh"), "").unwrap();
        let outside = temp.path().join("outside.gitconfig");
        std::fs::write(&outside, "").unwrap();
        for (key, value) in [
            ("include.path", worktree.join("extra.gitconfig")),
            ("include.path", outside.clone()),
            ("core.fsmonitor", worktree.join("fsmonitor.sh")),
        ] {
            git(&["config", "--add", key, value.to_str().unwrap()], &main);
        }

        let git = git_metadata(&worktree).unwrap();
        assert_eq!(
            git.config_paths,
            vec![
                worktree.join("extra.gitconfig"),
                worktree.join("fsmonitor.sh")
            ]
        );
    }

    #[test]
    fn relative_hooks_and_programs_resolve_against_the_worktree_as_git_does() {
        // Husky sets `core.hooksPath .husky/_`; git runs hooks, filters and
        // diff drivers from the top of the working tree, not the config's
        // directory, and looks programs without a slash up on PATH.
        let (_temp, main, worktree) = linked_worktree();
        std::fs::create_dir_all(worktree.join(".husky/_")).unwrap();
        std::fs::create_dir_all(worktree.join("scripts")).unwrap();
        std::fs::write(worktree.join("scripts/clean.sh"), "").unwrap();
        std::fs::write(worktree.join("git-lfs"), "").unwrap();
        std::fs::create_dir_all(main.join(".git/.husky/_")).unwrap();
        for (key, value) in [
            ("core.hooksPath", ".husky/_"),
            ("filter.x.clean", "./scripts/clean.sh %f"),
            ("filter.lfs.process", "git-lfs filter-process"),
        ] {
            git(&["config", key, value], &main);
        }

        let git = git_metadata(&worktree).unwrap();
        assert_eq!(
            git.config_paths,
            vec![worktree.join(".husky/_"), worktree.join("scripts/clean.sh")]
        );
    }

    #[test]
    fn scripts_programs_run_through_an_interpreter_are_protected_too() {
        // `sh ./scripts/clean.sh` runs the script, not just `sh`: every word
        // naming a file in the worktree or git dir is protected. Directories
        // and words naming nothing stay writable.
        let (_temp, main, worktree) = linked_worktree();
        std::fs::create_dir_all(worktree.join("scripts")).unwrap();
        for file in ["scripts/clean.sh", "scripts/smudge.py", "merge.rb"] {
            std::fs::write(worktree.join(file), "").unwrap();
        }
        std::fs::write(main.join(".git/textconv.sh"), "").unwrap();
        let textconv = main.join(".git/textconv.sh");
        for (key, value) in [
            ("filter.x.clean", "sh ./scripts/clean.sh %f"),
            ("filter.x.smudge", "python3 'scripts/smudge.py' --quiet ."),
            ("merge.x.driver", "ruby merge.rb %O %A %B"),
            ("diff.x.textconv", &format!("bash {}", textconv.display())),
        ] {
            git(&["config", key, value], &main);
        }

        let git = git_metadata(&worktree).unwrap();
        let mut paths = git.config_paths.clone();
        paths.sort();
        let mut expected = vec![
            worktree.join("scripts/clean.sh"),
            worktree.join("scripts/smudge.py"),
            worktree.join("merge.rb"),
            textconv,
        ];
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[test]
    fn main_worktree_is_refused() {
        let (_temp, main, _worktree) = linked_worktree();
        let error = git_metadata(&main).unwrap_err();
        assert!(
            error.to_string().contains("linked Git worktree"),
            "{error:#}"
        );
    }

    #[test]
    fn submodule_git_dirs_are_found_and_given_policy_paths() {
        let (_temp, main, worktree) = linked_worktree();
        let module = main.join(".git/modules/vendor");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("config"), "[core]\n\tbare = false\n").unwrap();

        let git = git_metadata(&worktree).unwrap();
        assert_eq!(git.module_roots, vec![module.clone()]);
        for dir in ["hooks", "info", "objects/info", "modules", "worktrees"] {
            assert!(module.join(dir).is_dir(), "{dir}");
        }
        assert!(module.join("config.worktree").is_file());
    }
}
