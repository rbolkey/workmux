//! `workmux project`: register projects that live on a remote.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use super::global_config::{self, PROJECTS};
use crate::config::ProjectEntry;
use crate::remote::registry;

#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Register a project on a remote in your global config
    Add {
        /// Name commands use to refer to the project
        name: String,
        /// Remote the project lives on
        #[arg(long)]
        remote: String,
        /// Git URL the remote clones the project from on first use
        #[arg(long, value_name = "URL")]
        repo: String,
        /// Path of the clone on the remote (default: ~/<name>)
        #[arg(long)]
        path: Option<String>,
        /// Workstation session for the project's windows (default: the project name)
        #[arg(long)]
        session: Option<String>,
    },
}

pub fn run(args: ProjectArgs) -> Result<()> {
    match args.command {
        ProjectCommand::Add {
            name,
            remote,
            repo,
            path,
            session,
        } => add(
            &name,
            ProjectEntry {
                path: Some(path.unwrap_or_else(|| format!("~/{name}"))),
                remote: Some(remote),
                git_url: Some(repo),
                session,
            },
        ),
    }
}

fn add(name: &str, project: ProjectEntry) -> Result<()> {
    let (path, content) = global_config::read()?;
    let updated = add_project_to(&content, name, &project)?;
    global_config::write(&path, &updated)?;
    println!(
        "Added project {name} on remote {} ({}) to {}",
        project.remote.as_deref().unwrap_or_default(),
        project.path.as_deref().unwrap_or_default(),
        path.display()
    );
    Ok(())
}

/// Add project `name` to a global config file's text. Its remote must be
/// registered, and no other project may have its name.
fn add_project_to(content: &str, name: &str, project: &ProjectEntry) -> Result<String> {
    registry::validate_project_name(name)?;
    let config = global_config::parse(content)?;
    if let Some(remote) = &project.remote
        && !config.remotes.contains_key(remote)
    {
        bail!(
            "no remote named {remote}; add it with: workmux remote add {remote} --transport ssh --target <host>"
        );
    }
    global_config::add_entry(content, &PROJECTS, name, project)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_REMOTE: &str = "\
# my remotes
remotes:
  devbox:
    transport: ssh
    target: devbox
";

    fn api() -> ProjectEntry {
        ProjectEntry {
            remote: Some("devbox".to_string()),
            git_url: Some("git@github.com:me/api.git".to_string()),
            path: Some("~/code/api".to_string()),
            session: None,
        }
    }

    #[test]
    fn adding_a_project_writes_its_global_config_entry() {
        let updated = add_project_to(WITH_REMOTE, "api", &api()).unwrap();

        let config = global_config::parse(&updated).unwrap();
        assert_eq!(config.projects["api"], api());
        assert_eq!(config.remotes["devbox"].target, "devbox");
        assert!(updated.starts_with(WITH_REMOTE), "{updated}");
    }

    #[test]
    fn project_names_are_unique_across_global_config() {
        let once = add_project_to(WITH_REMOTE, "api", &api()).unwrap();

        let error = add_project_to(&once, "api", &api()).unwrap_err();

        assert_eq!(error.to_string(), "project api already exists");
    }

    #[test]
    fn a_project_on_an_unregistered_remote_is_refused() {
        let error = add_project_to("", "api", &api()).unwrap_err();

        assert!(
            error.to_string().starts_with("no remote named devbox"),
            "{error}"
        );
    }
}
