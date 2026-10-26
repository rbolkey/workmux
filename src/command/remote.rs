//! `workmux remote`: check that remotes are reachable and manage the
//! lifecycle of OpenShell sandboxes.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::Duration;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::config::{RemoteConfig, TransportKind};
use crate::remote::carry::AtRisk;
use crate::remote::channels::{Reach, RemoteStatus};
use crate::remote::openshell::Listed;
use crate::remote::pane::{RemotePaneId, SessionHolder};
use crate::remote::router::RemoteProject;
use crate::remote::transport::{Provisioning, RemoteState, Transport, ensure_running};
use std::sync::Arc;

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
    /// List remotes and whether each is reachable
    #[command(visible_alias = "ls")]
    List,
    /// Show whether this project's remote is reachable, with its workmux and protocol versions (outside any project: every remote)
    Status,
    /// Start this project's stopped sandbox (creating it if it does not exist yet)
    Start,
    /// Stop this project's sandbox, keeping its workspace
    Stop,
    /// Delete and recreate this project's OpenShell sandbox, carrying its committed work across
    Recreate {
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete this project's OpenShell sandbox and close its workstation windows
    Rm {
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
        RemoteCommand::List => list(),
        RemoteCommand::Status => {
            let all_reachable = match crate::remote::router::resolve()? {
                Some(project) => status(&project_remotes(std::slice::from_ref(&project)), &[])?,
                None => {
                    let remotes = known_remotes(None);
                    status(&remotes, &gateway_only(&remotes))?
                }
            };
            if !all_reachable {
                std::process::exit(1);
            }
            Ok(())
        }
        RemoteCommand::Start => start(&current_project()?),
        RemoteCommand::Stop => stop(&current_project()?),
        RemoteCommand::Recreate { yes } => recreate(&current_project()?, yes),
        RemoteCommand::Rm { yes } => rm(&current_project()?, yes),
        RemoteCommand::Attach { remote, pane } => crate::remote::attach::run(&remote, &pane),
        RemoteCommand::Serve => crate::remote::serve::run(),
        RemoteCommand::PaneAttach { pane } => {
            use std::os::unix::process::CommandExt;
            let error = SessionHolder::for_this_remote()?
                .attach_command(&pane)
                .exec();
            Err(error).context("Failed to run the session holder dtach")
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
                .context("Remote forward runs only in a remote shell")?;
            mux.forward(serde_json::from_str(&request).context("Unreadable request")?)
        }
    }
}

/// The project this command targets: the working directory's repository,
/// or the project window it was run from.
fn current_project() -> Result<RemoteProject> {
    crate::remote::router::resolve()?.ok_or_else(|| {
        anyhow!(
            "Not in a project with a sandbox; run this inside a clone of a repository \
             whose .workmux.yaml sets sandbox.backend: openshell"
        )
    })
}

/// The remotes of `projects`, by name.
fn project_remotes(projects: &[RemoteProject]) -> BTreeMap<String, RemoteConfig> {
    projects
        .iter()
        .map(|project| (project.site.remote.clone(), project.remote_config.clone()))
        .collect()
}

/// The remotes workmux knows of here: the working directory's project's,
/// `here`, and those of projects with open workstation windows.
fn known_remotes(here: Option<RemoteProject>) -> BTreeMap<String, RemoteConfig> {
    let mut projects = crate::remote::router::remote_projects();
    projects.extend(here);
    project_remotes(&projects)
}

/// The sandboxes workmux made on the gateway that none of `known` is:
/// listed, never reached.
fn gateway_only(known: &BTreeMap<String, RemoteConfig>) -> Vec<Listed> {
    match crate::remote::openshell::workmux_sandboxes() {
        Ok(listed) => listed
            .into_iter()
            .filter(|sandbox| {
                !known.values().any(|remote| {
                    remote.transport == TransportKind::Openshell && remote.target == sandbox.name
                })
            })
            .collect(),
        Err(error) => {
            eprintln!("warning: could not list the gateway's sandboxes: {error:#}");
            Vec::new()
        }
    }
}

