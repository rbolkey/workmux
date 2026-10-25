use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::config;
use crate::config::MuxMode;
use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
use crate::project::{Project, ProjectHost};
use crate::remote::router::{self, RemoteProject};
use crate::remote::session::ServedProject;
use crate::util::format_compact_age;
use crate::workflow::types::AgentStatusSummary;
use crate::{git, nerdfont, workflow};
use anyhow::{Context, Result, bail};
use pathdiff::diff_paths;
use serde::{Deserialize, Serialize};
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, disable::Remove, location::ByColumnName, object::Columns},
};

struct WorktreeRow {
    project: String,
    branch: String,
    age: String,
    pr_status: String,
    agent_status: String,
    mux_status: String,
    unmerged_status: String,
    path_str: String,
}

impl Tabled for WorktreeRow {
    const LENGTH: usize = 8;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
            Cow::Borrowed(&self.project),
            Cow::Borrowed(&self.branch),
            Cow::Borrowed(&self.age),
            Cow::Borrowed(&self.pr_status),
            Cow::Borrowed(&self.agent_status),
            Cow::Borrowed(&self.mux_status),
            Cow::Borrowed(&self.unmerged_status),
            Cow::Borrowed(&self.path_str),
        ]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            Cow::Borrowed("PROJECT"),
            Cow::Borrowed("BRANCH"),
            Cow::Borrowed("AGE"),
            Cow::Borrowed("PR"),
            Cow::Borrowed("AGENT"),
            Cow::Borrowed("MUX"),
            Cow::Borrowed("UNMERGED"),
            Cow::Borrowed("PATH"),
        ]
    }
}

fn format_pr_status(pr_info: Option<crate::github::PrSummary>) -> String {
    pr_info
        .map(|pr| {
            let icons = nerdfont::pr_icons();
            // GitHub-style colors: green for open, gray for draft, purple for merged, red for closed
            let (icon, color) = match pr.state.as_str() {
                "OPEN" if pr.is_draft => (icons.draft, "\x1b[90m"), // gray
                "OPEN" => (icons.open, "\x1b[32m"),                 // green
                "MERGED" => (icons.merged, "\x1b[35m"),             // purple/magenta
                "CLOSED" => (icons.closed, "\x1b[31m"),             // red
                _ => (icons.open, "\x1b[32m"),
            };
            format!("#{} {}{}\x1b[0m", pr.number, color, icon)
        })
        .unwrap_or_else(|| "-".to_string())
}

/// Format a single agent status as either an icon (TTY) or text label (piped).
fn format_status_label(status: AgentStatus, config: &config::Config, use_icons: bool) -> String {
    if use_icons {
        match status {
            AgentStatus::Working => config.status_icons.working().to_string(),
            AgentStatus::Waiting => config.status_icons.waiting().to_string(),
            AgentStatus::Done => config.status_icons.done().to_string(),
        }
    } else {
        match status {
            AgentStatus::Working => "working".to_string(),
            AgentStatus::Waiting => "waiting".to_string(),
            AgentStatus::Done => "done".to_string(),
        }
    }
}

fn format_agent_status(
    summary: Option<&AgentStatusSummary>,
    config: &config::Config,
    use_icons: bool,
) -> String {
    let summary = match summary {
        Some(s) if !s.statuses.is_empty() => s,
        _ => return "-".to_string(),
    };

    let total = summary.statuses.len();
    if total == 1 {
        format_status_label(summary.statuses[0], config, use_icons)
    } else {
        // Multiple agents: show breakdown
        let working = summary
            .statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Working))
            .count();
        let waiting = summary
            .statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Waiting))
            .count();
        let done = summary
            .statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Done))
            .count();

        let mut parts = Vec::new();
        if working > 0 {
            let label = format_status_label(AgentStatus::Working, config, use_icons);
            parts.push(format!("{}{}", working, label));
        }
        if waiting > 0 {
            let label = format_status_label(AgentStatus::Waiting, config, use_icons);
            parts.push(format!("{}{}", waiting, label));
        }
        if done > 0 {
            let label = format_status_label(AgentStatus::Done, config, use_icons);
            parts.push(format!("{}{}", done, label));
        }
        parts.join(" ")
    }
}

