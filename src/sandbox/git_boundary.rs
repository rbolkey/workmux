//! Checks and discovery for the git metadata a sandbox mounts read-only.
//!
//! Both the container and docker_sbx backends keep an agent from changing what
//! git runs on the host, each building its own mounts from these.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Reject hook layouts whose executable policy can be changed outside the protected directory.
pub(super) fn validate_git_hooks(common_dir: &Path) -> Result<()> {
    let hooks = common_dir.join("hooks");
    if hooks.exists() {
        let metadata = std::fs::symlink_metadata(&hooks)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("Git hooks path must be a directory: {}", hooks.display());
        }
        for entry in std::fs::read_dir(&hooks)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                anyhow::bail!("Git hooks must not be symbolic links: {}", path.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() > 1 {
                    anyhow::bail!("Git hooks must not have hard links: {}", path.display());
                }
            }
        }
    }
    Ok(())
}

/// Create a policy file, refusing one a read-only mount wouldn't protect.
pub(super) fn ensure_policy_file(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("Git policy path must be a regular file: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                anyhow::bail!(
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

/// Create a policy directory, refusing a symlink or a non-directory.
pub(super) fn ensure_policy_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("Git policy path must be a directory: {}", path.display());
    }
    Ok(())
}

/// Collect `.git` pointer files under the worktree, not following symlinks.
pub(super) fn collect_worktree_git_pointers(worktree: &Path) -> Result<Vec<PathBuf>> {
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

/// Collect submodule git dirs under `<common_dir>/modules`, nested ones included.
pub(super) fn module_git_roots(common_dir: &Path) -> Result<Vec<PathBuf>> {
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

/// Where `config_include_paths` looks for the programs a config runs.
#[derive(Clone, Copy)]
pub(super) enum ProgramPaths<'a> {
    /// The first word of each program, relative to its config's directory.
    /// The container backend's rule.
    BesideConfig,
    /// As git resolves them, from the top of this worktree. Every word naming
    /// a file counts, so a script run through an interpreter is protected too.
    InWorktree(&'a Path),
}

impl ProgramPaths<'_> {
    /// Return the `git config --get-regexp` pattern for keys naming programs.
    ///
    /// git lowercases keys before matching, so `BesideConfig`'s `hooksPath`
    /// never matches. It's kept so the container backend's mounts don't change.
    fn pattern(self) -> &'static str {
        match self {
            ProgramPaths::BesideConfig => {
                "^(core\\.(hooksPath|fsmonitor)|diff\\..*\\.(command|textconv)|filter\\..*\\.(clean|smudge|process)|merge\\..*\\.driver)$"
            }
            ProgramPaths::InWorktree(_) => {
                "^(core\\.(hookspath|fsmonitor)|diff\\..*\\.(command|textconv)|filter\\..*\\.(clean|smudge|process)|merge\\..*\\.driver)$"
            }
        }
    }
}

