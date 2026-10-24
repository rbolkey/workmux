//! `workmux project`: register and unregister projects that live on a remote.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use super::global_config::{self, PROJECTS};
use crate::config::{OpenshellProject, ProjectEntry, ProjectPlacement};
use crate::remote::openshell::github::GH_LOGIN;
use crate::remote::registry;

#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Register a project in your global config
    #[command(subcommand)]
    Add(AddCommand),
    /// Unregister a project from your global config, closing its workstation windows; the remote and its clone are kept
    Rm {
        name: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

/// `project add`, one subcommand per kind of project.
#[derive(Debug, Subcommand)]
pub enum AddCommand {
    /// Register a project on a remote added with `workmux remote add`
    Remote(RemoteAddArgs),
    /// Register a project whose remote is its own OpenShell sandbox, created on first use
    Openshell(Box<OpenshellAddArgs>),
}

const PROJECT_OPTIONS: &str = "Project options";

/// What every `project add` takes.
#[derive(Debug, Args)]
pub struct ProjectAddArgs {
    /// Name commands use to refer to the project
    pub name: String,
    /// Git URL the remote clones the project from on first use
    #[arg(long, value_name = "URL", help_heading = PROJECT_OPTIONS)]
    pub repo: String,
    /// Path of the clone on the remote (default: ~/<name>)
    #[arg(long, help_heading = PROJECT_OPTIONS)]
    pub path: Option<String>,
    /// Workstation session for the project's windows (default: the project name)
    #[arg(long, help_heading = PROJECT_OPTIONS)]
    pub session: Option<String>,
    /// Where the project's windows go in window mode
    #[arg(long, value_enum, help_heading = PROJECT_OPTIONS)]
    pub placement: Option<ProjectPlacement>,
}

#[derive(Debug, Args)]
pub struct RemoteAddArgs {
    #[command(flatten)]
    pub project: ProjectAddArgs,
    /// Remote the project lives on
    #[arg(long)]
    pub remote: String,
}

#[derive(Debug, Args)]
pub struct OpenshellAddArgs {
    #[command(flatten)]
    pub project: ProjectAddArgs,
    #[command(flatten)]
    pub openshell: OpenshellOptions,
}

#[derive(Debug, Args)]
#[command(next_help_heading = "OpenShell options")]
pub struct OpenshellOptions {
    /// Image the sandbox runs; it must contain git
    #[arg(long)]
    pub image: Option<String>,
    /// Policy file merged on top of workmux's base policy
    #[arg(long, value_name = "FILE")]
    pub policy: Option<std::path::PathBuf>,
    /// OpenShell provider to attach to the sandbox, for credentials (repeatable).
    /// `gh-login` is special: workmux creates it from your workstation's `gh` login
    /// and keeps it current, and it gives the sandbox everything that login can do on
    /// GitHub, every repo it reaches, push and pull requests included. At most one
    /// attached provider may give GitHub credentials
    #[arg(long = "provider", value_name = "PROVIDER")]
    pub providers: Vec<String>,
    /// Agent the sandbox's image installs, such as claude or codex; the first is its
    /// default agent (repeatable; default: your default agent's type)
    #[arg(long = "agent", value_name = "AGENT")]
    pub agents: Vec<String>,
    /// Sandbox name (default: derived from the project name)
    #[arg(long)]
    pub sandbox: Option<String>,
}

pub fn run(args: ProjectArgs) -> Result<()> {
    match args.command {
        ProjectCommand::Add(command) => {
            let (name, entry) = project_entry(command)?;
            add(&name, entry)
        }
        ProjectCommand::Rm { name, yes } => rm(&name, yes),
    }
}

/// The project's name and global config entry for `project add`'s
/// arguments. An OpenShell project's SSH `--repo` on github.com is stored
/// as its HTTPS form, with the original beside it; on another host, it is
/// refused.
fn project_entry(command: AddCommand) -> Result<(String, ProjectEntry)> {
    let (project, remote, openshell) = match command {
        AddCommand::Remote(args) => (args.project, Some(args.remote), None),
        AddCommand::Openshell(args) => {
            let OpenshellAddArgs { project, openshell } = *args;
            (project, None, Some(openshell))
        }
    };
    let ProjectAddArgs {
        name,
        repo,
        path,
        session,
        placement,
    } = project;
    let (git_url, ssh_git_url) = match openshell
        .is_some()
        .then(|| crate::remote::openshell::repo::https_for_ssh(&repo))
        .transpose()?
        .flatten()
    {
        Some(https) => (https, Some(repo)),
        None => (repo, None),
    };
    let openshell = openshell
        .map(|options| -> Result<OpenshellProject> {
            Ok(OpenshellProject {
                sandbox: options.sandbox,
                image: options.image,
                policy: options.policy.map(absolute_policy).transpose()?,
                providers: options.providers,
                agents: options.agents,
            })
        })
        .transpose()?;
    let entry = ProjectEntry {
        path: Some(path.unwrap_or_else(|| format!("~/{name}"))),
        remote,
        git_url: Some(git_url),
        ssh_git_url,
        session,
        placement,
        openshell,
    };
    Ok((name, entry))
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
    if project.openshell.is_some() {
        let config = global_config::parse(&updated)?.with_implicit_remotes();
        let remote = &config.remotes[name];
        let sandbox =
            crate::remote::openshell::OpenShell::new(&remote.target, remote.openshell.clone());
        sandbox.check_github_credentials()?;
        global_config::write(&path, &updated)?;
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
        let github = sandbox
            .github_providers()
            .map_err(|error| format!("{error:#}"));
        if remote
            .openshell
            .as_ref()
            .is_some_and(|origin| origin.settings.providers.iter().any(|p| p == GH_LOGIN))
        {
            println!(
                "Note: provider {GH_LOGIN} gives the sandbox your gh login's GitHub access, which usually \
                 reaches every repo you can (push and pull requests included); workmux keeps its token in \
                 that provider on the gateway and updates it whenever it creates or attaches to the sandbox"
            );
        }
        if project.ssh_git_url.is_some() {
            let notice = crate::remote::openshell::repo::ssh_notice(
                github.as_deref().map_err(String::as_str),
            );
            println!("Note: {notice}");
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

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: ProjectCommand,
    }

    fn parse(args: &str) -> Result<ProjectCommand, clap::Error> {
        use clap::Parser;
        Cli::try_parse_from(std::iter::once("project").chain(args.split_whitespace()))
            .map(|cli| cli.command)
    }

    fn add_command(args: &str) -> AddCommand {
        match parse(&format!("add {args}")).unwrap() {
            ProjectCommand::Add(command) => command,
            other => panic!("{other:?}"),
        }
    }

    fn help(args: &str) -> String {
        parse(&format!("add {args} --help"))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn project_add_remote_takes_a_registered_remote_and_the_common_options() {
        let (name, entry) = project_entry(add_command(
            "remote api --remote devbox --repo git@github.com:me/api.git --path code/api \
             --session work --placement current-session",
        ))
        .unwrap();

        assert_eq!(name, "api");
        assert_eq!(
            entry,
            ProjectEntry {
                remote: Some("devbox".to_string()),
                git_url: Some("git@github.com:me/api.git".to_string()),
                path: Some("code/api".to_string()),
                session: Some("work".to_string()),
                placement: Some(ProjectPlacement::CurrentSession),
                ..ProjectEntry::default()
            }
        );
    }

    #[test]
    fn project_add_openshell_takes_the_openshell_options() {
        let (name, entry) = project_entry(add_command(
            "openshell api --repo https://github.com/me/api.git --image ghcr.io/me/api-dev:1 \
             --provider work --provider anthropic --agent claude --agent codex --sandbox api-box",
        ))
        .unwrap();

        assert_eq!(name, "api");
        assert_eq!(
            entry,
            ProjectEntry {
                git_url: Some("https://github.com/me/api.git".to_string()),
                path: Some("~/api".to_string()),
                openshell: Some(OpenshellProject {
                    sandbox: Some("api-box".to_string()),
                    image: Some("ghcr.io/me/api-dev:1".to_string()),
                    providers: vec!["work".to_string(), "anthropic".to_string()],
                    agents: vec!["claude".to_string(), "codex".to_string()],
                    ..OpenshellProject::default()
                }),
                ..ProjectEntry::default()
            }
        );
    }

    #[test]
    fn each_kind_of_project_takes_only_its_own_options() {
        for args in [
            "add remote api --remote devbox --repo u --image i",
            "add remote api --remote devbox --repo u --provider p",
            "add remote api --repo u",
            "add openshell api --remote devbox --repo u",
            "add openshell api",
            "add api --remote devbox --repo u",
            "add api --openshell --repo u",
        ] {
            assert!(parse(args).is_err(), "{args}");
        }
    }

    #[test]
    fn project_add_help_groups_options_under_headings() {
        let openshell = help("openshell");
        let project = openshell.find("Project options:").expect(&openshell);
        let sandbox = openshell.find("OpenShell options:").expect(&openshell);
        assert!(
            openshell[project..sandbox].contains("--repo <URL>"),
            "{openshell}"
        );
        for option in ["--image", "--policy", "--provider", "--agent", "--sandbox"] {
            assert!(
                openshell[sandbox..].contains(option),
                "{option}: {openshell}"
            );
        }

        let remote = help("remote");
        assert!(remote.contains("Project options:"), "{remote}");
        assert!(remote.contains("--remote <REMOTE>"), "{remote}");
        assert!(!remote.contains("OpenShell"), "{remote}");
    }

    #[test]
    fn an_openshell_projects_ssh_github_repo_is_stored_as_https_with_its_original() {
        let entry = project_entry(add_command(
            "openshell api --repo git@github.com:me/api.git",
        ))
        .unwrap()
        .1;

        assert_eq!(
            entry.git_url.as_deref(),
            Some("https://github.com/me/api.git")
        );
        assert_eq!(
            entry.ssh_git_url.as_deref(),
            Some("git@github.com:me/api.git")
        );
        // An SSH remote speaks SSH.
        let entry = project_entry(add_command(
            "remote api --remote devbox --repo git@github.com:me/api.git",
        ))
        .unwrap()
        .1;
        assert_eq!(entry.git_url.as_deref(), Some("git@github.com:me/api.git"));
        assert_eq!(entry.ssh_git_url, None);
    }

    #[test]
    fn an_openshell_projects_ssh_repo_on_another_host_is_refused() {
        let error = project_entry(add_command(
            "openshell api --repo git@gitlab.com:me/api.git",
        ))
        .unwrap_err();

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