#[derive(Serialize, Deserialize)]
struct JsonWorktree {
    project: String,
    project_path: String,
    handle: String,
    branch: String,
    path: String,
    is_main: bool,
    mode: String,
    has_uncommitted_changes: bool,
    is_open: bool,
    agent_statuses: Vec<AgentStatus>,
    created_at: Option<u64>,
    /// The remote a remote project's worktree lives on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    /// Sent by remote workmux, so the workstation can show the column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    has_unmerged: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_branch: Option<String>,
}

struct ProjectWorktree {
    project: Project,
    project_path: PathBuf,
    worktree: workflow::types::WorktreeInfo,
    /// Known already for a remote worktree, whose files aren't here.
    has_uncommitted_changes: Option<bool>,
}

impl ProjectWorktree {
    /// A worktree of `project` as remote workmux listed it.
    fn from_remote(project: &Project, entry: JsonWorktree) -> Self {
        let statuses = entry.agent_statuses;
        Self {
            project: project.clone(),
            project_path: PathBuf::from(entry.project_path),
            worktree: workflow::types::WorktreeInfo {
                repo_key: PathBuf::new(),
                handle: entry.handle,
                branch: entry.branch,
                path: PathBuf::from(entry.path),
                is_main: entry.is_main,
                mode: if entry.mode == "session" {
                    MuxMode::Session
                } else {
                    MuxMode::Window
                },
                has_mux_window: entry.is_open,
                has_unmerged: entry.has_unmerged.unwrap_or(false),
                pr_info: None,
                agent_status: (!statuses.is_empty()).then_some(AgentStatusSummary { statuses }),
                created_at: entry.created_at,
                base_branch: entry.base_branch,
            },
            has_uncommitted_changes: Some(entry.has_uncommitted_changes),
        }
    }
}

/// The project this process lists: the remote project remote workmux runs
/// it for, else the workstation project at `root`.
fn project(root: &Path) -> Project {
    match ServedProject::current() {
        Some(served) => Project::remote(served.remote, served.name),
        None => workstation_project(root),
    }
}

fn workstation_project(root: &Path) -> Project {
    Project::from_main_worktree_root(root)
        .unwrap_or_else(|| Project::workstation(root.to_string_lossy()))
}

fn discover_project_roots(mux: &dyn crate::multiplexer::Multiplexer) -> Result<Vec<PathBuf>> {
    let agents = crate::state::StateStore::new()?.load_reconciled_agents(mux)?;
    let roots: BTreeSet<PathBuf> = agents
        .iter()
        .filter_map(|agent| git::get_main_worktree_root_in(Some(&agent.path)).ok())
        .map(|root| crate::util::canon_or_self(&root))
        .collect();
    Ok(roots.into_iter().collect())
}

fn list_project(
    root: &Path,
    mux: &dyn crate::multiplexer::Multiplexer,
    show_pr: bool,
    filter: &[String],
) -> Result<Vec<ProjectWorktree>> {
    let (config, _) = config::Config::load_with_location_from(root, None)?;
    let project = workstation_project(root);
    let worktrees = workflow::list_in(&config, mux, show_pr, filter, Some(root))?;
    Ok(worktrees
        .into_iter()
        .map(|worktree| ProjectWorktree {
            project: project.clone(),
            project_path: root.to_path_buf(),
            worktree,
            has_uncommitted_changes: None,
        })
        .collect())
}

/// List a remote project's worktrees through remote workmux, on a remote
/// already found running.
fn list_remote_project(remote: &RemoteProject, filter: &[String]) -> Result<Vec<ProjectWorktree>> {
    let mut argv = vec!["workmux", "list", "--json"];
    argv.extend(filter.iter().map(String::as_str));
    let (exit, stdout) = router::capture_running(remote, &argv)?;
    if exit != 0 {
        bail!("workmux list exited with {exit}");
    }
    remote_entries(remote, &stdout)
}

