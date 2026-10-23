use crate::git;
use crate::remote::{router, session::ServedProject};
use anyhow::{Result, anyhow};

pub fn run(name: &str, project: Option<&str>, json: bool) -> Result<()> {
    if let Some(project) = router::resolve(project)? {
        let mut argv = vec!["workmux", "path", name];
        if json {
            argv.push("--json");
        }
        return router::exit_with(router::run(&project, &argv)?);
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
