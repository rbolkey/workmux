//! `workmux sandbox` for a repository whose config puts it in an OpenShell
//! sandbox: stop and prune workmux's sandboxes, rebuild and report on the
//! repository's own, and open a shell in it.
//!
//! Unlike a Lima VM, an OpenShell sandbox holds the project's clone rather
//! than mounting the workstation's worktrees, so deleting one loses the work
//! that exists only there.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{RemoteConfig, TransportKind};
use crate::remote::carry::AtRisk;
use crate::remote::channels::{Reach, RemoteStatus};
use crate::remote::openshell::{Listed, OpenShell};
use crate::remote::router::RemoteProject;
use crate::remote::transport::{Provisioning, RemoteState, Transport, TtyExec, ensure_running};
use crate::sandbox::lima::VM_PREFIX;

/// How long a status call may take: opening a channel can wait on a slow
/// transport.
const STATUS_TIMEOUT: Duration = Duration::from_secs(120);

/// The project this command targets: the working directory's repository,
/// or the project window it was run from.
pub fn current_project() -> Result<RemoteProject> {
    crate::remote::router::resolve()?.ok_or_else(|| {
        anyhow!(
            "Not in a project with a sandbox; run this inside a clone of a repository \
             whose .workmux.yaml sets sandbox.backend: openshell"
        )
    })
}

/// workmux's sandboxes on the gateway: those it made (by their annotation)
/// whose names carry the `wm-` prefix. Others are never listed or touched.
fn workmux_sandboxes() -> Result<Vec<Listed>> {
    Ok(crate::remote::openshell::workmux_sandboxes()?
        .into_iter()
        .filter(|sandbox| sandbox.name.starts_with(VM_PREFIX))
        .collect())
}

/// The sandbox `sandbox` names, with the current repository's settings when
/// it is the current repository's: a sandbox started without them keeps its
/// policy as it is.
fn transport_for(sandbox: &Listed, here: Option<&RemoteProject>) -> Arc<dyn Transport> {
    match here.filter(|project| project.remote_config.target == sandbox.name) {
        Some(project) => match crate::remote::transport::build(&project.remote_config) {
            Ok(transport) => transport,
            Err(_) => Arc::new(OpenShell::new(&sandbox.name, None)),
        },
        None => Arc::new(OpenShell::new(&sandbox.name, None)),
    }
}

fn lifecycle(transport: &dyn Transport) -> &dyn Provisioning {
    transport
        .provisioning()
        .expect("an OpenShell sandbox has a lifecycle")
}

/// Read one line of input after `prompt`.
fn read_answer(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("Failed to flush stdout")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    Ok(input.trim().to_string())
}