fn remote_entries(remote: &RemoteProject, stdout: &[u8]) -> Result<Vec<ProjectWorktree>> {
    let entries: Vec<JsonWorktree> =
        serde_json::from_slice(stdout).context("Remote workmux sent an unreadable list")?;
    let project = remote.project();
    Ok(entries
        .into_iter()
        .map(|entry| ProjectWorktree::from_remote(&project, entry))
        .collect())
}

/// A remote project's worktrees, listed by remote workmux without writing
/// to this terminal. Each carries the project's cache key as its repo key.
/// While the remote is stopped or unreachable, the last list it sent is
/// used, without agent statuses, so its branches keep their PR status.
pub fn remote_worktrees(remote: &RemoteProject) -> Result<Vec<workflow::types::WorktreeInfo>> {
    let saved = saved_list_path(remote);
    let (stdout, live) = match list_from_remote(remote) {
        Ok(stdout) => {
            if let Ok(path) = &saved
                && let Err(error) = save_list(path, &stdout)
            {
                tracing::warn!(error = %format!("{error:#}"), "saving the remote worktree list");
            }
            (stdout, true)
        }
        Err(error) => match saved
            .as_ref()
            .ok()
            .and_then(|path| std::fs::read(path).ok())
        {
            Some(stdout) => {
                tracing::debug!(error = %format!("{error:#}"), "using the last remote worktree list");
                (stdout, false)
            }
            None => return Err(error),
        },
    };
    let project = remote.project();
    let repo_key = project.cache_key(Path::new(""))?;
    Ok(remote_entries(remote, &stdout)?
        .into_iter()
        .map(|entry| workflow::types::WorktreeInfo {
            repo_key: repo_key.clone(),
            agent_status: entry.worktree.agent_status.filter(|_| live),
            ..entry.worktree
        })
        .collect())
}

fn list_from_remote(remote: &RemoteProject) -> Result<Vec<u8>> {
    let listed = router::captured(remote, &["workmux", "list", "--json"])?;
    if listed.exit_code != 0 {
        bail!(
            "workmux list exited with {}: {}",
            listed.exit_code,
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    Ok(listed.stdout)
}

/// Where the workstation keeps the last worktree list `remote` sent.
fn saved_list_path(remote: &RemoteProject) -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?
        .join("remote-worktrees")
        .join(&remote.site.remote)
        .join(format!("{}.json", remote.site.name)))
}

fn save_list(path: &Path, stdout: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, stdout)?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

