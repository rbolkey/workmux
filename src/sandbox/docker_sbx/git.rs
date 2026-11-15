//! The git metadata a worktree's sandbox mounts, as found on the host.
//!
//! The launch plan decides which of these paths are read-only (see
//! `plan::git_workspaces`). sbx mounts only paths that exist, so the policy
//! files and directories the plan protects are created here first, as the
//! container backend creates them.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use super::plan::GitMetadata;

/// Find a linked worktree's git metadata and make the paths the plan protects
/// exist. Fails for the main worktree, as the container backend does: its
/// admin files are the main repository's own.
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
        for dir in ["hooks", "info", "objects/info", "modules", "worktrees"] {
            let path = root.join(dir);
            std::fs::create_dir_all(&path)?;
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("Git policy path must be a directory: {}", path.display());
            }
        }
        for name in ["config", "config.worktree"] {
            ensure_policy_file(&root.join(name))?;
        }
    }

    let config_paths = config_paths(&common.join("config"), &identity.worktree)?
        .into_iter()
        .filter(|path| path.starts_with(&identity.worktree) || path.starts_with(common))
        .collect();

    Ok(GitMetadata {
        pointer_files: worktree_git_pointers(&identity.worktree)?,
        module_roots,
        config_paths,
        rr_cache: common.join("rr-cache").is_dir(),
        common_dir: identity.common_dir,
        admin_dir: identity.admin_dir,
    })
}

/// Reject hook layouts a read-only hooks directory wouldn't protect.
fn validate_git_hooks(common_dir: &Path) -> Result<()> {
    let hooks = common_dir.join("hooks");
    if hooks.exists() {
        let metadata = std::fs::symlink_metadata(&hooks)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("Git hooks path must be a directory: {}", hooks.display());
        }
        for entry in std::fs::read_dir(&hooks)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("Git hooks must not be symbolic links: {}", path.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() > 1 {
                    bail!("Git hooks must not have hard links: {}", path.display());
                }
            }
        }
    }
    Ok(())
}

/// Make a policy file exist, refusing one a read-only mount wouldn't protect.
fn ensure_policy_file(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("Git policy path must be a regular file: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                bail!(
                    "Git policy files must not have hard links: {}",
                    path.display()
                );
            }
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"")?;
    Ok(())
}

/// `.git` pointer files under the worktree, not following symlinks.
fn worktree_git_pointers(worktree: &Path) -> Result<Vec<PathBuf>> {
    fn visit(dir: &Path, pointers: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if path.file_name() == Some(std::ffi::OsStr::new(".git")) {
                if metadata.is_file() {
                    pointers.push(path);
                }
                continue;
            }
            if metadata.is_dir() {
                visit(&path, pointers)?;
            }
        }
        Ok(())
    }

    let mut pointers = Vec::new();
    visit(worktree, &mut pointers)?;
    Ok(pointers)
}

/// Submodule git dirs under `<common_dir>/modules`, nested ones included.
fn module_git_roots(common_dir: &Path) -> Result<Vec<PathBuf>> {
    fn visit(path: &Path, roots: &mut Vec<PathBuf>) -> Result<()> {
        if !path.is_dir() {
            return Ok(());
        }
        let is_root = path.join("config").is_file()
            || (path.join("HEAD").is_file() && path.join("objects").is_dir());
        if is_root {
            roots.push(path.to_path_buf());
        }
        let children = if is_root {
            path.join("modules")
        } else {
            path.to_path_buf()
        };
        if children.is_dir() {
            for entry in std::fs::read_dir(children)? {
                let child = entry?.path();
                if child.is_dir() {
                    visit(&child, roots)?;
                }
            }
        }
        Ok(())
    }

    let mut roots = Vec::new();
    let modules = common_dir.join("modules");
    if modules.is_dir() {
        for entry in std::fs::read_dir(modules)? {
            let child = entry?.path();
            if child.is_dir() {
                visit(&child, &mut roots)?;
            }
        }
    }
    Ok(roots)
}

/// `git config --file <config> --get-regexp <pattern>`, as (key, value) pairs.
fn config_entries(config: &Path, pattern: &str) -> Result<Vec<(String, String)>> {
    let output = crate::git::unattended_git(None)?
        .args(["config", "--file"])
        .arg(config)
        .args(["--get-regexp", pattern])
        .output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!("Failed to inspect Git config in {}", config.display());
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect())
}

/// Files the repository config includes, and the programs it runs, that
/// exist: an agent able to change them could change what git does on the host.
///
/// Paths resolve as git resolves them: an include relative to the config
/// file that names it; `core.hooksPath` and the programs (`core.fsmonitor`,
/// diff, filter and merge drivers) relative to the top of `worktree`, where
/// git runs hooks and commands, a program without a slash being looked up
/// on `PATH`; `~/` in the home directory.
fn config_paths(config: &Path, worktree: &Path) -> Result<Vec<PathBuf>> {
    fn visit(
        config: &Path,
        worktree: &Path,
        visited: &mut std::collections::HashSet<PathBuf>,
        paths: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let config = config.canonicalize()?;
        if !visited.insert(config.clone()) {
            return Ok(());
        }
        let dir = config.parent().unwrap_or(Path::new("/")).to_path_buf();
        for (_, value) in config_entries(&config, "^include.*\\.path$")? {
            if value.contains(['*', '?', '[', '%']) {
                bail!("Git config include path cannot be protected safely: {value}");
            }
            let include = dir.join(crate::util::expand_tilde(&value));
            if !include.is_file() {
                bail!(
                    "Git config include target must exist before sandbox startup: {}",
                    include.display()
                );
            }
            paths.push(include.clone());
            visit(&include, worktree, visited, paths)?;
        }

        // git matches the pattern against lowercased keys.
        for (key, value) in config_entries(
            &config,
            "^(core\\.(hookspath|fsmonitor)|diff\\..*\\.(command|textconv)|filter\\..*\\.(clean|smudge|process)|merge\\..*\\.driver)$",
        )? {
            let hooks_path = key.eq_ignore_ascii_case("core.hooksPath");
            if key.eq_ignore_ascii_case("core.fsmonitor")
                && matches!(value.as_str(), "true" | "false")
            {
                continue;
            }
            let token = if hooks_path {
                value.as_str()
            } else {
                value
                    .trim_start_matches('!')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(['\'', '"'])
            };
            if token.is_empty() || token.contains(['$', '`', '%']) {
                continue;
            }
            // A program named without a slash comes from PATH.
            if !hooks_path && !token.contains('/') {
                continue;
            }
            let candidate = worktree.join(crate::util::expand_tilde(token));
            if candidate.exists() {
                paths.push(candidate);
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(
        config,
        worktree,
        &mut std::collections::HashSet::new(),
        &mut paths,
    )?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn main_worktree_is_refused() {
        let (_temp, main, _worktree) = linked_worktree();
        let error = git_metadata(&main).unwrap_err();
        assert!(
            error.to_string().contains("linked Git worktree"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_hooks_are_refused() {
        let (temp, main, worktree) = linked_worktree();
        let target = temp.path().join("target");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&target, main.join(".git/hooks/pre-commit")).unwrap();

        let error = git_metadata(&worktree).unwrap_err();
        assert!(error.to_string().contains("Git hooks"), "{error:#}");
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
