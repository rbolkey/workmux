//! `workmux remote`: register remotes and check that they are reachable.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::config::{Config, RemoteConfig, TransportKind};
use crate::multiplexer::{create_backend, detect_backend};
use crate::remote::channels::{Reach, RemoteStatus};
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
    Rm {
        name: String,
    },
    /// Serve the control channel on stdin and stdout (run by transports)
    #[command(hide = true)]
    Serve,
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
        RemoteCommand::Serve => crate::remote::serve::run(),
    }
}

fn global_config_file() -> Result<(PathBuf, String)> {
    let path = crate::config::global_config_path()
        .ok_or_else(|| anyhow!("could not determine your home directory"))?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    Ok((path, content))
}

fn write_global_config(path: &PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

fn add(name: &str, remote: RemoteConfig) -> Result<()> {
    registry::validate_name(name)?;
    let (path, content) = global_config_file()?;
    let updated = add_remote_to(&content, name, &remote)?;
    write_global_config(&path, &updated)?;
    println!(
        "Added remote {name} ({} {}) to {}",
        remote.transport,
        remote.target,
        path.display()
    );
    Ok(())
}

fn rm(name: &str) -> Result<()> {
    let (path, content) = global_config_file()?;
    let users: Vec<String> = parse_config(&content)?
        .projects
        .into_iter()
        .filter(|(_, project)| project.remote.as_deref() == Some(name))
        .map(|(project, _)| project)
        .collect();
    if !users.is_empty() {
        bail!("remote {name} is used by project(s) {}", users.join(", "));
    }
    let updated = remove_remote_from(&content, name)?;
    write_global_config(&path, &updated)?;
    println!("Removed remote {name} from {}", path.display());
    Ok(())
}

/// The workstation daemon's socket, when a tmux server can host it.
fn daemon_socket() -> Option<PathBuf> {
    let mux = create_backend(detect_backend());
    if mux.name() != "tmux" || !mux.is_running().unwrap_or(false) {
        return None;
    }
    match crate::workstation_daemon::ensure_current() {
        Ok(socket) => Some(socket),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "workstation daemon unavailable; checking remotes directly");
            None
        }
    }
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
    match daemon_socket() {
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
        println!("No remotes. Add one with: workmux remote add <name> --transport ssh --target <host>");
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
        println!("No remotes. Add one with: workmux remote add <name> --transport ssh --target <host>");
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

fn parse_config(content: &str) -> Result<Config> {
    if content.trim().is_empty() {
        return Ok(Config::default());
    }
    serde_yaml::from_str(content).context("global config does not parse")
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The top-level `remotes:` block: the header line's index, the index just
/// past its last line, and the indent of its entries.
fn remotes_section(lines: &[&str]) -> Result<Option<(usize, usize, Option<usize>)>> {
    let Some(header) = lines.iter().position(|line| line.starts_with("remotes:")) else {
        return Ok(None);
    };
    let rest = lines[header]["remotes:".len()..].trim();
    if !(rest.is_empty() || rest.starts_with('#')) {
        bail!("the remotes section of your global config is not a block mapping; edit it by hand");
    }
    let end = lines[header + 1..]
        .iter()
        .position(|line| !is_blank(line) && indent_of(line) == 0)
        .map_or(lines.len(), |offset| header + 1 + offset);
    let indent = lines[header + 1..end]
        .iter()
        .find(|line| !is_blank(line) && !line.trim_start().starts_with('#'))
        .map(|line| indent_of(line));
    Ok(Some((header, end, indent)))
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut joined = lines.join("\n");
    if trailing_newline && !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// Add `name` to the `remotes:` section of a global config file's text,
/// keeping the rest of the file (including comments) as it is.
fn add_remote_to(content: &str, name: &str, remote: &RemoteConfig) -> Result<String> {
    let before = parse_config(content)?;
    if before.remotes.contains_key(name) {
        bail!("remote {name} already exists");
    }
    let entry = serde_yaml::to_string(&std::collections::BTreeMap::from([(name, remote)]))?;

    let lines: Vec<&str> = content.lines().collect();
    let mut updated: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    match remotes_section(&lines)? {
        Some((header, end, indent)) => {
            let pad = " ".repeat(indent.unwrap_or(2));
            // Directly after the section's last non-blank line.
            let at = (header + 1..end)
                .rev()
                .find(|&i| !is_blank(lines[i]))
                .map_or(header + 1, |i| i + 1);
            let entry_lines = entry.lines().map(|line| format!("{pad}{line}"));
            updated.splice(at..at, entry_lines);
        }
        None => {
            updated.push("remotes:".to_string());
            updated.extend(entry.lines().map(|line| format!("  {line}")));
        }
    }
    let updated = join_lines(&updated, true);

    let after = parse_config(&updated)
        .ok()
        .filter(|after| after.remotes.get(name) == Some(remote))
        .filter(|after| after.remotes.len() == before.remotes.len() + 1);
    if after.is_none() {
        bail!("could not add remote {name} to your global config automatically; add it by hand");
    }
    Ok(updated)
}

/// Remove `name` from the `remotes:` section of a global config file's text.
fn remove_remote_from(content: &str, name: &str) -> Result<String> {
    let before = parse_config(content)?;
    if !before.remotes.contains_key(name) {
        bail!("no remote named {name}");
    }
    let lines: Vec<&str> = content.lines().collect();
    let (header, end, indent) = remotes_section(&lines)?
        .ok_or_else(|| anyhow!("no remotes section in your global config"))?;
    let indent = indent.unwrap_or(2);
    let is_entry_line = |line: &str| {
        indent_of(line) == indent && {
            let key = line.trim_start();
            [format!("{name}:"), format!("\"{name}\":"), format!("'{name}':")]
                .iter()
                .any(|prefix| {
                    key.strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.trim().is_empty() || rest.trim().starts_with('#'))
                })
        }
    };
    let start = (header + 1..end)
        .find(|&i| is_entry_line(lines[i]))
        .ok_or_else(|| anyhow!("could not find remote {name} in your global config; remove it by hand"))?;
    let stop = (start + 1..end)
        .find(|&i| !is_blank(lines[i]) && indent_of(lines[i]) <= indent)
        .unwrap_or(end);

    let mut kept: Vec<&str> = lines.clone();
    kept.drain(start..stop);
    let section_end = end - (stop - start);
    let section_empty = kept[header + 1..section_end]
        .iter()
        .all(|line| is_blank(line) || line.trim_start().starts_with('#'));
    if section_empty {
        kept.drain(header..section_end);
    }
    let updated = join_lines(
        &kept.iter().map(|line| line.to_string()).collect::<Vec<_>>(),
        content.ends_with('\n'),
    );

    let after = parse_config(&updated)
        .ok()
        .filter(|after| !after.remotes.contains_key(name))
        .filter(|after| after.remotes.len() + 1 == before.remotes.len());
    if after.is_none() {
        bail!("could not remove remote {name} from your global config automatically; remove it by hand");
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let updated = add_remote_to(content, "devbox", &ssh("me@devbox")).unwrap();

        assert!(updated.starts_with(content), "{updated}");
        assert_eq!(parse(&updated).remotes["devbox"], ssh("me@devbox"));
        assert_eq!(parse(&updated).nerdfont, Some(true));
    }

    #[test]
    fn adding_to_an_empty_config_creates_it() {
        let updated = add_remote_to("", "devbox", &ssh("devbox")).unwrap();

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

        let updated = add_remote_to(
            content,
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

        let error = add_remote_to(content, "devbox", &ssh("other")).unwrap_err();

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

        let updated = remove_remote_from(content, "devbox").unwrap();

        let config = parse(&updated);
        assert!(!config.remotes.contains_key("devbox"));
        assert_eq!(config.remotes["gpu"].host_commands, vec!["just"]);
        assert_eq!(config.nerdfont, Some(true));
        assert!(updated.starts_with("# top comment\n"));
    }

    #[test]
    fn removing_the_last_remote_removes_the_section() {
        let content = "remotes:\n  devbox:\n    transport: ssh\n    target: devbox\nnerdfont: true\n";

        let updated = remove_remote_from(content, "devbox").unwrap();

        assert_eq!(updated, "nerdfont: true\n");
    }

    #[test]
    fn removing_an_unknown_remote_is_an_error() {
        let error = remove_remote_from("nerdfont: true\n", "devbox").unwrap_err();

        assert_eq!(error.to_string(), "no remote named devbox");
    }
}
