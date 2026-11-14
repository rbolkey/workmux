//! Command wrapping for the docker_sbx backend.

use anyhow::Result;
use std::path::Path;

use crate::shell::shell_escape;

/// Wrap a command to run inside the worktree's sbx sandbox via the sandbox
/// supervisor.
///
/// Generates a `workmux sandbox run` command. The supervisor starts the RPC
/// server and shims, makes sure the sandbox exists with its policy rules,
/// and runs the command with `sbx exec -it`.
///
/// # Arguments
/// * `command` - The command string to run (may contain shell operators)
/// * `worktree_root` - The worktree root, which names and mounts the sandbox
/// * `pane_cwd` - Working directory inside the sandbox
pub fn wrap_for_docker_sbx(command: &str, worktree_root: &Path, pane_cwd: &Path) -> Result<String> {
    // Strip the leading history-prevention space before passing the command to
    // the sandbox supervisor.
    let command = command.strip_prefix(' ').unwrap_or(command);

    let mut parts = format!(
        "workmux sandbox run '{}'",
        shell_escape(&pane_cwd.to_string_lossy()),
    );

    // Only add --worktree-root when it differs from pane_cwd
    if worktree_root != pane_cwd {
        parts.push_str(&format!(
            " --worktree-root '{}'",
            shell_escape(&worktree_root.to_string_lossy()),
        ));
    }

    parts.push_str(&format!(" -- '{}'", shell_escape(command)));

    // Prefix with space to prevent shell history entry.
    Ok(format!(" {}", parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_generates_supervisor_command() {
        let result = wrap_for_docker_sbx(
            " claude -- \"$(cat PROMPT.md)\"",
            Path::new("/Users/test/project"),
            Path::new("/Users/test/project"),
        )
        .unwrap();

        assert_eq!(
            result,
            " workmux sandbox run '/Users/test/project' -- 'claude -- \"$(cat PROMPT.md)\"'"
        );
    }

    #[test]
    fn test_wrap_passes_worktree_root_when_cwd_differs() {
        let result = wrap_for_docker_sbx(
            "echo 'hi'",
            Path::new("/tmp/wt"),
            Path::new("/tmp/wt/sub dir"),
        )
        .unwrap();

        assert_eq!(
            result,
            " workmux sandbox run '/tmp/wt/sub dir' --worktree-root '/tmp/wt' -- 'echo '\\''hi'\\'''"
        );
    }
}