/// `project`'s transport, which must manage its remote's lifecycle.
fn managed(project: &RemoteProject) -> Result<Arc<dyn Transport>> {
    let transport = crate::remote::transport::build(&project.remote_config)?;
    if transport.provisioning().is_none() {
        bail!(
            "Remote {} is reached over {}; workmux does not start or stop it",
            project.site.remote,
            project.remote_config.transport
        );
    }
    Ok(transport)
}

fn lifecycle(transport: &dyn Transport) -> &dyn Provisioning {
    transport.provisioning().expect("a managed remote")
}

fn print_notice(notice: &str) {
    eprintln!("{notice}");
}

fn start(project: &RemoteProject) -> Result<()> {
    let name = &project.site.remote;
    let transport = managed(project)?;
    ensure_running(name, transport.as_ref(), true, &mut print_notice)?;
    // Reconnect now rather than at the next backoff retry.
    for status in fetch_statuses(&project_remotes(std::slice::from_ref(project)))? {
        println!("{name}: {}", summary(&status.reach));
    }
    Ok(())
}

fn stop(project: &RemoteProject) -> Result<()> {
    let name = &project.site.remote;
    let transport = managed(project)?;
    let provisioning = lifecycle(transport.as_ref());
    match provisioning.status()? {
        RemoteState::Missing => bail!("Remote {name} has no sandbox yet"),
        RemoteState::Stopped => println!("Remote {name} is already stopped"),
        RemoteState::Running => {
            eprintln!("Stopping remote {name}…");
            provisioning.stop()?;
            println!(
                "Stopped remote {name}; its agents show as stale until `workmux remote start`"
            );
        }
    }
    Ok(())
}

/// Refuse `what` for `project` unless it lives in an OpenShell sandbox.
fn openshell_only(project: &RemoteProject, what: &str) -> Result<()> {
    if project.remote_config.openshell.is_none() {
        bail!(
            "Remote {} is not an OpenShell project's sandbox; only those are {what}",
            project.site.remote
        );
    }
    Ok(())
}

/// Ask `prompt`, unless `yes`.
fn confirmed(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    crate::ui::confirm::confirm(prompt, crate::ui::confirm::ConfirmDefault::No)
}

/// The confirmation for deleting `sandbox`, naming the work only it holds
/// and the branches not merged into the project's base branch.
fn delete_prompt(name: &str, sandbox: &str, risks: Option<&[AtRisk]>) -> String {
    with_risks(
        format!("Delete sandbox {sandbox} of project {name}? Its clone and worktrees are lost."),
        risks,
    )
}

/// The confirmation for recreating `sandbox`, naming the same work as
/// [`delete_prompt`], which the recreate carries across.
fn recreate_prompt(name: &str, sandbox: &str, risks: Option<&[AtRisk]>) -> String {
    with_risks(
        format!(
            "Recreate sandbox {sandbox} of project {name}? Agents running in it are ended and their remote panes lost; committed work on every branch is carried across."
        ),
        risks,
    )
}

/// `prompt`, then each of `risks` on a line of its own.
fn with_risks(mut prompt: String, risks: Option<&[AtRisk]>) -> String {
    match risks {
        None => prompt.push_str(
            "
  Its worktrees could not be checked for unpushed work.",
        ),
        Some([]) => {}
        Some(risks) => {
            prompt.push_str(
                "
  These have work that is unmerged or exists only in the sandbox:",
            );
            for risk in risks {
                prompt.push_str(&format!(
                    "
    {}",
                    risk.describe()
                ));
            }
        }
    }
    prompt.push_str(
        "
 ",
    );
    prompt
}

