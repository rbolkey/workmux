use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::git;
use crate::multiplexer::{AgentPane, AgentStatus, create_backend, detect_backend};
use crate::state::StateStore;
use crate::util;
use crate::workflow;

/// The worktree a wait names. A remote worktree's agents are matched by
/// their project host and worktree: their path is the remote's.
#[derive(Debug)]
enum WaitTarget {
    Path(std::path::PathBuf),
    Remote {
        remote: String,
        project: String,
        worktree: String,
    },
}

impl WaitTarget {
    fn agents<'a>(&self, agents: &'a [AgentPane]) -> Vec<&'a AgentPane> {
        match self {
            Self::Path(path) => workflow::match_agents_to_worktree(agents, path),
            Self::Remote {
                remote,
                project,
                worktree,
            } => agents
                .iter()
                .filter(|agent| {
                    agent.remote.as_ref().is_some_and(|known| {
                        known.remote == *remote
                            && known.project == *project
                            && known.worktree == *worktree
                    })
                })
                .collect(),
        }
    }
}

/// Resolve a worktree name to its wait target, trying local git first then
/// global agents.
///
/// Local resolution is preferred because it works even before an agent starts
/// (the worktree directory exists from `workmux add`). Global resolution requires
/// a running agent.
fn resolve_target(name: &str, mux: &dyn crate::multiplexer::Multiplexer) -> Result<WaitTarget> {
    // Try local git resolution first (supports waiting for unstarted agents)
    if git::is_git_repo().unwrap_or(false) {
        match git::find_worktree(name) {
            Ok((path, _branch)) => return Ok(WaitTarget::Path(path)),
            Err(e) if e.downcast_ref::<git::WorktreeNotFound>().is_some() => {}
            Err(e) => return Err(e),
        }
    }

    // Fall back to global agent resolution, then to a remote project's
    // worktree whose agent has not reported yet
    let (path, agents) = match workflow::resolve_worktree_agents(name, mux) {
        Ok(resolved) => resolved,
        Err(error) => return unstarted_remote_target(name)?.ok_or(error),
    };
    Ok(
        match agents.first().and_then(|agent| agent.remote.as_ref()) {
            Some(remote) => WaitTarget::Remote {
                remote: remote.remote.clone(),
                project: remote.project.clone(),
                worktree: remote.worktree.clone(),
            },
            None => WaitTarget::Path(path),
        },
    )
}

/// A remote project's worktree that exists on its remote, for a target
/// whose agent the state store does not know yet: `project:handle` naming a
/// remote project (or `project@remote:handle`), or a plain handle typed in a
/// remote project's session.
fn unstarted_remote_target(name: &str) -> Result<Option<WaitTarget>> {
    let (qualifier, handle) = match name.split_once(':') {
        Some((project, handle)) if !project.is_empty() && !handle.is_empty() => {
            (Some(project), handle)
        }
        _ => (None, name),
    };
    let project = match qualifier {
        Some(qualifier) => crate::remote::router::remote_projects()?
            .into_iter()
            .find(|project| {
                project.site.name == qualifier || project.project().group_label() == qualifier
            }),
        None => crate::remote::router::resolve(None)?,
    };
    let Some(project) = project else {
        return Ok(None);
    };
    if crate::remote::router::quietly(&project, &["workmux", "path", handle])? != 0 {
        return Ok(None);
    }
    Ok(Some(WaitTarget::Remote {
        remote: project.site.remote.clone(),
        project: project.site.name.clone(),
        worktree: handle.to_string(),
    }))
}

/// What one look at the state store says about a wait target.
#[derive(Debug, PartialEq, Eq)]
enum Observation {
    /// No agent yet: it may not have started.
    NotStarted,
    Reached,
    Pending,
    /// Only stale remote entries: their status is the last known, so it
    /// neither satisfies the wait nor ends it until the remote reconnects.
    Stale,
    /// The agent was seen and is gone.
    Gone,
}

fn observe(
    target: &WaitTarget,
    agents: &[AgentPane],
    status: AgentStatus,
    seen: bool,
) -> Observation {
    let matching = target.agents(agents);
    if matching.is_empty() {
        return if seen {
            Observation::Gone
        } else {
            Observation::NotStarted
        };
    }
    let live: Vec<_> = matching
        .into_iter()
        .filter(|agent| !agent.is_remote_stale())
        .collect();
    if live.is_empty() {
        Observation::Stale
    } else if live.iter().any(|agent| agent.status == Some(status)) {
        Observation::Reached
    } else {
        Observation::Pending
    }
}

/// Whether a worktree whose agent is gone was removed (merged) rather than
/// left with its agent exited. `None` when a remote cannot say.
fn worktree_removed(target: &WaitTarget) -> Option<bool> {
    match target {
        WaitTarget::Path(path) => Some(!path.exists()),
        WaitTarget::Remote {
            project, worktree, ..
        } => {
            let project = crate::remote::router::remote_project(
                &crate::config::Config::load_global_only().ok()?,
                project,
            )
            .ok()?;
            match crate::remote::router::quietly(&project, &["workmux", "path", worktree]) {
                Ok(exit) => Some(exit != 0),
                Err(error) => {
                    tracing::debug!(error = %format!("{error:#}"), "remote unreachable");
                    None
                }
            }
        }
    }
}