pub fn run(
    show_pr: bool,
    json: bool,
    all: bool,
    remote_project: Option<&str>,
    filter: &[String],
) -> Result<()> {
    if let Some(remote) = router::resolve(remote_project)? {
        let mut argv = vec!["workmux", "list"];
        if show_pr {
            argv.push("--pr");
        }
        if json {
            argv.push("--json");
        }
        argv.extend(filter.iter().map(String::as_str));
        return router::exit_with(router::run(&remote, &argv)?);
    }

    let display_config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let served = ServedProject::current();
    let mut worktrees = Vec::new();
    if all {
        for root in discover_project_roots(mux.as_ref())? {
            worktrees.extend(list_project(&root, mux.as_ref(), show_pr && !json, filter)?);
        }
        for remote in router::remote_projects()? {
            // A stopped sandbox is never started just to list it.
            if !router::ready(&remote, false).unwrap_or(true) {
                eprintln!(
                    "warning: project {} is not listed: remote {} is stopped",
                    remote.site.name, remote.site.remote
                );
                continue;
            }
            match list_remote_project(&remote, filter) {
                Ok(entries) => worktrees.extend(entries),
                Err(error) => eprintln!(
                    "warning: could not list project {} on {}: {error:#}",
                    remote.site.name, remote.site.remote
                ),
            }
        }
    } else {
        let root = git::get_main_worktree_root()?;
        let project = project(&root);
        worktrees.extend(
            workflow::list(&display_config, mux.as_ref(), show_pr && !json, filter)?
                .into_iter()
                .map(|worktree| ProjectWorktree {
                    project: project.clone(),
                    project_path: root.clone(),
                    worktree,
                    has_uncommitted_changes: None,
                }),
        );
    }

    if worktrees.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No worktrees found");
        }
        return Ok(());
    }

    if json {
        let entries: Vec<JsonWorktree> = worktrees
            .into_iter()
            .map(|entry| {
                let wt = entry.worktree;
                let remote = match entry.project.host {
                    ProjectHost::Workstation => None,
                    ProjectHost::Remote(remote) => Some(remote),
                };
                JsonWorktree {
                    has_unmerged: remote.is_some().then_some(wt.has_unmerged),
                    base_branch: wt.base_branch.filter(|_| remote.is_some()),
                    remote,
                    project: entry.project.name,
                    project_path: entry.project_path.to_string_lossy().into_owned(),
                    handle: wt.handle,
                    branch: wt.branch,
                    path: wt.path.to_string_lossy().to_string(),
                    is_main: wt.is_main,
                    mode: match wt.mode {
                        MuxMode::Window => "window".to_string(),
                        MuxMode::Session => "session".to_string(),
                    },
                    has_uncommitted_changes: entry
                        .has_uncommitted_changes
                        .unwrap_or_else(|| git::has_uncommitted_changes(&wt.path).unwrap_or(true)),
                    is_open: wt.has_mux_window,
                    agent_statuses: wt
                        .agent_status
                        .map(|summary| summary.statuses)
                        .unwrap_or_default(),
                    created_at: wt.created_at,
                }
            })
            .collect();
        println!("{}", serde_json::to_string(&entries)?);
        return Ok(());
    }

    // Use icons when outputting to a terminal, text labels when piped (for agents)
    let use_icons = std::io::stdout().is_terminal();
    let current_dir = std::env::current_dir()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let display_data: Vec<WorktreeRow> = worktrees
        .into_iter()
        .map(|entry| {
            let wt = entry.worktree;
            // A remote worktree's path is shown in full: it isn't relative
            // to anywhere on the workstation.
            let remote = matches!(entry.project.host, ProjectHost::Remote(_));
            let path_str = diff_paths(&wt.path, &current_dir)
                .filter(|_| !remote)
                .map(|p| {
                    let s = p.display().to_string();
                    if s.is_empty() || s == "." {
                        "(here)".to_string()
                    } else {
                        s
                    }
                })
                .unwrap_or_else(|| wt.path.display().to_string());

            let age = if wt.is_main {
                "-".to_string()
            } else {
                wt.created_at
                    .map(|ts| format_compact_age(now.saturating_sub(ts)))
                    .unwrap_or_else(|| "-".to_string())
            };

            WorktreeRow {
                // A remote project's label names its remote.
                project: entry.project.group_label(),
                branch: wt.branch,
                age,
                pr_status: format_pr_status(wt.pr_info),
                agent_status: format_agent_status(
                    wt.agent_status.as_ref(),
                    &display_config,
                    use_icons,
                ),
                mux_status: if wt.has_mux_window {
                    "✓".to_string()
                } else {
                    "-".to_string()
                },
                unmerged_status: if wt.has_unmerged {
                    "●".to_string()
                } else {
                    "-".to_string()
                },
                path_str,
            }
        })
        .collect();

    let mut table = Table::new(display_data);
    table
        .with(Style::blank())
        .modify(Columns::new(0..8), Padding::new(0, 1, 0, 0));

    if !all && served.is_none() {
        table.with(Remove::column(ByColumnName::new("PROJECT")));
    }
    if !show_pr {
        table.with(Remove::column(ByColumnName::new("PR")));
    }

    println!("{table}");

    Ok(())
}