/// Delete an OpenShell project's sandbox and close the project's
/// workstation windows. A sandbox started only to check its worktrees is
/// stopped again when the delete does not go ahead.
fn rm(project: &RemoteProject, yes: bool) -> Result<()> {
    openshell_only(project, "deleted")?;
    let name = &project.site.remote;
    let transport = crate::remote::transport::build(&project.remote_config)?;
    let provisioning = lifecycle(transport.as_ref());
    let state = provisioning.status()?;
    if state != RemoteState::Missing {
        let started = state == RemoteState::Stopped;
        if started {
            eprintln!(
                "Starting remote {name} to check its worktrees for work that exists only there…"
            );
            provisioning.start()?;
        }
        let stop_again = || {
            if started {
                eprintln!("Stopping remote {name} again…");
                if let Err(error) = provisioning.stop() {
                    eprintln!("warning: remote {name} was left running: {error:#}");
                }
            }
        };
        let risks = match crate::remote::carry::remote_at_risk(
            transport.as_ref(),
            &project.site.path,
        ) {
            Ok(risks) => Some(risks),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "checking worktrees before delete");
                None
            }
        };
        match confirmed(
            &delete_prompt(
                &project.site.name,
                &project.remote_config.target,
                risks.as_deref(),
            ),
            yes,
        ) {
            Ok(true) => {}
            Ok(false) => {
                stop_again();
                println!("Kept remote {name}");
                return Ok(());
            }
            Err(error) => {
                stop_again();
                return Err(error);
            }
        }
        eprintln!("Deleting sandbox {}…", project.remote_config.target);
        provisioning.delete()?;
    }
    close_workstation_windows(&project.site.remote, &project.site.name);
    println!(
        "Deleted sandbox {} of project {}; the next workmux command in its repository creates it again",
        project.remote_config.target, project.site.name,
    );
    Ok(())
}

/// Close the workstation windows of project `name` on `remote` and untag
/// its adopted session, for a project whose remote is going away.
fn close_workstation_windows(remote: &str, name: &str) {
    use crate::multiplexer::Multiplexer as _;
    let tmux = crate::multiplexer::TmuxBackend::new();
    if !tmux.is_running().unwrap_or(false) {
        return;
    }
    match crate::remote::forwarded::close_project_windows(&tmux, remote, name) {
        Ok(0) => {}
        Ok(closed) => eprintln!("Closed {closed} workstation window(s) of project {name}"),
        Err(error) => eprintln!(
            "warning: could not close the workstation windows of project {name}: {error:#}"
        ),
    }
}

fn recreate(project: &RemoteProject, yes: bool) -> Result<()> {
    openshell_only(project, "recreated")?;
    let name = &project.site.remote;
    let transport = crate::remote::transport::build(&project.remote_config)?;
    let provisioning = lifecycle(transport.as_ref());
    let keep = crate::xdg::state_dir()?
        .join("recreate")
        .join(&project.remote_config.target);
    // An earlier recreate that failed after deleting the sandbox kept the
    // work; this one creates the sandbox if need be and restores it.
    let resuming = crate::remote::carry::has_kept_work(&keep);
    if provisioning.status()? == RemoteState::Missing && !resuming {
        return ensure_running(name, transport.as_ref(), true, &mut print_notice).map(|_| ());
    }
    ensure_running(name, transport.as_ref(), true, &mut print_notice)?;
    if !resuming && !yes {
        let risks = match crate::remote::carry::remote_at_risk(
            transport.as_ref(),
            &project.site.path,
        ) {
            Ok(risks) => Some(risks),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "checking worktrees before recreate");
                None
            }
        };
        if !confirmed(
            &recreate_prompt(
                &project.site.name,
                &project.remote_config.target,
                risks.as_deref(),
            ),
            yes,
        )? {
            println!("Kept remote {name} as it is");
            return Ok(());
        }
    }
    crate::remote::carry::recreate(
        transport.as_ref(),
        &project.site.path,
        &keep,
        &mut || {
            eprintln!("Deleting sandbox {}…", project.remote_config.target);
            provisioning.delete()?;
            eprintln!("Creating sandbox {}…", project.remote_config.target);
            provisioning.create()
        },
        &mut print_notice,
    )
    .with_context(|| format!("Failed to recreate remote {name}"))?;
    let hooks =
        transport.exec_captured(&["workmux", "setup", "--hooks", "--yes"].map(String::from))?;
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