fn parse_status(s: &str) -> Result<AgentStatus> {
    match s {
        "working" => Ok(AgentStatus::Working),
        "waiting" => Ok(AgentStatus::Waiting),
        "done" => Ok(AgentStatus::Done),
        _ => Err(anyhow!(
            "Invalid status '{}'. Must be: working, waiting, done",
            s
        )),
    }
}

pub fn run(
    worktree_names: &[String],
    target_status: &str,
    timeout_secs: Option<u64>,
    any: bool,
) -> Result<()> {
    if let Some(code) = crate::remote::shell::run_relayed(crate::remote::shell::own_argv(), None)? {
        std::process::exit(code);
    }
    let target_status_name = target_status;
    let target_status = parse_status(target_status)?;
    let mux = create_backend(detect_backend());
    let start = Instant::now();

    // Resolve worktree targets upfront (local git first, then global agents)
    let targets: Vec<_> = worktree_names
        .iter()
        .map(|name| Ok((name.clone(), resolve_target(name, mux.as_ref())?)))
        .collect::<Result<Vec<_>>>()?;

    let mut reached: HashSet<String> = HashSet::new();
    let mut seen_agent: HashSet<String> = HashSet::new();
    let mut reported_stale: HashSet<String> = HashSet::new();

    loop {
        // Check timeout
        if let Some(timeout) = timeout_secs
            && start.elapsed() > Duration::from_secs(timeout)
        {
            let remaining: Vec<_> = worktree_names
                .iter()
                .filter(|n| !reached.contains(n.as_str()))
                .collect();
            eprintln!(
                "Timeout waiting for: {}",
                remaining
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            std::process::exit(1);
        }

        // Load current agent state
        let agent_panes =
            StateStore::new().and_then(|store| store.load_reconciled_agents(mux.as_ref()))?;

        for (name, target) in &targets {
            if reached.contains(name) {
                continue;
            }

            let observation = observe(
                target,
                &agent_panes,
                target_status,
                seen_agent.contains(name),
            );
            if observation != Observation::NotStarted {
                seen_agent.insert(name.clone());
            }
            if observation != Observation::Stale {
                reported_stale.remove(name);
            }
            match observation {
                Observation::Reached => {
                    let elapsed = util::format_elapsed_duration(start.elapsed());
                    eprintln!("{}: {} ({})", name, target_status_name, elapsed);
                    reached.insert(name.clone());

                    if any {
                        return Ok(());
                    }
                }
                Observation::Stale => {
                    if reported_stale.insert(name.clone())
                        && let WaitTarget::Remote { remote, .. } = target
                    {
                        eprintln!("{name}: {remote} is disconnected; waiting for it to reconnect");
                    }
                }
                // Agent was previously running but disappeared. A removed
                // worktree was merged (success).
                Observation::Gone => match worktree_removed(target) {
                    Some(true) => {
                        let elapsed = util::format_elapsed_duration(start.elapsed());
                        eprintln!("{}: merged ({})", name, elapsed);
                        reached.insert(name.clone());

                        if any {
                            return Ok(());
                        }
                    }
                    Some(false) => {
                        // Worktree exists but agent gone - crashed/exited unexpectedly
                        eprintln!("{}: agent exited unexpectedly", name);
                        std::process::exit(3);
                    }
                    // The remote cannot say yet; look again next time.
                    None => {}
                },
                // If we haven't seen an agent yet, still wait -- the agent may
                // not have started yet. The timeout flag handles the overall
                // deadline.
                Observation::NotStarted | Observation::Pending => {}
            }
        }

        // Check if all have reached target
        if reached.len() == targets.len() {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::RemoteAgentPane;

    fn remote_agent(worktree: &str, status: AgentStatus, stale: bool) -> AgentPane {
        let mut agent: AgentPane = serde_json::from_value(serde_json::json!({
            "session": "api",
            "window_name": "wm-feature",
            "pane_id": "%1",
            // The remote's directory: never looked up on the workstation.
            "path": "/remote/api__worktrees/elsewhere",
        }))
        .unwrap();
        agent.status = Some(status);
        agent.remote = Some(RemoteAgentPane {
            remote: "devbox".to_string(),
            project: "api".to_string(),
            worktree: worktree.to_string(),
            stale,
        });
        agent
    }

    fn feature() -> WaitTarget {
        WaitTarget::Remote {
            remote: "devbox".to_string(),
            project: "api".to_string(),
            worktree: "feature".to_string(),
        }
    }

    #[test]
    fn a_stale_remote_agent_neither_satisfies_nor_ends_a_wait() {
        let done = AgentStatus::Done;
        let stale = [remote_agent("feature", done, true)];
        assert_eq!(observe(&feature(), &stale, done, false), Observation::Stale);
        assert_eq!(observe(&feature(), &stale, done, true), Observation::Stale);

        let reconnected = [remote_agent("feature", done, false)];
        assert_eq!(
            observe(&feature(), &reconnected, done, true),
            Observation::Reached
        );
        let working = [remote_agent("feature", AgentStatus::Working, false)];
        assert_eq!(
            observe(&feature(), &working, done, true),
            Observation::Pending
        );
    }

    #[test]
    fn a_remote_target_matches_its_worktree_not_a_path() {
        let done = AgentStatus::Done;
        let other = [remote_agent("other", done, false)];
        assert_eq!(
            observe(&feature(), &other, done, false),
            Observation::NotStarted
        );
        assert_eq!(observe(&feature(), &other, done, true), Observation::Gone);
    }
}