/// Collect the existing files the repository config includes or runs, which
/// an agent could change to change what git does on the host.
pub(super) fn config_include_paths(
    config: &Path,
    programs: ProgramPaths<'_>,
) -> Result<Vec<PathBuf>> {
    fn visit(
        config: &Path,
        programs: ProgramPaths<'_>,
        visited: &mut std::collections::HashSet<PathBuf>,
        paths: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let config = config.canonicalize()?;
        if !visited.insert(config.clone()) {
            return Ok(());
        }
        let config_dir = config.parent().unwrap_or(Path::new("/"));
        let output = crate::git::unattended_git(None)?
            .args([
                "config",
                "--file",
                config.to_string_lossy().as_ref(),
                "--get-regexp",
                "^include.*\\.path$",
            ])
            .output()?;
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!(
                "Failed to inspect Git config includes in {}",
                config.display()
            );
        }
        for line in String::from_utf8(output.stdout)?.lines() {
            let Some((_, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            if value.contains(['*', '?', '[', '%']) {
                anyhow::bail!("Git config include path cannot be protected safely: {value}");
            }
            let include = if let Some(rest) = value.strip_prefix("~/") {
                home::home_dir()
                    .context("Could not resolve home directory for Git config include")?
                    .join(rest)
            } else {
                let path = Path::new(value);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    config_dir.join(path)
                }
            };
            if !include.is_file() {
                anyhow::bail!(
                    "Git config include target must exist before sandbox startup: {}",
                    include.display()
                );
            }
            paths.push(include.clone());
            visit(&include, programs, visited, paths)?;
        }

        let executable_output = crate::git::unattended_git(None)?
            .args([
                "config",
                "--file",
                config.to_string_lossy().as_ref(),
                "--get-regexp",
                programs.pattern(),
            ])
            .output()?;
        if !executable_output.status.success() && executable_output.status.code() != Some(1) {
            anyhow::bail!(
                "Failed to inspect executable Git config in {}",
                config.display()
            );
        }
        for line in String::from_utf8(executable_output.stdout)?.lines() {
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            if key.eq_ignore_ascii_case("core.fsmonitor") && matches!(value, "true" | "false") {
                continue;
            }
            match programs {
                ProgramPaths::BesideConfig => program_beside_config(value, config_dir, paths),
                ProgramPaths::InWorktree(worktree) => {
                    program_in_worktree(key, value, worktree, paths)
                }
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(
        config,
        programs,
        &mut std::collections::HashSet::new(),
        &mut paths,
    )?;
    Ok(paths)
}

fn program_beside_config(value: &str, config_dir: &Path, paths: &mut Vec<PathBuf>) {
    let token = value
        .trim_start_matches('!')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(['\'', '"']);
    if token.is_empty() || token.contains(['$', '`', '%']) {
        return;
    }
    let candidate = Path::new(token);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        config_dir.join(candidate)
    };
    if candidate.exists() {
        paths.push(candidate);
    }
}

fn program_in_worktree(key: &str, value: &str, worktree: &Path, paths: &mut Vec<PathBuf>) {
    if key.eq_ignore_ascii_case("core.hooksPath") {
        if !value.is_empty() && !value.contains(['$', '`', '%']) {
            let candidate = worktree.join(crate::util::expand_tilde(value));
            if candidate.exists() {
                paths.push(candidate);
            }
        }
        return;
    }
    let words = value.trim_start_matches('!').split_whitespace();
    for (i, word) in words.enumerate() {
        let token = word.trim_matches(['\'', '"']);
        if token.is_empty() || token.contains(['$', '`', '%']) {
            continue;
        }
        if i == 0 && !token.contains('/') {
            continue;
        }
        let candidate = worktree.join(crate::util::expand_tilde(token));
        if candidate.is_file() && !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(config: &Path, key: &str, value: &str) {
        let status = std::process::Command::new("git")
            .args(["config", "--file"])
            .arg(config)
            .args(["--add", key, value])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn beside_config_programs_resolve_their_first_word_against_the_config_dir() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let config = dir.join("config");
        std::fs::write(&config, "").unwrap();
        std::fs::write(dir.join("clean.sh"), "").unwrap();
        std::fs::write(dir.join("script.sh"), "").unwrap();
        set(&config, "filter.x.clean", "./clean.sh %f");
        set(&config, "filter.x.smudge", "sh ./script.sh");
        set(&config, "core.fsmonitor", "true");

        let paths = config_include_paths(&config, ProgramPaths::BesideConfig).unwrap();
        assert_eq!(paths, vec![dir.join("./clean.sh")]);
    }

    #[test]
    fn beside_config_leaves_hooks_path_out() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let config = dir.join("config");
        std::fs::write(&config, "").unwrap();
        std::fs::create_dir(dir.join("hooks")).unwrap();
        set(&config, "core.hooksPath", "hooks");

        let paths = config_include_paths(&config, ProgramPaths::BesideConfig).unwrap();
        assert_eq!(paths, Vec::<PathBuf>::new());
        let paths = config_include_paths(&config, ProgramPaths::InWorktree(&dir)).unwrap();
        assert_eq!(paths, vec![dir.join("hooks")]);
    }

    #[test]
    fn includes_are_followed_and_named() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let config = dir.join("config");
        std::fs::write(&config, "").unwrap();
        let nested = dir.join("nested.gitconfig");
        std::fs::write(&nested, "").unwrap();
        std::fs::write(dir.join("fsmonitor.sh"), "").unwrap();
        set(&config, "include.path", "nested.gitconfig");
        set(&nested, "core.fsmonitor", "./fsmonitor.sh");

        let paths = config_include_paths(&config, ProgramPaths::BesideConfig).unwrap();
        assert_eq!(paths, vec![nested, dir.join("./fsmonitor.sh")]);
    }

    #[test]
    fn unprotectable_include_paths_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        std::fs::write(&config, "").unwrap();
        set(&config, "include.path", "*.gitconfig");

        let error = config_include_paths(&config, ProgramPaths::BesideConfig).unwrap_err();
        assert!(
            error.to_string().contains("cannot be protected"),
            "{error:#}"
        );
    }
}
