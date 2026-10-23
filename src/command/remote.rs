//! `workmux remote`: register remotes and check that they are reachable.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::borrow::Cow;
use std::time::Duration;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use super::global_config::{self, REMOTES};
use crate::config::{RemoteConfig, TransportKind};
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
    /// Remove a remote from your global config
    Rm { name: String },
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
            },
        ),
        RemoteCommand::List => list(),
        RemoteCommand::Status { name } => {
            if !status(name.as_deref())? {
                std::process::exit(1);
            }
            Ok(())
        }
        RemoteCommand::Rm { name } => rm(&name),
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
    }
}

fn add(name: &str, remote: RemoteConfig) -> Result<()> {
    registry::validate_name(name)?;
    let (path, content) = global_config::read()?;
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

fn rm(name: &str) -> Result<()> {
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
            } => {
                println!("  status:   reachable");
                println!("  workmux:  {workmux}");
                println!("  protocol: {protocol}");
                if let Some(warning) = warning {
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
        }
    }

    fn parse(content: &str) -> Config {
        serde_yaml::from_str(content).unwrap()
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