/// Show an OpenShell remote's repository, its effective policy, read from
/// the sandbox, and its agents with the providers that give them
/// credentials.
fn print_policy(name: &str, remote: &RemoteConfig) {
    if let Some(origin) = &remote.openshell
        && let Some(git_url) = &origin.git_url
    {
        println!("  repo:     {}", git_url.describe());
    }
    let sandbox =
        crate::remote::openshell::OpenShell::new(&remote.target, remote.openshell.clone());
    match sandbox.policy_report() {
        Ok(None) => {
            println!("  sandbox:  not created yet; the first command that needs it creates it")
        }
        Ok(Some((policy, warnings))) => {
            let mut lines = policy.iter();
            println!(
                "  policy:   {}",
                lines.next().map(String::as_str).unwrap_or_default()
            );
            for line in lines {
                println!("            {line}");
            }
            for warning in warnings {
                println!("  warning:  {warning}");
            }
        }
        Err(error) => println!("  policy:   unknown ({error:#}) for {name}"),
    }
    if let Some(report) = sandbox.agent_report() {
        for note in report.notes {
            println!("  agent:    {note}");
        }
        for warning in report.warnings {
            println!("  warning:  {warning}");
        }
    }
}

/// Fetch the statuses of `remotes` through the workstation daemon, which
/// holds the control channels, or directly when no daemon can run.
fn fetch_statuses(remotes: &BTreeMap<String, RemoteConfig>) -> Result<Vec<RemoteStatus>> {
    if remotes.is_empty() {
        return Ok(Vec::new());
    }
    match crate::remote::router::daemon_socket() {
        Some(socket) => {
            let result = crate::workstation_daemon::call_with_timeout(
                &socket,
                "remote.status",
                serde_json::json!({ "remotes": remotes }),
                STATUS_TIMEOUT,
            )?;
            serde_json::from_value(result).context("Workstation daemon sent unreadable statuses")
        }
        None => crate::remote::statuses(crate::remote::channels(), remotes, None),
    }
}

/// What `list` and `status` say when no project here or in an open window
/// is on a remote, and the gateway has no sandbox workmux made.
const NO_REMOTES: &str = "No remotes. A repository whose .workmux.yaml sets sandbox.backend: \
openshell gets its sandbox the first time workmux runs in it";