/// Stop workmux's OpenShell sandboxes: `name`, every running one (`all`), or
/// those picked from a list that defaults to the current repository's.
pub fn stop(name: Option<String>, all: bool, skip_confirm: bool) -> Result<()> {
    let sandboxes = workmux_sandboxes()?;
    let running: Vec<&Listed> = sandboxes
        .iter()
        .filter(|sandbox| sandbox.state() == RemoteState::Running)
        .collect();

    let to_stop: Vec<&Listed> = if all {
        if running.is_empty() {
            println!("No running workmux sandboxes found.");
            return Ok(());
        }
        running
    } else if let Some(name) = &name {
        match sandboxes.iter().find(|sandbox| sandbox.name == *name) {
            Some(sandbox) if sandbox.state() == RemoteState::Running => vec![sandbox],
            Some(sandbox) => {
                println!(
                    "Sandbox '{name}' is already stopped (phase: {}).",
                    sandbox.phase
                );
                return Ok(());
            }
            None => bail!("Sandbox '{name}' not found. Use 'workmux sandbox stop' to see them"),
        }
    } else {
        if !io::stdin().is_terminal() {
            bail!("Non-interactive stdin detected. Use --all or specify a sandbox name");
        }
        if running.is_empty() {
            println!("No running workmux sandboxes found.");
            return Ok(());
        }
        let here = current_project()
            .ok()
            .map(|project| project.remote_config.target);
        select_interactive(&running, here.as_deref())?
    };

    println!("The following sandboxes will be stopped:");
    for sandbox in &to_stop {
        println!("  - {} (project {})", sandbox.name, sandbox.project);
    }
    if !skip_confirm {
        let answer = read_answer(&format!(
            "\nAre you sure you want to stop {} sandbox(es)? Every process in them ends, agents included. [y/N] ",
            to_stop.len()
        ))?
        .to_ascii_lowercase();
        if !matches!(answer.as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut stopped = 0;
    let mut failed: Vec<(String, String)> = Vec::new();
    for sandbox in to_stop {
        print!("Stopping {}... ", sandbox.name);
        io::stdout().flush()?;
        match OpenShell::new(&sandbox.name, None).stop() {
            Ok(()) => {
                println!("✓");
                stopped += 1;
            }
            Err(error) => {
                println!("✗");
                failed.push((sandbox.name.clone(), format!("{error:#}")));
            }
        }
    }
    if stopped > 0 {
        println!(
            "\n✓ Successfully stopped {stopped} sandbox(es); the next command that needs one, \
             such as `workmux open`, starts it again"
        );
    }
    if !failed.is_empty() {
        eprintln!("\nFailed to stop {} sandbox(es):", failed.len());
        for (name, error) in &failed {
            eprintln!("  - {name}: {error}");
        }
        bail!("Some sandboxes could not be stopped");
    }
    Ok(())
}

/// Ask which of `running` to stop. An empty answer picks `default`, the
/// current repository's sandbox, when it is among them.
fn select_interactive<'a>(
    running: &[&'a Listed],
    default: Option<&str>,
) -> Result<Vec<&'a Listed>> {
    let default = default.and_then(|name| running.iter().position(|s| s.name == name));
    println!("Running workmux sandboxes:");
    println!();
    for (i, sandbox) in running.iter().enumerate() {
        let mark = if Some(i) == default {
            " [this repository]"
        } else {
            ""
        };
        println!(
            "  {}. {} (project {}){mark}",
            i + 1,
            sandbox.name,
            sandbox.project
        );
    }
    println!();
    match default {
        Some(i) => println!(
            "Enter sandbox number to stop, 'all' for all sandboxes, or nothing for {}:",
            i + 1
        ),
        None => println!("Enter sandbox number to stop (or 'all' for all sandboxes):"),
    }
    let input = read_answer("> ")?;
    if input.eq_ignore_ascii_case("all") {
        return Ok(running.to_vec());
    }
    if input.is_empty()
        && let Some(i) = default
    {
        return Ok(vec![running[i]]);
    }
    let i: usize = input
        .parse()
        .context("Invalid input. Please enter a number or 'all'")?;
    if i < 1 || i > running.len() {
        bail!(
            "Invalid selection. Please choose a number between 1 and {}",
            running.len()
        );
    }
    Ok(vec![running[i - 1]])
}

/// A sandbox `prune` would delete, with what checking it found.
struct Candidate {
    sandbox: Listed,
    transport: Arc<dyn Transport>,
    /// Its work that is unmerged or exists only there: `None` when it could
    /// not be checked, or was not (`--force`).
    risks: Option<Vec<AtRisk>>,
    /// Started only to be checked: stopped again unless deleted.
    started: bool,
}

impl Candidate {
    fn has_uncommitted_changes(&self) -> bool {
        self.risks
            .as_deref()
            .is_some_and(|risks| risks.iter().any(|risk| risk.uncommitted))
    }

    fn stop_again(&self) {
        if self.started {
            eprintln!("Stopping sandbox {} again...", self.sandbox.name);
            if let Err(error) = lifecycle(self.transport.as_ref()).stop() {
                eprintln!(
                    "warning: sandbox {} was left running: {error:#}",
                    self.sandbox.name
                );
            }
        }
    }
}

/// The one confirmation `prune` asks for, naming first the work that is
/// unmerged or exists only in a sandbox (its unpushed commits are lost).
fn confirmation(candidates: &[Candidate]) -> String {
    let mut prompt = String::new();
    for candidate in candidates {
        for risk in candidate.risks.iter().flatten() {
            if prompt.is_empty() {
                prompt.push_str("These have work that is unmerged or exists only in a sandbox:\n");
            }
            prompt.push_str(&format!(
                "  {}: {}\n",
                candidate.sandbox.name,
                risk.describe()
            ));
        }
    }
    if !prompt.is_empty() {
        prompt.push('\n');
    }
    prompt.push_str("Delete all these sandboxes? Their clones and worktrees are lost. [y/N] ");
    prompt
}

/// Delete workmux's OpenShell sandboxes after one confirmation, closing the
/// workstation windows of their projects, as `workmux remove` deletes
/// worktrees: refused while any has uncommitted changes (or could not be
/// checked for them), and naming unpushed commits and unmerged branches in
/// the confirmation. `force` skips the check and the confirmation.
pub fn prune(force: bool) -> Result<()> {
    let sandboxes = workmux_sandboxes()?;
    if sandboxes.is_empty() {
        println!("No workmux OpenShell sandboxes found.");
        return Ok(());
    }
    let here = current_project().ok();

    let mut candidates = Vec::new();
    for sandbox in sandboxes {
        let transport = transport_for(&sandbox, here.as_ref());
        let mut candidate = Candidate {
            sandbox,
            transport,
            risks: None,
            started: false,
        };
        if !force {
            check(&mut candidate);
        }
        candidates.push(candidate);
    }
    let stop_again = |candidates: &[Candidate]| candidates.iter().for_each(Candidate::stop_again);

    println!(
        "Found {} workmux OpenShell sandbox(es):\n",
        candidates.len()
    );
    for (i, candidate) in candidates.iter().enumerate() {
        let sandbox = &candidate.sandbox;
        println!(
            "{}. {} ({}, project {})",
            i + 1,
            sandbox.name,
            sandbox.phase.to_lowercase(),
            sandbox.project
        );
        match &candidate.risks {
            None if force => {}
            None => {
                println!("   Its worktrees could not be checked for work that exists only there")
            }
            Some(risks) => {
                for risk in risks {
                    println!("   {}", risk.describe());
                }
            }
        }
        println!();
    }

    if !force {
        let names = |keep: &dyn Fn(&Candidate) -> bool| -> Vec<&str> {
            candidates
                .iter()
                .filter(|candidate| keep(candidate))
                .map(|candidate| candidate.sandbox.name.as_str())
                .collect()
        };
        let uncommitted = names(&|candidate| candidate.has_uncommitted_changes());
        if !uncommitted.is_empty() {
            stop_again(&candidates);
            bail!(
                "Cannot delete sandboxes with uncommitted changes: {}. Use --force to override",
                uncommitted.join(", ")
            );
        }
        let unchecked = names(&|candidate| candidate.risks.is_none());
        if !unchecked.is_empty() {
            stop_again(&candidates);
            bail!(
                "Could not check {} for uncommitted changes. Use --force to delete anyway",
                unchecked.join(", ")
            );
        }
        let answer = read_answer(&confirmation(&candidates));
        match answer {
            Ok(answer) if answer.eq_ignore_ascii_case("y") => {}
            Ok(_) => {
                stop_again(&candidates);
                println!("Aborted.");
                return Ok(());
            }
            Err(error) => {
                stop_again(&candidates);
                return Err(error);
            }
        }
    }

    println!("\nDeleting sandboxes...");
    let mut deleted = 0;
    let mut failed: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        let sandbox = &candidate.sandbox;
        print!("  Deleting {}... ", sandbox.name);
        io::stdout().flush().ok();
        match lifecycle(candidate.transport.as_ref()).delete() {
            Ok(()) => {
                println!("done");
                deleted += 1;
                close_workstation_windows(&sandbox.name, &sandbox.project);
            }
            Err(error) => {
                println!("failed");
                failed.push((sandbox.name.clone(), format!("{error:#}")));
            }
        }
    }
    println!();
    if deleted > 0 {
        println!("Deleted {deleted} sandbox(es).");
    }
    if !failed.is_empty() {
        eprintln!("\nFailed to delete {} sandbox(es):", failed.len());
        for (name, error) in &failed {
            eprintln!("  - {name}: {error}");
        }
        bail!("Some sandboxes could not be deleted");
    }
    Ok(())
}

