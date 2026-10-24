//! `workmux project`: register and unregister projects that live on a remote.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use super::global_config::{self, PROJECTS};
use crate::config::{OpenshellProject, ProjectEntry, ProjectPlacement};
use crate::remote::registry;

#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Register a project on a remote in your global config
    Add(Box<AddArgs>),
    /// Unregister a project from your global config, closing its workstation windows; the remote and its clone are kept
    Rm {
        name: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Name commands use to refer to the project
    pub name: String,
    /// Remote the project lives on
    #[arg(
        long,
        required_unless_present = "openshell",
        conflicts_with = "openshell"
    )]
    pub remote: Option<String>,
    /// Give the project its own OpenShell sandbox as its remote, created on first use
    #[arg(long)]
    pub openshell: bool,
    /// Image the sandbox runs; it must contain git (OpenShell only)
    #[arg(long, requires = "openshell")]
    pub image: Option<String>,
    /// Policy file merged on top of workmux's base policy (OpenShell only)
    #[arg(long, value_name = "FILE", requires = "openshell")]
    pub policy: Option<std::path::PathBuf>,
    /// OpenShell provider to attach to the sandbox, for credentials (repeatable)
    #[arg(long = "provider", value_name = "PROVIDER", requires = "openshell")]
    pub providers: Vec<String>,
    /// Agent the sandbox's image installs, such as claude or codex; the first is its
    /// default agent (repeatable; default: your default agent's type; OpenShell only)
    #[arg(long = "agent", value_name = "AGENT", requires = "openshell")]
    pub agents: Vec<String>,
    /// Sandbox name (default: derived from the project name; OpenShell only)
    #[arg(long, requires = "openshell")]
    pub sandbox: Option<String>,
    /// Git URL the remote clones the project from on first use
    #[arg(long, value_name = "URL")]
    pub repo: String,
    /// Path of the clone on the remote (default: ~/<name>)
    #[arg(long)]
    pub path: Option<String>,
    /// Workstation session for the project's windows (default: the project name)
    #[arg(long)]
    pub session: Option<String>,
    /// Where the project's windows go in window mode
    #[arg(long, value_enum)]
    pub placement: Option<ProjectPlacement>,
}

pub fn run(args: ProjectArgs) -> Result<()> {
    match args.command {
        ProjectCommand::Add(args) => {
            let name = args.name.clone();
            add(&name, project_entry(*args)?)
        }
        ProjectCommand::Rm { name, yes } => rm(&name, yes),
    }
}

/// The global config entry for `project add`'s arguments. An OpenShell
/// project's SSH `--repo` on github.com is stored as its HTTPS form, with
/// the original beside it; on another host, it is refused.
fn project_entry(args: AddArgs) -> Result<ProjectEntry> {
    let AddArgs {
        name,
        remote,
        openshell,
        image,
        policy,
        providers,
        agents,
        sandbox,
        repo,
        path,
        session,
        placement,
    } = args;
    let (git_url, ssh_git_url) = match openshell
        .then(|| crate::remote::openshell::repo::https_for_ssh(&repo))
        .transpose()?
        .flatten()
    {
        Some(https) => (https, Some(repo)),
        None => (repo, None),
    };
    let openshell = if openshell {
        Some(OpenshellProject {
            sandbox,
            image,
            policy: policy.map(absolute_policy).transpose()?,
            providers,
            agents,
        })
    } else {
        None
    };
    Ok(ProjectEntry {
        path: Some(path.unwrap_or_else(|| format!("~/{name}"))),
        remote,
        git_url: Some(git_url),
        ssh_git_url,
        session,
        placement,
        openshell,
    })
}

/// A policy file's absolute path, so the project resolves it from anywhere.
fn absolute_policy(policy: std::path::PathBuf) -> Result<String> {
    let absolute = std::path::absolute(&policy)
        .with_context(|| format!("failed to resolve {}", policy.display()))?;
    if !absolute.is_file() {
        bail!("policy file {} does not exist", absolute.display());
    }
    Ok(absolute.display().to_string())
}

fn add(name: &str, project: ProjectEntry) -> Result<()> {
    let (path, content) = global_config::read()?;
    let updated = add_project_to(&content, name, &project)?;
    global_config::write(&path, &updated)?;
    if project.openshell.is_some() {
        let config = global_config::parse(&updated)?.with_implicit_remotes();
        println!(
            "Added project {name} in OpenShell sandbox {} ({}) to {}; the sandbox is created on first use",
            config.remotes[name].target,
            project.path.as_deref().unwrap_or_default(),
            path.display()
        );
        println!(
            "Note: only a flake devshell (flake.nix) is baked into an OpenShell sandbox's image; \
             a devbox project gets no devshell there, so put its toolchain in the image (--image)"
        );
        let remote = &config.remotes[name];
        let sandbox =
            crate::remote::openshell::OpenShell::new(&remote.target, remote.openshell.clone());
        if project.ssh_git_url.is_some() {
            let github = sandbox
                .github_providers()
                .map_err(|error| format!("{error:#}"));
            println!(
                "Note: {}",
                crate::remote::openshell::repo::ssh_notice(
                    github.as_deref().map_err(String::as_str)
                )
            );
        }
        if let Some(report) = sandbox.agent_report() {
            for note in report.notes {
                println!("Agent {note}");
            }
            for warning in report.warnings {
                eprintln!("warning: {warning}");
            }
        }
        return Ok(());
    }
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
    if project.openshell.is_some() && config.remotes.contains_key(name) {
        bail!(
            "remote {name} already exists; an OpenShell project's sandbox is its remote, named after it"
        );
    }
    global_config::add_entry(content, &PROJECTS, name, project)
}

