//! `workmux remote`: check that remotes are reachable and manage the
//! lifecycle of OpenShell sandboxes.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::borrow::Cow;
use std::time::Duration;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::config::{RemoteConfig, TransportKind};
use crate::remote::carry::AtRisk;
use crate::remote::channels::{Reach, RemoteStatus};
use crate::remote::pane::{RemotePaneId, SessionHolder};
use crate::remote::registry;
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
    /// Delete an OpenShell project's sandbox and close its workstation windows
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

/// A remote whose transport manages its lifecycle.
fn managed(name: &str) -> Result<(RemoteConfig, Arc<dyn Transport>)> {
    let remote = registry::remotes()?
        .remove(name)
        .ok_or_else(|| anyhow!("No remote named {name}"))?;
    let transport = registry::transport(name, &remote)?;
    if transport.provisioning().is_none() {
        bail!(
            "Remote {name} is reached over {}; workmux does not start or stop it",
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
    for status in fetch_statuses(Some(name))? {
        println!("{name}: {}", summary(&status.reach));
    }
    Ok(())
}

fn stop(name: &str) -> Result<()> {
    let (_, transport) = managed(name)?;
    let provisioning = lifecycle(transport.as_ref());
    match provisioning.status()? {
        RemoteState::Missing => bail!("Remote {name} has no sandbox yet"),
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

/// The OpenShell project whose sandbox is remote `name`.
fn openshell_project(name: &str) -> Result<Option<crate::remote::router::RemoteProject>> {
    let Some(origin) = registry::remotes()?
        .remove(name)
        .and_then(|remote| remote.openshell)
    else {
        return Ok(None);
    };
    crate::remote::router::remote_project_on(name, &origin.project).map(Some)
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

/// Delete an OpenShell project's sandbox, close the project's workstation
/// windows, then forget the project until a command resolves it again. A sandbox started only to check
/// its worktrees is stopped again when the delete does not go ahead.
fn rm_openshell(project: &crate::remote::router::RemoteProject, yes: bool) -> Result<()> {
    let name = &project.site.remote;
    let transport = registry::transport(name, &project.remote)?;
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
            &delete_prompt(&project.site.name, &project.remote.target, risks.as_deref()),
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
        eprintln!("Deleting sandbox {}…", project.remote.target);
        provisioning.delete()?;
    }
    close_workstation_windows(&project.site.remote, &project.site.name);
    registry::forget_remote(name)?;
    println!(
        "Deleted sandbox {} of project {}; the next workmux command in its repository creates it again",
        project.remote.target, project.site.name,
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

fn recreate(name: &str, yes: bool) -> Result<()> {
    let project = openshell_project(name)?.ok_or_else(|| {
        anyhow!("Remote {name} is not an OpenShell project's sandbox; only those are recreated")
    })?;
    let transport = registry::transport(name, &project.remote)?;
    let provisioning = lifecycle(transport.as_ref());
    let keep = crate::xdg::state_dir()?
        .join("recreate")
        .join(&project.remote.target);
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
            &recreate_prompt(&project.site.name, &project.remote.target, risks.as_deref()),
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
            eprintln!("Deleting sandbox {}…", project.remote.target);
            provisioning.delete()?;
            eprintln!("Creating sandbox {}…", project.remote.target);
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

fn rm(name: &str, yes: bool) -> Result<()> {
    match openshell_project(name)? {
        Some(project) => rm_openshell(&project, yes),
        None if registry::remotes()?.contains_key(name) => {
            bail!("Remote {name} is no OpenShell sandbox; workmux deletes only those")
        }
        None => bail!("No remote named {name}"),
    }
}

/// Fetch statuses through the workstation daemon, which holds the control
/// channels, or directly when no daemon can run.
fn fetch_statuses(name: Option<&str>) -> Result<Vec<RemoteStatus>> {
    let remotes = registry::remotes()?;
    if let Some(name) = name
        && !remotes.contains_key(name)
    {
        bail!("No remote named {name}");
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
            serde_json::from_value(result).context("Workstation daemon sent unreadable statuses")
        }
        None => crate::remote::statuses(crate::remote::channels(), &remotes, name),
    }
}

/// What `list` and `status` say when no command has put a project on a
/// remote yet.
const NO_REMOTES: &str = "No remotes. A repository whose .workmux.yaml sets sandbox.backend: \
openshell gets its sandbox the first time workmux runs in it";

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
        println!("{NO_REMOTES}");
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

        let records = tempfile::tempdir().unwrap();
        crate::remote::registry::with_recorded(records.path(), &[openshell_api()], || {
            // stdin is closed: the confirmation reads no, the default.
            rm("api", false).unwrap();

            assert!(openshell_project("api").unwrap().is_some());
        });
    }

    /// Project `api` in its OpenShell sandbox `api`.
    fn openshell_api() -> crate::remote::router::RemoteProject {
        let git_url = Some(crate::remote::git_url::GitUrl::new(
            "https://example.com/api.git",
        ));
        crate::remote::router::RemoteProject {
            site: crate::remote::session::ProjectSite {
                remote: "api".to_string(),
                name: "api".to_string(),
                path: "~/api".to_string(),
                git_url: git_url.clone(),
                session: Some("api".to_string()),
            },
            remote: RemoteConfig {
                transport: TransportKind::Openshell,
                target: "api".to_string(),
                builtin_shims: None,
                host_commands: Vec::new(),
                openshell: Some(crate::config::OpenshellRemote {
                    project: "api".to_string(),
                    git_url,
                    path: "~/api".to_string(),
                    settings: crate::config::OpenshellProject::default(),
                    agents: vec!["claude".to_string()],
                }),
            },
        }
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