/// Check `candidate`'s worktrees and branches for work that is unmerged or
/// exists only in it, starting a stopped sandbox to look.
fn check(candidate: &mut Candidate) {
    let name = &candidate.sandbox.name;
    let provisioning = lifecycle(candidate.transport.as_ref());
    if candidate.sandbox.state() != RemoteState::Running {
        eprintln!(
            "Starting sandbox {name} to check its worktrees for work that exists only there..."
        );
        if let Err(error) = provisioning.start() {
            tracing::warn!(sandbox = %name, error = %format!("{error:#}"), "starting a sandbox to check it");
            return;
        }
        candidate.started = true;
    }
    match crate::remote::carry::remote_at_risk(
        candidate.transport.as_ref(),
        &candidate.sandbox.path,
    ) {
        Ok(risks) => candidate.risks = Some(risks),
        Err(error) => {
            tracing::warn!(sandbox = %name, error = %format!("{error:#}"), "checking worktrees before prune");
        }
    }
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

/// The confirmation for rebuilding `sandbox`, naming the work only it
/// holds and the branches not merged into the project's base branch, which
/// the rebuild carries across.
fn rebuild_prompt(name: &str, sandbox: &str, risks: Option<&[AtRisk]>) -> String {
    let mut prompt = format!(
        "Rebuild sandbox {sandbox} of project {name}? Agents running in it are ended and their remote panes lost; committed work on every branch is carried across."
    );
    match risks {
        None => prompt.push_str("\n  Its worktrees could not be checked for unpushed work."),
        Some([]) => {}
        Some(risks) => {
            prompt.push_str("\n  These have work that is unmerged or exists only in the sandbox:");
            for risk in risks {
                prompt.push_str(&format!("\n    {}", risk.describe()));
            }
        }
    }
    prompt.push_str("\n ");
    prompt
}

/// Delete the current repository's sandbox and create it again from the
/// current image and config, carrying its committed work across.
pub fn rebuild(project: &RemoteProject, yes: bool) -> Result<()> {
    openshell_only(project, "rebuilt")?;
    // Refused before the sandbox is deleted, not by the create after it.
    let installed = crate::remote::openshell::image::installed_workmux();
    crate::remote::openshell::image::linux_workmux(installed.as_deref())?;
    let name = &project.site.remote;
    let transport = crate::remote::transport::build(&project.remote_config)?;
    let provisioning = lifecycle(transport.as_ref());
    let keep = crate::xdg::state_dir()?
        .join("recreate")
        .join(&project.remote_config.target);
    // An earlier rebuild that failed after deleting the sandbox kept the
    // work; this one creates the sandbox if need be and restores it.
    let resuming = crate::remote::carry::has_kept_work(&keep);
    if provisioning.status()? == RemoteState::Missing && !resuming {
        return ensure_running(name, transport.as_ref(), true, &mut |notice| {
            eprintln!("{notice}")
        })
        .map(|_| ());
    }
    ensure_running(name, transport.as_ref(), true, &mut |notice| {
        eprintln!("{notice}")
    })?;
    if !resuming && !yes {
        let risks = match crate::remote::carry::remote_at_risk(
            transport.as_ref(),
            &project.site.path,
        ) {
            Ok(risks) => Some(risks),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "checking worktrees before rebuild");
                None
            }
        };
        if !crate::ui::confirm::confirm(
            &rebuild_prompt(
                &project.site.name,
                &project.remote_config.target,
                risks.as_deref(),
            ),
            crate::ui::confirm::ConfirmDefault::No,
        )? {
            println!("Kept sandbox {name} as it is");
            return Ok(());
        }
    }
    crate::remote::carry::recreate(
        transport.as_ref(),
        &project.site.path,
        &keep,
        &mut || {
            eprintln!("Deleting sandbox {}...", project.remote_config.target);
            provisioning.delete()?;
            eprintln!("Creating sandbox {}...", project.remote_config.target);
            provisioning.create()
        },
        &mut |notice| eprintln!("{notice}"),
    )
    .with_context(|| format!("Failed to rebuild sandbox {name}"))?;
    let hooks =
        transport.exec_captured(&["workmux", "setup", "--hooks", "--yes"].map(String::from))?;
    if !hooks.status.success() {
        eprintln!(
            "warning: installing agent hooks in {name} failed ({}); run `workmux setup --hooks` there",
            hooks.status
        );
    }
    println!(
        "Rebuilt sandbox {name}; its branches and worktrees are back. Reopen worktrees with `workmux open`"
    );
    Ok(())
}