/// Unregister project `name`, closing its workstation windows. Nothing on
/// its remote is touched.
fn rm(name: &str, yes: bool) -> Result<()> {
    let (path, content) = global_config::read()?;
    let updated = remove_project_from(&content, name)?;
    let entry = &global_config::parse(&content)?.projects[name];
    let remote = entry.remote.as_deref().unwrap_or_default();
    let mut prompt = format!(
        "Remove project {name} from {}? Its workstation windows are closed",
        path.display()
    );
    if !remote.is_empty() {
        prompt.push_str(&format!(
            "; remote {remote} and the clone at {} are kept",
            entry.path.clone().unwrap_or_else(|| format!("~/{name}"))
        ));
    }
    prompt.push('.');
    if !yes && !crate::ui::confirm::confirm(&prompt, crate::ui::confirm::ConfirmDefault::No)? {
        println!("Kept project {name}");
        return Ok(());
    }
    super::remote::close_workstation_windows(remote, name);
    global_config::write(&path, &updated)?;
    println!("Removed project {name} from {}", path.display());
    Ok(())
}

/// Remove project `name` from a global config file's text. An OpenShell
/// project is refused: its sandbox is its own remote, which `remote rm`
/// deletes along with the project.
fn remove_project_from(content: &str, name: &str) -> Result<String> {
    if global_config::parse(content)?
        .projects
        .get(name)
        .is_some_and(|project| project.openshell.is_some())
    {
        bail!(
            "project {name} is an OpenShell project, whose sandbox is its remote; remove both with: workmux remote rm {name}"
        );
    }
    global_config::remove_entry(content, &PROJECTS, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TransportKind;

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
            ssh_git_url: None,
            path: Some("~/code/api".to_string()),
            session: None,
            placement: Some(ProjectPlacement::CurrentSession),
            openshell: None,
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

    fn openshell_api() -> ProjectEntry {
        ProjectEntry {
            remote: None,
            git_url: Some("https://github.com/me/api.git".to_string()),
            path: Some("~/api".to_string()),
            openshell: Some(OpenshellProject {
                image: Some("ghcr.io/me/api-dev:1".to_string()),
                providers: vec!["github".to_string()],
                ..OpenshellProject::default()
            }),
            ..ProjectEntry::default()
        }
    }

    #[test]
    fn an_openshell_project_needs_no_registered_remote() {
        let updated = add_project_to("", "api", &openshell_api()).unwrap();

        let config = global_config::parse(&updated)
            .unwrap()
            .with_implicit_remotes();
        assert_eq!(config.projects["api"].openshell, openshell_api().openshell);
        assert_eq!(config.remotes["api"].transport, TransportKind::Openshell);
        assert!(!updated.contains("remotes:"), "{updated}");
    }

    #[test]
    fn an_openshell_project_may_not_take_a_registered_remotes_name() {
        let content = "remotes:\n  api:\n    transport: ssh\n    target: devbox\n";

        let error = add_project_to(content, "api", &openshell_api()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "remote api already exists; an OpenShell project's sandbox is its remote, named after it"
        );
    }

    #[test]
    fn removing_a_project_keeps_the_rest_of_the_file() {
        let content = format!(
            "{WITH_REMOTE}projects:\n  # the api\n  api:\n    remote: devbox\n    git_url: git@github.com:me/api.git\n  web:\n    remote: devbox\n    git_url: git@github.com:me/web.git\n"
        );

        let updated = remove_project_from(&content, "api").unwrap();

        assert_eq!(
            updated,
            format!(
                "{WITH_REMOTE}projects:\n  # the api\n  web:\n    remote: devbox\n    git_url: git@github.com:me/web.git\n"
            )
        );
    }

    #[test]
    fn removing_an_unknown_project_is_an_error() {
        let error = remove_project_from(WITH_REMOTE, "api").unwrap_err();

        assert_eq!(error.to_string(), "no project named api");
    }

    #[test]
    fn an_openshell_project_is_removed_with_its_sandbox_by_remote_rm() {
        let content = add_project_to("", "api", &openshell_api()).unwrap();

        let error = remove_project_from(&content, "api").unwrap_err();

        assert_eq!(
            error.to_string(),
            "project api is an OpenShell project, whose sandbox is its remote; remove both with: workmux remote rm api"
        );
    }

    fn add_args(repo: &str, openshell: bool) -> AddArgs {
        AddArgs {
            name: "api".to_string(),
            remote: (!openshell).then(|| "devbox".to_string()),
            openshell,
            image: None,
            policy: None,
            providers: Vec::new(),
            agents: Vec::new(),
            sandbox: None,
            repo: repo.to_string(),
            path: None,
            session: None,
            placement: None,
        }
    }

    #[test]
    fn an_openshell_projects_ssh_github_repo_is_stored_as_https_with_its_original() {
        let entry = project_entry(add_args("git@github.com:me/api.git", true)).unwrap();

        assert_eq!(
            entry.git_url.as_deref(),
            Some("https://github.com/me/api.git")
        );
        assert_eq!(
            entry.ssh_git_url.as_deref(),
            Some("git@github.com:me/api.git")
        );
        // An SSH remote speaks SSH.
        let entry = project_entry(add_args("git@github.com:me/api.git", false)).unwrap();
        assert_eq!(entry.git_url.as_deref(), Some("git@github.com:me/api.git"));
        assert_eq!(entry.ssh_git_url, None);
    }

    #[test]
    fn an_openshell_projects_ssh_repo_on_another_host_is_refused() {
        let error = project_entry(add_args("git@gitlab.com:me/api.git", true)).unwrap_err();

        assert!(
            error.to_string().contains(
                "OpenShell sandboxes don't support: they reach git hosts over HTTPS only"
            ),
            "{error}"
        );
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