/// The status of a sandbox the gateway lists that no clone here or open
/// window resolves to: it is not reached.
fn gateway_summary(sandbox: &Listed) -> String {
    format!(
        "{}, project {} has no open window",
        sandbox.phase.to_lowercase(),
        sandbox.project
    )
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
    let here = if crate::remote::router::on_a_remote() {
        None
    } else {
        crate::remote::router::resolve().unwrap_or_else(|error| {
            eprintln!("warning: {error:#}");
            None
        })
    };
    let remotes = known_remotes(here);
    let statuses = fetch_statuses(&remotes)?;
    let listed = gateway_only(&remotes);
    if statuses.is_empty() && listed.is_empty() {
        println!("{NO_REMOTES}");
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
        .chain(listed.iter().map(|sandbox| Row {
            name: sandbox.name.clone(),
            transport: TransportKind::Openshell.to_string(),
            target: sandbox.name.clone(),
            status: gateway_summary(sandbox),
        }))
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

/// Print the status of each of `remotes`, then each of the gateway's
/// sandboxes in `listed`. Returns whether none was unreachable.
fn status(remotes: &BTreeMap<String, RemoteConfig>, listed: &[Listed]) -> Result<bool> {
    let statuses = fetch_statuses(remotes)?;
    if statuses.is_empty() && listed.is_empty() {
        println!("{NO_REMOTES}");
        return Ok(true);
    }
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
    for (i, sandbox) in listed.iter().enumerate() {
        if i > 0 || !statuses.is_empty() {
            println!();
        }
        println!("{} (openshell {})", sandbox.name, sandbox.name);
        println!("  status:   {}", gateway_summary(sandbox));
    }
    Ok(all_reachable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declining_to_delete_a_stopped_sandbox_stops_it_again() {
        const TEST_NAME: &str =
            "command::remote::tests::declining_to_delete_a_stopped_sandbox_stops_it_again";
        if !crate::test_support::is_isolated_child(TEST_NAME) {
            use std::os::unix::fs::PermissionsExt;
            let temp = tempfile::tempdir().unwrap();
            let bin = temp.path().join("bin");
            let config = temp.path().join("config/workmux");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&config).unwrap();
            let dir = temp.path().display();
            std::fs::write(temp.path().join("phase"), "Stopped").unwrap();
            for (name, body) in [
                (
                    "openshell",
                    format!(
                        "#!/bin/sh\n\
                         printf '%s\\n' \"$*\" >> '{dir}/log'\n\
                         case \"$1 $2\" in\n\
                         'sandbox get') printf '{{\"phase\":\"%s\",\"policy\":{{}}}}' \"$(cat '{dir}/phase')\" ;;\n\
                         'sandbox start') echo Ready > '{dir}/phase' ;;\n\
                         'sandbox stop') echo Stopped > '{dir}/phase' ;;\n\
                         'sandbox delete') rm -f '{dir}/phase' ;;\n\
                         'sandbox exec') shift 2; while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\" ;;\n\
                         esac\n"
                    ),
                ),
                ("workmux", "#!/bin/sh\necho '[]'\n".to_string()),
                ("docker", "#!/bin/sh\nexit 1\n".to_string()),
            ] {
                let path = bin.join(name);
                std::fs::write(&path, body).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let path = std::env::join_paths(
                std::iter::once(bin.clone())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            crate::test_support::run_isolated_test(
                TEST_NAME,
                temp.path(),
                &[
                    ("XDG_CONFIG_HOME", &temp.path().join("config")),
                    ("XDG_STATE_HOME", temp.path()),
                    ("HOME", temp.path()),
                    ("PATH", std::path::Path::new(&path)),
                ],
            );
            let log = std::fs::read_to_string(temp.path().join("log")).unwrap_or_default();
            assert!(log.contains("sandbox start"), "{log}");
            assert!(!log.contains("sandbox delete"), "{log}");
            assert_eq!(
                std::fs::read_to_string(temp.path().join("phase"))
                    .unwrap()
                    .trim(),
                "Stopped",
                "{log}"
            );
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);

        // stdin is closed: the confirmation reads no, the default.
        rm(&crate::remote::testing::openshell_api(), false).unwrap();
    }

    #[test]
    fn deleting_a_sandbox_names_the_work_that_exists_only_there() {
        let risks = [
            AtRisk {
                path: Some("/sandbox/api__worktrees/feature".into()),
                branch: Some("feature".to_string()),
                uncommitted: true,
                unpushed: 2,
                unmerged_into: Some("main".to_string()),
            },
            AtRisk {
                path: None,
                branch: Some("spike".to_string()),
                uncommitted: false,
                unpushed: 1,
                unmerged_into: None,
            },
            AtRisk {
                path: None,
                branch: Some("review".to_string()),
                uncommitted: false,
                unpushed: 0,
                unmerged_into: Some("main".to_string()),
            },
        ];

        assert_eq!(
            delete_prompt("api", "api", Some(&risks)),
            "Delete sandbox api of project api? Its clone and worktrees are lost.\n  \
             These have work that is unmerged or exists only in the sandbox:\n    \
             /sandbox/api__worktrees/feature (branch feature): uncommitted changes, 2 commits not on any remote, not merged into main\n    \
             branch spike: 1 commit not on any remote\n    \
             branch review: not merged into main\n "
        );
        assert!(delete_prompt("api", "api", None).contains("could not be checked"));
    }

    #[test]
    fn recreating_a_sandbox_names_the_unmerged_work_it_carries_across() {
        let risks = [AtRisk {
            path: Some("/sandbox/api__worktrees/review".into()),
            branch: Some("review".to_string()),
            uncommitted: false,
            unpushed: 0,
            unmerged_into: Some("main".to_string()),
        }];

        assert_eq!(
            recreate_prompt("api", "api", Some(&risks)),
            "Recreate sandbox api of project api? Agents running in it are ended and their remote panes lost; committed work on every branch is carried across.\n  \
             These have work that is unmerged or exists only in the sandbox:\n    \
             /sandbox/api__worktrees/review (branch review): not merged into main\n "
        );
        assert!(recreate_prompt("api", "api", None).contains("could not be checked"));
    }
}
