use crate::git;
use crate::remote::{router, session::ServedProject};
use anyhow::{Result, anyhow};

pub fn run(name: &str, json: bool) -> Result<()> {
    if let Some(project) = router::resolve()? {
        let mut argv = vec!["workmux", "path", name];
        if json {
            argv.push("--json");
        }
        // Looking a worktree up never creates or starts the sandbox.
        if let Some(why) = router::not_running(&project)? {
            return Err(anyhow!(why));
        }
        return router::exit_with(router::run_running(&project, &argv)?);
    }

    if !git::is_git_repo()? {
        return Err(anyhow!(git::NOT_IN_A_REPOSITORY));
    }
    // Smart resolution: try handle first, then branch name
    let (path, _branch) = git::find_worktree(name).map_err(|_| {
        anyhow!(
            "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
            name
        )
    })?;
    if json {
        // Run for a remote project, remote workmux names its remote.
        let remote = ServedProject::current().map(|served| served.remote);
        println!(
            "{}",
            serde_json::json!({ "remote": remote, "path": path.display().to_string() })
        );
    } else {
        println!("{}", path.display());
    }
    Ok(())
}