/// Open a shell, or run `command`, in the current worktree inside the
/// repository's sandbox: the worktree of the project window it was run
/// from, else the sandbox's clone. Creates or starts the sandbox, as a
/// command that acts does.
pub fn shell(project: &RemoteProject, exec: bool, command: Vec<String>) -> Result<()> {
    if exec {
        bail!(
            "The --exec flag is only supported with the container backend; \
             'workmux sandbox shell' always connects to the project's sandbox"
        );
    }
    eprintln!("Running in {}", project.target());
    crate::remote::router::ready(project, true)?;
    // A command session clones the project on first use and runs in the
    // invoking window's worktree.
    let (code, stdout) = crate::remote::router::capture_running(project, &["pwd"])?;
    if code != 0 {
        bail!(
            "Failed to find the worktree in {} (pwd exited with {code})",
            project.target()
        );
    }
    let workdir = String::from_utf8_lossy(&stdout).trim().to_string();
    let transport = crate::remote::transport::build(&project.remote_config)?;
    let status = transport
        .tty_command(&TtyExec {
            argv: shell_argv(&command),
            workdir: Some(workdir),
            env: Vec::new(),
        })?
        .status()
        .context("Failed to run a shell in the sandbox")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// The sandbox user's login shell, or `command` run by it.
fn shell_argv(command: &[String]) -> Vec<String> {
    let login =
        r#"shell=${SHELL:-$(getent passwd "$(id -u)" | cut -d: -f7)}; exec "${shell:-/bin/sh}" -l"#;
    if command.is_empty() {
        ["sh", "-c", login].map(String::from).to_vec()
    } else {
        [
            "sh",
            "-c",
            &format!(r#"{login} -c "$1""#),
            "sh",
            &command.join(" "),
        ]
        .map(String::from)
        .to_vec()
    }
}

/// Print the repository's sandbox: whether it is reachable, with its
/// workmux and protocol versions and warnings, and for OpenShell its
/// effective policy and agents. Returns whether it was reachable (or not
/// meant to be: stopped, or not created yet).
pub fn status(project: &RemoteProject) -> Result<bool> {
    let remotes: BTreeMap<String, RemoteConfig> =
        [(project.site.remote.clone(), project.remote_config.clone())].into();
    let mut all_reachable = true;
    for status in fetch_statuses(&remotes)? {
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
            Reach::Missing => println!("  status:   not created yet"),
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

/// Show an OpenShell remote's repository, its effective policy, read from
/// the sandbox, and its agents with the providers that give them
/// credentials.
fn print_policy(name: &str, remote: &RemoteConfig) {
    if let Some(origin) = &remote.openshell
        && let Some(git_url) = &origin.git_url
    {
        println!("  repo:     {}", git_url.describe());
    }
    let sandbox = OpenShell::new(&remote.target, remote.openshell.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A gateway with workmux's sandboxes `wm-api-…` (running, holding
    /// `api` work) and `wm-web-…` (stopped, its work pushed and merged but
    /// for a pushed unmerged branch), the trial's unprefixed `trellis-…` and
    /// an unannotated `wm-theirs`: a fake `openshell` and remote `workmux` on
    /// PATH, logging every call.
    struct Gateway {
        dir: tempfile::TempDir,
    }

    const API: &str = "wm-api-1a2b3c4d";

    /// The work `wm-api-…` holds.
    enum Work {
        None,
        Uncommitted,
        Unpushed,
    }
    const WEB: &str = "wm-web-5e6f7a8b";
    const TRELLIS: &str = "trellis-7fba5166";

    impl Gateway {
        fn new(api: Work) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            std::fs::create_dir_all(d.join("bin")).unwrap();
            for (name, phase) in [
                (API, "Ready"),
                (WEB, "Stopped"),
                (TRELLIS, "Ready"),
                ("wm-theirs", "Ready"),
            ] {
                std::fs::write(d.join(format!("phase-{name}")), phase).unwrap();
            }
            let pushed = r#"[{"path":"/sandbox/web","branch":"main","uncommitted":false,"unpushed":0,"unmerged_into":null},{"path":null,"branch":"review","uncommitted":false,"unpushed":0,"unmerged_into":"main"}]"#;
            let api = match api {
                Work::None => "[]",
                Work::Uncommitted => {
                    r#"[{"path":"/sandbox/api__worktrees/feature","branch":"feature","uncommitted":true,"unpushed":2,"unmerged_into":"main"}]"#
                }
                Work::Unpushed => {
                    r#"[{"path":null,"branch":"spike","uncommitted":false,"unpushed":2,"unmerged_into":"main"}]"#
                }
            };
            std::fs::write(d.join(format!("risks-{API}")), api).unwrap();
            std::fs::write(d.join(format!("risks-{WEB}")), pushed).unwrap();
            std::fs::write(d.join(format!("risks-{TRELLIS}")), api).unwrap();
            let annotated = |name: &str, project: &str, path: &str| {
                format!(
                    r#"{{"name":"{name}","phase":"PHASE-{name}","annotations":{{"workmux.project":"{project}"{path}}}}}"#
                )
            };
            std::fs::write(
                d.join("sandboxes"),
                format!(
                    r#"{{"next_page_token":"","sandboxes":[{},{},{},{{"name":"wm-theirs","phase":"Ready","annotations":{{}}}}]}}"#,
                    annotated(API, "api", r#","workmux.path":"/sandbox/api""#),
                    annotated(WEB, "web", ""),
                    annotated(TRELLIS, "trellis", ""),
                ),
            )
            .unwrap();
            let dir_s = d.display();
            let openshell = format!(
                "#!/bin/sh\n\
                 dir='{dir_s}'\n\
                 printf '%s\\n' \"$*\" >> \"$dir/log\"\n\
                 case \"$1 $2\" in\n\
                 'sandbox list') page=$(cat \"$dir/sandboxes\");\n\
                   for f in \"$dir\"/phase-*; do n=${{f##*/phase-}}; page=$(printf '%s' \"$page\" | sed \"s/PHASE-$n/$(cat \"$f\")/\"); done;\n\
                   printf '%s' \"$page\" ;;\n\
                 'sandbox get') if [ -f \"$dir/phase-$3\" ]; then printf '{{\"phase\":\"%s\",\"policy\":{{}}}}' \"$(cat \"$dir/phase-$3\")\";\n\
                   else echo 'sandbox not found' >&2; exit 1; fi ;;\n\
                 'sandbox start') echo Ready > \"$dir/phase-$3\" ;;\n\
                 'sandbox stop') echo Stopped > \"$dir/phase-$3\" ;;\n\
                 'sandbox delete') rm -f \"$dir/phase-$3\" ;;\n\
                 'sandbox exec') export SANDBOX=$4; shift 2; while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\" ;;\n\
                 esac\n"
            );
            // Remote workmux: `remote carry at-risk <path>` lists the
            // sandbox's work.
            let workmux = format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$SANDBOX\" \"$*\" >> '{dir_s}/remote-log'\ncat '{dir_s}/risks-'\"$SANDBOX\"\n"
            );
            for (name, body) in [("openshell", openshell), ("workmux", workmux)] {
                let path = d.join("bin").join(name);
                std::fs::write(&path, body).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir }
        }

        /// Run test `name` in a child with this gateway on PATH, no tmux,
        /// and an isolated home, outside any repository.
        fn run(&self, name: &str) {
            let d = self.dir.path();
            let path = std::env::join_paths(
                std::iter::once(d.join("bin"))
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            crate::test_support::run_isolated_test(
                name,
                d,
                &[
                    ("XDG_CONFIG_HOME", &d.join("config")),
                    ("XDG_STATE_HOME", d),
                    ("HOME", d),
                    ("PATH", Path::new(&path)),
                    ("TMUX", &d.join("no-tmux")),
                    ("TMUX_PANE", Path::new("%0")),
                    ("GATEWAY", d),
                ],
            );
        }

        fn phase(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join(format!("phase-{name}")))
                .ok()
                .map(|phase| phase.trim().to_string())
        }

        fn log(&self, file: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(file)).unwrap_or_default()
        }

        /// The sandboxes named in `openshell` calls other than `list`.
        fn touched(&self) -> Vec<String> {
            let mut touched: Vec<String> = self
                .log("log")
                .lines()
                .filter(|line| !line.starts_with("sandbox list"))
                .filter_map(|line| {
                    let words: Vec<&str> = line.split(' ').collect();
                    match words.get(1) {
                        Some(&"exec") => words.get(3),
                        _ => words.get(2),
                    }
                    .map(|name| name.to_string())
                })
                .collect();
            touched.sort();
            touched.dedup();
            touched
        }
    }

    /// In the child: the gateway's directory.
    fn gateway_dir() -> PathBuf {
        PathBuf::from(std::env::var_os("GATEWAY").unwrap())
    }

    #[test]
    fn stop_stops_only_workmux_sandboxes() {
        const NAME: &str = "command::sandbox_openshell::tests::stop_stops_only_workmux_sandboxes";
        if !crate::test_support::is_isolated_child(NAME) {
            let gateway = Gateway::new(Work::None);
            gateway.run(NAME);
            assert_eq!(gateway.phase(API).as_deref(), Some("Stopped"));
            assert_eq!(gateway.phase(TRELLIS).as_deref(), Some("Ready"));
            assert_eq!(gateway.phase("wm-theirs").as_deref(), Some("Ready"));
            assert_eq!(gateway.touched(), [API], "{}", gateway.log("log"));
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);
        let _ = gateway_dir();

        stop(None, true, true).unwrap();
        // Only workmux's sandboxes are found by name.
        for other in [TRELLIS, "wm-theirs"] {
            let error = stop(Some(other.to_string()), false, true).unwrap_err();
            assert!(error.to_string().contains("not found"), "{error:#}");
        }
        // Already stopped: nothing to do.
        stop(Some(WEB.to_string()), false, true).unwrap();
    }

    #[test]
    fn prune_refuses_while_a_sandbox_has_uncommitted_changes_naming_it() {
        const NAME: &str = "command::sandbox_openshell::tests::prune_refuses_while_a_sandbox_has_uncommitted_changes_naming_it";
        if !crate::test_support::is_isolated_child(NAME) {
            let gateway = Gateway::new(Work::Uncommitted);
            gateway.run(NAME);
            assert_eq!(gateway.phase(API).as_deref(), Some("Ready"));
            // Started to be checked, then stopped again.
            assert_eq!(gateway.phase(WEB).as_deref(), Some("Stopped"));
            assert!(!gateway.log("log").contains("sandbox delete"));
            let remote = gateway.log("remote-log");
            // Each is checked at its clone: recorded, else the default.
            assert!(
                remote.contains(&format!("{API} remote carry at-risk /sandbox/api")),
                "{remote}"
            );
            assert!(
                remote.contains(&format!("{WEB} remote carry at-risk ~/web")),
                "{remote}"
            );
            assert_eq!(gateway.touched(), [API, WEB], "{}", gateway.log("log"));
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);

        let error = format!("{:#}", prune(false).unwrap_err());

        assert!(error.contains(API), "{error}");
        assert!(!error.contains(WEB), "{error}");
        assert!(error.contains("uncommitted changes"), "{error}");
        assert!(error.contains("--force"), "{error}");
    }

    #[test]
    fn prune_asks_rather_than_refuses_for_unpushed_or_unmerged_work() {
        const NAME: &str = "command::sandbox_openshell::tests::prune_asks_rather_than_refuses_for_unpushed_or_unmerged_work";
        if !crate::test_support::is_isolated_child(NAME) {
            let gateway = Gateway::new(Work::Unpushed);
            gateway.run(NAME);
            assert!(!gateway.log("log").contains("sandbox delete"));
            assert_eq!(gateway.phase(WEB).as_deref(), Some("Stopped"));
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);

        // stdin is closed: the confirmation reads no, the default.
        prune(false).unwrap();
    }

    #[test]
    fn the_confirmation_names_unpushed_and_unmerged_work() {
        let candidate = |name: &str, risks: Vec<AtRisk>| Candidate {
            sandbox: Listed {
                name: name.to_string(),
                project: "api".to_string(),
                path: "~/api".to_string(),
                phase: "Ready".to_string(),
            },
            transport: Arc::new(OpenShell::new(name, None)),
            risks: Some(risks),
            started: false,
        };
        let spike = AtRisk {
            path: None,
            branch: Some("spike".to_string()),
            uncommitted: false,
            unpushed: 2,
            unmerged_into: Some("main".to_string()),
        };
        let review = AtRisk {
            path: Some("/sandbox/web__worktrees/review".into()),
            branch: Some("review".to_string()),
            uncommitted: false,
            unpushed: 0,
            unmerged_into: Some("main".to_string()),
        };

        assert_eq!(
            confirmation(&[
                candidate(API, vec![spike]),
                candidate(WEB, vec![review]),
                candidate("wm-idle-0a0b0c0d", Vec::new()),
            ]),
            "These have work that is unmerged or exists only in a sandbox:\n  \
             wm-api-1a2b3c4d: branch spike: 2 commits not on any remote, not merged into main\n  \
             wm-web-5e6f7a8b: /sandbox/web__worktrees/review (branch review): not merged into main\n\
             \nDelete all these sandboxes? Their clones and worktrees are lost. [y/N] "
        );
        assert_eq!(
            confirmation(&[candidate("wm-idle-0a0b0c0d", Vec::new())]),
            "Delete all these sandboxes? Their clones and worktrees are lost. [y/N] "
        );
    }

    #[test]
    fn prune_deletes_nothing_unless_confirmed() {
        const NAME: &str =
            "command::sandbox_openshell::tests::prune_deletes_nothing_unless_confirmed";
        if !crate::test_support::is_isolated_child(NAME) {
            let gateway = Gateway::new(Work::None);
            gateway.run(NAME);
            assert!(!gateway.log("log").contains("sandbox delete"));
            assert_eq!(gateway.phase(WEB).as_deref(), Some("Stopped"));
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);

        // stdin is closed: the confirmation reads no, the default.
        prune(false).unwrap();
    }

    #[test]
    fn prune_force_deletes_workmux_sandboxes_even_with_work() {
        const NAME: &str = "command::sandbox_openshell::tests::prune_force_deletes_workmux_sandboxes_even_with_work";
        if !crate::test_support::is_isolated_child(NAME) {
            let gateway = Gateway::new(Work::Uncommitted);
            gateway.run(NAME);
            assert_eq!(gateway.phase(API), None);
            assert_eq!(gateway.phase(WEB), None);
            assert_eq!(gateway.phase(TRELLIS).as_deref(), Some("Ready"));
            assert_eq!(gateway.phase("wm-theirs").as_deref(), Some("Ready"));
            assert_eq!(gateway.touched(), [API, WEB], "{}", gateway.log("log"));
            // Nothing is checked or started: the delete goes ahead anyway.
            assert!(gateway.log("remote-log").is_empty());
            assert!(!gateway.log("log").contains("sandbox start"));
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);

        prune(true).unwrap();
    }

    #[test]
    fn without_a_linux_workmux_rebuild_refuses_before_touching_the_sandbox() {
        const NAME: &str = "command::sandbox_openshell::tests::without_a_linux_workmux_rebuild_refuses_before_touching_the_sandbox";
        if !crate::test_support::is_isolated_child(NAME) {
            let gateway = Gateway::new(Work::None);
            gateway.run(NAME);
            assert_eq!(gateway.log("log"), "");
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);
        let config: crate::config::Config =
            serde_yaml::from_str("sandbox:\n  enabled: true\n  backend: openshell\n").unwrap();
        let project = crate::remote::resolve::project_in(
            &config,
            &gateway_dir(),
            Some("https://github.com/me/api.git"),
        )
        .unwrap();

        let error = rebuild(&project, true).unwrap_err();

        assert_eq!(
            error.to_string(),
            "No workmux release for this build can serve OpenShell sandboxes yet; \
             run `workmux sandbox install-dev`"
        );
    }

    #[test]
    fn rebuilding_a_sandbox_names_the_unmerged_work_it_carries_across() {
        let risks = [AtRisk {
            path: Some("/sandbox/api__worktrees/review".into()),
            branch: Some("review".to_string()),
            uncommitted: false,
            unpushed: 0,
            unmerged_into: Some("main".to_string()),
        }];

        assert_eq!(
            rebuild_prompt("api", "wm-api-1a2b3c4d", Some(&risks)),
            "Rebuild sandbox wm-api-1a2b3c4d of project api? Agents running in it are ended and their remote panes lost; committed work on every branch is carried across.\n  \
             These have work that is unmerged or exists only in the sandbox:\n    \
             /sandbox/api__worktrees/review (branch review): not merged into main\n "
        );
        assert!(rebuild_prompt("api", "wm-api-1a2b3c4d", None).contains("could not be checked"));
    }
}
