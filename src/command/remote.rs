//! `workmux remote`: register remotes and check that they are reachable.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::borrow::Cow;
use std::time::Duration;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use super::global_config::{self, PROJECTS, REMOTES};
use crate::config::{RemoteConfig, TransportKind};
use crate::remote::carry::AtRisk;
use crate::remote::transport::{Provisioning, RemoteState, Transport, ensure_running};
use std::sync::Arc;
use crate::remote::channels::{Reach, RemoteStatus};
use crate::remote::pane::{RemotePaneId, SessionHolder};
use crate::remote::registry;

/// How long a status call may take: opening a channel can wait on a slow
/// transport.
const STATUS_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Register a remote in your global config
    Add {
        /// Name projects use to refer to the remote
        name: String,
        /// How to reach the remote
        #[arg(long, value_enum)]
        transport: TransportKind,
        /// Where to reach it: an SSH host (as in your SSH config) or a sandbox name
        #[arg(long)]
        target: String,
        /// Disable the built-in shims (sound playback, clipboard read) for this remote
        #[arg(long)]
        no_builtin_shims: bool,
        /// Allow the remote to run this command on the workstation (repeatable)
        #[arg(long = "host-command", value_name = "COMMAND")]
        host_commands: Vec<String>,
    },
    /// List remotes and whether each is reachable
    #[command(visible_alias = "ls")]
    List,
    /// Show whether a remote is reachable, with its workmux and protocol versions
    Status {
        /// Remote to check (default: all)
        name: Option<String>,
    },
    /// Start a remote's stopped sandbox (creating it if it does not exist yet)
    Start { name: String },
    /// Stop a remote's sandbox, keeping its workspace
    Stop { name: String },
    /// Delete and recreate an OpenShell project's sandbox, carrying its committed work across
    Recreate {
        name: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove a remote from your global config; for an OpenShell project, delete its sandbox and the project
    Rm {
        name: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Show a remote pane in this terminal, reattaching after drops (run by workstation panes)
    Attach {
        /// Remote the pane runs on
        remote: String,
        /// Remote pane, as <project>/<worktree>/<slot>
        pane: RemotePaneId,
    },
    /// Serve the control channel on stdin and stdout (run by transports)
    #[command(hide = true)]
    Serve,
    /// Attach this terminal to a remote pane on this machine (run through transports)
    #[command(hide = true)]
    PaneAttach { pane: RemotePaneId },
    /// Print whether a remote pane on this machine is running or ended
    #[command(hide = true)]
    PaneState { pane: RemotePaneId },
    /// Carry a project's committed work across a recreate (run through transports)
    #[command(hide = true)]
    Carry {
        /// at-risk, manifest, bundle or restore
        action: String,
        /// The project's clone
        path: String,
        /// For restore: the manifest, as JSON
        manifest: Option<String>,
    },
    /// Forward one multiplexer request from a remote shell to the workstation (run by deferred scripts)
    #[command(hide = true)]
    Forward {
        /// The request, as JSON
        request: String,
    },
}

pub fn run(args: RemoteArgs) -> Result<()> {
    match args.command {
        RemoteCommand::Add {
            name,
            transport,
            target,
            no_builtin_shims,
            host_commands,
        } => add(
            &name,
            RemoteConfig {
                transport,
                target,
                builtin_shims: no_builtin_shims.then_some(false),
                host_commands,
                openshell: None,
            },
        ),
        RemoteCommand::List => list(),
        RemoteCommand::Status { name } => {
            if !status(name.as_deref())? {
                std::process::exit(1);
            }
            Ok(())
        }
        RemoteCommand::Start { name } => start(&name),
        RemoteCommand::Stop { name } => stop(&name),
        RemoteCommand::Recreate { name, yes } => recreate(&name, yes),
        RemoteCommand::Rm { name, yes } => rm(&name, yes),
        RemoteCommand::Attach { remote, pane } => crate::remote::attach::run(&remote, &pane),
        RemoteCommand::Serve => crate::remote::serve::run(),
        RemoteCommand::PaneAttach { pane } => {
            use std::os::unix::process::CommandExt;
            let error = SessionHolder::for_this_remote()?
                .attach_command(&pane)
                .exec();
            Err(error).context("failed to run the session holder dtach")
        }
        RemoteCommand::PaneState { pane } => {
            println!(
                "{}",
                SessionHolder::for_this_remote()?.state(&pane)?.as_str()
            );
            Ok(())
        }
        RemoteCommand::Carry {
            action,
            path,
            manifest,
        } => crate::remote::carry::serve(&action, &path, manifest.as_deref()),
        RemoteCommand::Forward { request } => {
            let mux = crate::remote::forwarding::ForwardingMux::from_env()
                .context("remote forward runs only in a remote shell")?;
            mux.forward(serde_json::from_str(&request).context("unreadable request")?)
        }
    }
}

fn add(name: &str, remote: RemoteConfig) -> Result<()> {
    registry::validate_name(name)?;
    let (path, content) = global_config::read()?;
    if global_config::parse(&content)?
        .projects
        .get(name)
        .is_some_and(|project| project.openshell.is_some())
    {
        bail!("project {name} is in an OpenShell sandbox, which is already its remote {name}");
    }
    let updated = global_config::add_entry(&content, &REMOTES, name, &remote)?;
    global_config::write(&path, &updated)?;
    println!(
        "Added remote {name} ({} {}) to {}",
        remote.transport,
        remote.target,
        path.display()
    );
    Ok(())
}

/// A registered remote whose transport manages its lifecycle.
fn managed(name: &str) -> Result<(RemoteConfig, Arc<dyn Transport>)> {
    let remote = registry::remotes()?
        .remove(name)
        .ok_or_else(|| anyhow!("no remote named {name}"))?;
    let transport = registry::transport(name, &remote)?;
    if transport.provisioning().is_none() {
        bail!(
            "remote {name} is reached over {}; workmux does not start or stop it",
            remote.transport
        );
    }
    Ok((remote, transport))
}

fn lifecycle(transport: &dyn Transport) -> &dyn Provisioning {
    transport.provisioning().expect("a managed remote")
}

fn print_notice(notice: &str) {
    eprintln!("{notice}");
}

fn start(name: &str) -> Result<()> {
    let (_, transport) = managed(name)?;
    ensure_running(name, transport.as_ref(), true, &mut print_notice)?;
    // Reconnect now rather than at the next backoff retry.
    let statuses = fetch_statuses(Some(name))?;
    for status in statuses {
        println!("{name}: {}", summary(&status.reach));
    }
    Ok(())
}

fn stop(name: &str) -> Result<()> {
    let (_, transport) = managed(name)?;
    let provisioning = lifecycle(transport.as_ref());
    match provisioning.status()? {
        RemoteState::Missing => bail!("remote {name} has no sandbox yet"),
        RemoteState::Stopped => println!("Remote {name} is already stopped"),
        RemoteState::Running => {
            eprintln!("Stopping remote {name}…");
            provisioning.stop()?;
            println!(
                "Stopped remote {name}; its agents show as stale until `workmux remote start {name}`"
            );
        }
    }
    Ok(())
}

/// The OpenShell project whose implicit remote `name` is.
fn openshell_project(name: &str) -> Result<Option<crate::remote::router::RemoteProject>> {
    let config = crate::config::Config::load_global_only()?;
    let Some(origin) = config.remotes.get(name).and_then(|r| r.openshell.clone()) else {
        return Ok(None);
    };
    crate::remote::router::remote_project(&config, &origin.project).map(Some)
}

/// Ask `prompt`, unless `yes`.
fn confirmed(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    crate::ui::confirm::confirm(prompt, crate::ui::confirm::ConfirmDefault::No)
}

/// The confirmation for deleting `sandbox`, naming the work only it holds.
fn delete_prompt(name: &str, sandbox: &str, risks: Option<&[AtRisk]>) -> String {
    let mut prompt = format!(
        "Delete sandbox {sandbox} of project {name}? Its clone and worktrees are lost."
    );
    match risks {
        None => prompt.push_str("
  Its worktrees could not be checked for unpushed work."),
        Some([]) => {}
        Some(risks) => {
            prompt.push_str("
  These have work that exists only in the sandbox:");
            for risk in risks {
                prompt.push_str(&format!("
    {}", risk.describe()));
            }
        }
    }
    prompt.push_str("
 ");
    prompt
}

/// Delete an OpenShell project's sandbox, then unregister the project.
fn rm_openshell(project: &crate::remote::router::RemoteProject, yes: bool) -> Result<()> {
    let name = &project.site.remote;
    let transport = registry::transport(name, &project.remote)?;
    let provisioning = lifecycle(transport.as_ref());
    if provisioning.status()? != RemoteState::Missing {
        if provisioning.status()? == RemoteState::Stopped {
            eprintln!("Starting remote {name} to check its worktrees for work that exists only there…");
            provisioning.start()?;
        }
        let risks = match crate::remote::carry::remote_at_risk(transport.as_ref(), &project.site.path)
        {
            Ok(risks) => Some(risks),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "checking worktrees before delete");
                None
            }
        };
        if !confirmed(
            &delete_prompt(&project.site.name, &project.remote.target, risks.as_deref()),
            yes,
        )? {
            println!("Kept remote {name}");
            return Ok(());
        }
        eprintln!("Deleting sandbox {}…", project.remote.target);
        provisioning.delete()?;
    }
    let (path, content) = global_config::read()?;
    let updated = global_config::remove_entry(&content, &PROJECTS, &project.site.name)?;
    global_config::write(&path, &updated)?;
    println!(
        "Deleted sandbox {} and removed project {} from {}",
        project.remote.target,
        project.site.name,
        path.display()
    );
    Ok(())
}

fn recreate(name: &str, yes: bool) -> Result<()> {
    let project = openshell_project(name)?.ok_or_else(|| {
        anyhow!("remote {name} is not an OpenShell project's sandbox; only those are recreated")
    })?;
    let transport = registry::transport(name, &project.remote)?;
    let provisioning = lifecycle(transport.as_ref());
    if provisioning.status()? == RemoteState::Missing {
        return ensure_running(name, transport.as_ref(), true, &mut print_notice).map(|_| ());
    }
    ensure_running(name, transport.as_ref(), true, &mut print_notice)?;
    if !confirmed(
        &format!(
            "Recreate sandbox {} of project {}? Agents running in it are ended and their remote panes lost; committed work on every branch is carried across.
 ",
            project.remote.target, project.site.name
        ),
        yes,
    )? {
        println!("Kept remote {name} as it is");
        return Ok(());
    }
    let keep = crate::xdg::state_dir()?
        .join("recreate")
        .join(&project.remote.target);
    crate::remote::carry::recreate(
        transport.as_ref(),
        &project.site.path,
        &keep,
        &mut || {
            eprintln!("Deleting sandbox {}…", project.remote.target);
            provisioning.delete()?;
            eprintln!("Creating sandbox {}…", project.remote.target);
            provisioning.create()
        },
        &mut print_notice,
    )
    .with_context(|| format!("recreating remote {name}"))?;
    let hooks = transport.exec_captured(&["workmux", "setup", "--hooks", "--yes"].map(String::from))?;
    if !hooks.status.success() {
        eprintln!(
            "warning: installing agent hooks on {name} failed ({}); run `workmux setup --hooks` there",
            hooks.status
        );
    }
    println!(
        "Recreated remote {name}; its branches and worktrees are back. Reopen worktrees with `workmux open`"
    );
    Ok(())
}

/// Show an OpenShell remote's effective policy, read from the sandbox.
fn print_policy(name: &str, remote: &RemoteConfig) {
    let sandbox = crate::remote::openshell::OpenShell::new(&remote.target, remote.openshell.clone());
    match sandbox.policy_report() {
        Ok(None) => println!("  sandbox:  not created yet; the first command that needs it creates it"),
        Ok(Some((policy, warnings))) => {
            let mut lines = policy.iter();
            println!("  policy:   {}", lines.next().map(String::as_str).unwrap_or_default());
            for line in lines {
                println!("            {line}");
            }
            for warning in warnings {
                println!("  warning:  {warning}");
            }
        }
        Err(error) => println!("  policy:   unknown ({error:#}) for {name}"),
    }
}

fn rm(name: &str, yes: bool) -> Result<()> {
    if let Some(project) = openshell_project(name)? {
        return rm_openshell(&project, yes);
    }
    let (path, content) = global_config::read()?;
    let users: Vec<String> = global_config::parse(&content)?
        .projects
        .into_iter()
        .filter(|(_, project)| project.remote.as_deref() == Some(name))
        .map(|(project, _)| project)
        .collect();
    if !users.is_empty() {
        bail!("remote {name} is used by project(s) {}", users.join(", "));
    }
    let updated = global_config::remove_entry(&content, &REMOTES, name)?;
    global_config::write(&path, &updated)?;
    println!("Removed remote {name} from {}", path.display());
    Ok(())
}

/// Fetch statuses through the workstation daemon, which holds the control
/// channels, or directly when no daemon can run.
fn fetch_statuses(name: Option<&str>) -> Result<Vec<RemoteStatus>> {
    let remotes = registry::remotes()?;
    if let Some(name) = name
        && !remotes.contains_key(name)
    {
        bail!("no remote named {name}");
    }
    if remotes.is_empty() {
        return Ok(Vec::new());
    }
    match crate::remote::router::daemon_socket() {
        Some(socket) => {
            let result = crate::workstation_daemon::call_with_timeout(
                &socket,
                "remote.status",
                serde_json::json!({ "name": name }),
                STATUS_TIMEOUT,
            )?;
            serde_json::from_value(result).context("workstation daemon sent unreadable statuses")
        }
        None => crate::remote::statuses(crate::remote::channels(), &remotes, name),
    }
}

/// One-line summary of a remote's reachability.
fn summary(reach: &Reach) -> String {
    match reach {
        Reach::Reachable { .. } => "reachable".to_string(),
        Reach::Stopped => "stopped".to_string(),
        Reach::Unreachable { error } => {
            format!("unreachable: {}", error.lines().next().unwrap_or_default())
        }
    }
}

struct Row {
    name: String,
    transport: String,
    target: String,
    status: String,
}

impl Tabled for Row {
    const LENGTH: usize = 4;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
            Cow::Borrowed(&self.name),
            Cow::Borrowed(&self.transport),
            Cow::Borrowed(&self.target),
            Cow::Borrowed(&self.status),
        ]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            Cow::Borrowed("NAME"),
            Cow::Borrowed("TRANSPORT"),
            Cow::Borrowed("TARGET"),
            Cow::Borrowed("STATUS"),
        ]
    }
}

fn list() -> Result<()> {
    let statuses = fetch_statuses(None)?;
    if statuses.is_empty() {
        println!(
            "No remotes. Add one with: workmux remote add <name> --transport ssh --target <host>"
        );
        return Ok(());
    }
    let rows: Vec<Row> = statuses
        .iter()
        .map(|status| Row {
            name: status.name.clone(),
            transport: status.transport.to_string(),
            target: status.target.clone(),
            status: summary(&status.reach),
        })
        .collect();
    let mut table = Table::new(rows);
    table
        .with(Style::blank())
        .modify(Columns::new(..), Padding::new(0, 1, 0, 0));
    println!("{table}");
    for status in &statuses {
        if let Reach::Reachable {
            warning: Some(warning),
            ..
        } = &status.reach
        {
            eprintln!("warning: {warning}");
        }
    }
    Ok(())
}

/// Print each remote's status. Returns whether none was unreachable.
fn status(name: Option<&str>) -> Result<bool> {
    let statuses = fetch_statuses(name)?;
    if statuses.is_empty() {
        println!(
            "No remotes. Add one with: workmux remote add <name> --transport ssh --target <host>"
        );
        return Ok(true);
    }
    let remotes = registry::remotes()?;
    let mut all_reachable = true;
    for (i, status) in statuses.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{} ({} {})", status.name, status.transport, status.target);
        match &status.reach {
            Reach::Reachable {
                workmux,
                protocol,
                warning,
                warnings,
            } => {
                println!("  status:   reachable");
                println!("  workmux:  {workmux}");
                println!("  protocol: {protocol}");
                for warning in warning.iter().chain(warnings) {
                    println!("  warning:  {warning}");
                }
            }
            Reach::Stopped => println!("  status:   stopped"),
            Reach::Unreachable { error } => {
                all_reachable = false;
                println!("  status:   unreachable");
                let mut lines = error.lines();
                println!("  error:    {}", lines.next().unwrap_or_default());
                for line in lines {
                    println!("            {line}");
                }
            }
        }
        if let Some(remote) = remotes.get(&status.name)
            && remote.transport == TransportKind::Openshell
        {
            print_policy(&status.name, remote);
        }
    }
    Ok(all_reachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn ssh(target: &str) -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Ssh,
            target: target.to_string(),
            builtin_shims: None,
            host_commands: vec![],
            openshell: None,
        }
    }

    fn parse(content: &str) -> Config {
        serde_yaml::from_str(content).unwrap()
    }

    #[test]
    fn deleting_a_sandbox_names_the_work_that_exists_only_there() {
        let risks = [
            AtRisk {
                path: Some("/sandbox/api__worktrees/feature".into()),
                branch: Some("feature".to_string()),
                uncommitted: true,
                unpushed: 2,
            },
            AtRisk {
                path: None,
                branch: Some("spike".to_string()),
                uncommitted: false,
                unpushed: 1,
            },
        ];

        assert_eq!(
            delete_prompt("api", "api", Some(&risks)),
            "Delete sandbox api of project api? Its clone and worktrees are lost.\n  \
             These have work that exists only in the sandbox:\n    \
             /sandbox/api__worktrees/feature (branch feature): uncommitted changes, 2 commits not on any remote\n    \
             branch spike: 1 commit not on any remote\n "
        );
        assert!(delete_prompt("api", "api", None).contains("could not be checked"));
    }

    #[test]
    fn adding_to_a_config_without_remotes_appends_the_section() {
        let content = "# my settings\nnerdfont: true\n";

        let updated =
            global_config::add_entry(content, &REMOTES, "devbox", &ssh("me@devbox")).unwrap();

        assert!(updated.starts_with(content), "{updated}");
        assert_eq!(parse(&updated).remotes["devbox"], ssh("me@devbox"));
        assert_eq!(parse(&updated).nerdfont, Some(true));
    }

    #[test]
    fn adding_to_an_empty_config_creates_it() {
        let updated = global_config::add_entry("", &REMOTES, "devbox", &ssh("devbox")).unwrap();

        assert_eq!(parse(&updated).remotes["devbox"], ssh("devbox"));
    }

    #[test]
    fn adding_joins_an_existing_remotes_section_and_keeps_comments() {
        let content = "\
remotes:
    # the big box
    devbox:
        transport: ssh
        target: devbox
nerdfont: true
";

        let updated = global_config::add_entry(
            content,
            &REMOTES,
            "gpu",
            &RemoteConfig {
                builtin_shims: Some(false),
                host_commands: vec!["just".to_string()],
                openshell: None,
                ..ssh("gpu-box")
            },
        )
        .unwrap();

        let config = parse(&updated);
        assert_eq!(config.remotes["devbox"], ssh("devbox"));
        assert!(!config.remotes["gpu"].builtin_shims());
        assert_eq!(config.remotes["gpu"].host_commands, vec!["just"]);
        assert_eq!(config.nerdfont, Some(true));
        assert!(updated.contains("# the big box"));
    }

    #[test]
    fn adding_a_remote_that_exists_is_refused() {
        let content = "remotes:\n  devbox:\n    transport: ssh\n    target: devbox\n";

        let error =
            global_config::add_entry(content, &REMOTES, "devbox", &ssh("other")).unwrap_err();

        assert_eq!(error.to_string(), "remote devbox already exists");
    }

    #[test]
    fn removing_a_remote_keeps_the_others_and_the_rest_of_the_file() {
        let content = "\
# top comment
remotes:
  devbox:
    transport: ssh
    target: devbox

  gpu:
    transport: ssh
    target: gpu-box
    host_commands:
      - just
nerdfont: true
";

        let updated = global_config::remove_entry(content, &REMOTES, "devbox").unwrap();

        let config = parse(&updated);
        assert!(!config.remotes.contains_key("devbox"));
        assert_eq!(config.remotes["gpu"].host_commands, vec!["just"]);
        assert_eq!(config.nerdfont, Some(true));
        assert!(updated.starts_with("# top comment\n"));
    }

    #[test]
    fn removing_the_last_remote_removes_the_section() {
        let content =
            "remotes:\n  devbox:\n    transport: ssh\n    target: devbox\nnerdfont: true\n";

        let updated = global_config::remove_entry(content, &REMOTES, "devbox").unwrap();

        assert_eq!(updated, "nerdfont: true\n");
    }

    #[test]
    fn removing_an_unknown_remote_is_an_error() {
        let error =
            global_config::remove_entry("nerdfont: true\n", &REMOTES, "devbox").unwrap_err();

        assert_eq!(error.to_string(), "no remote named devbox");
    }
}
