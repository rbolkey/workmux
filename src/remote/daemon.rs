//! The workstation daemon's remote work: remote agents' status, remote
//! worktrees' git status, and which remotes are connected. The daemon's
//! refresh loop calls in here and keeps its own timings.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::git::GitStatus;
use crate::multiplexer::AgentPane;

/// Git status by `AgentPane::git_key`, shared with the worker that fills it.
pub type GitStatuses = Arc<Mutex<HashMap<PathBuf, GitStatus>>>;

/// Have remote agents' status, which arrives over the control channels this
/// daemon holds (ADR 0002), applied to the tmux server `instance_id`.
pub fn install_status_sink(instance_id: &str, config: &Config) {
    match super::status::StatusApplier::for_tmux(instance_id, config) {
        Ok(applier) => super::channels().set_status_sink(Arc::new(
            move |remote: &str, records: Vec<super::status::StatusRecord>| {
                applier.apply(remote, &records)
            },
        )),
        Err(error) => tracing::warn!(%error, "remote agent status unavailable"),
    }
}

/// Mark the agents of remotes whose control channel is down stale: shown
/// with their last known status.
pub fn mark_stale_agents(agents: &mut [AgentPane]) {
    let connected = super::channels().connected_remotes();
    for remote in agents.iter_mut().filter_map(|agent| agent.remote.as_mut()) {
        remote.stale = !connected.contains(&remote.remote);
    }
}

/// Remotes whose agents among `agents` are not stale.
pub fn connected_remotes(agents: &[AgentPane]) -> HashSet<String> {
    agents
        .iter()
        .filter_map(|agent| agent.remote.as_ref())
        .filter(|remote| !remote.stale)
        .map(|remote| remote.remote.clone())
        .collect()
}

/// Close idle control channels. Returns whether any is still open: an open
/// channel keeps the daemon alive.
pub fn close_idle_channels() -> bool {
    let channels = super::channels();
    channels.close_idle();
    channels.open_count() > 0
}

/// A remote agent's worktree, whose git status remote workmux reports.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RemoteGitPath {
    /// The agent's `AgentPane::git_key`.
    key: PathBuf,
    remote: String,
    project: String,
    /// The worktree's path on the remote.
    worktree: PathBuf,
}

impl RemoteGitPath {
    fn for_agent(agent: &AgentPane) -> Option<Self> {
        let remote = agent.remote.as_ref()?;
        Some(Self {
            key: agent.git_key(),
            remote: remote.remote.clone(),
            project: remote.project.clone(),
            worktree: agent.path.clone(),
        })
    }
}

/// How often remote worktrees' git status is asked for: there is no
/// filesystem to watch on a remote.
const REMOTE_GIT_INTERVAL: Duration = Duration::from_secs(5);

/// Whether two git statuses look the same to whoever shows them.
pub type SameGitStatus = fn(&GitStatus, &GitStatus) -> bool;

/// A background worker keeping remote worktrees' git status.
pub struct RemoteGit {
    statuses: GitStatuses,
    worktrees: Sender<Vec<RemoteGitPath>>,
}

impl RemoteGit {
    /// Start the worker. When the statuses change it sets `dirty_flag` and
    /// wakes the daemon through `wake_tx`; `same` tells a change apart.
    pub fn spawn(
        term: Arc<AtomicBool>,
        dirty_flag: Arc<AtomicBool>,
        wake_tx: SyncSender<()>,
        same: SameGitStatus,
    ) -> Self {
        let statuses: GitStatuses = Arc::new(Mutex::new(HashMap::new()));
        let worker_statuses = statuses.clone();
        let (worktrees, rx) = std::sync::mpsc::channel::<Vec<RemoteGitPath>>();
        thread::spawn(move || run_worker(rx, &worker_statuses, &term, &dirty_flag, &wake_tx, same));
        Self {
            statuses,
            worktrees,
        }
    }

    /// The remote worktrees' git status the worker last learned.
    pub fn statuses(&self) -> &GitStatuses {
        &self.statuses
    }

    /// Keep the git status of the remote agents among `agents`, and only
    /// theirs: workstation git never runs on a remote agent's path.
    pub fn follow<'a>(&self, agents: impl IntoIterator<Item = &'a AgentPane>) {
        let entries = agents
            .into_iter()
            .filter_map(RemoteGitPath::for_agent)
            .collect();
        let _ = self.worktrees.send(entries);
    }
}

fn run_worker(
    rx: Receiver<Vec<RemoteGitPath>>,
    statuses: &GitStatuses,
    term: &AtomicBool,
    dirty_flag: &AtomicBool,
    wake_tx: &SyncSender<()>,
    same: SameGitStatus,
) {
    let mut entries: Vec<RemoteGitPath> = Vec::new();
    let mut last_refresh = Instant::now() - REMOTE_GIT_INTERVAL;
    while !term.load(Ordering::Relaxed) {
        let mut changed_entries = false;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(mut latest) => {
                while let Ok(newer) = rx.try_recv() {
                    latest = newer;
                }
                latest.sort();
                latest.dedup();
                changed_entries = latest != entries;
                entries = latest;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if !changed_entries && last_refresh.elapsed() < REMOTE_GIT_INTERVAL {
            continue;
        }
        last_refresh = Instant::now();
        if refresh_remote_git_statuses(&entries, statuses, same, &mut ask_remote_git_status) {
            dirty_flag.store(true, Ordering::Relaxed);
            let _ = wake_tx.try_send(());
        }
    }
}

/// Ask for each remote worktree's git status through `ask`, keeping the
/// last known status of one that cannot be reached and dropping worktrees
/// no longer shown. Returns whether the cache changed.
fn refresh_remote_git_statuses(
    entries: &[RemoteGitPath],
    cache: &GitStatuses,
    same: SameGitStatus,
    ask: &mut dyn FnMut(&RemoteGitPath) -> Result<GitStatus>,
) -> bool {
    let fetched: Vec<(PathBuf, GitStatus)> = entries
        .iter()
        .filter_map(|entry| match ask(entry) {
            Ok(status) => Some((entry.key.clone(), status)),
            Err(error) => {
                tracing::debug!(remote = %entry.remote, error = %format!("{error:#}"), "remote git status unavailable");
                None
            }
        })
        .collect();
    let Ok(mut cache) = cache.lock() else {
        return false;
    };
    let before = cache.len();
    cache.retain(|key, _| entries.iter().any(|entry| entry.key == *key));
    let mut changed = cache.len() != before;
    for (key, status) in fetched {
        if cache.get(&key).is_some_and(|old| same(old, &status)) {
            continue;
        }
        cache.insert(key, status);
        changed = true;
    }
    changed
}

/// Remote workmux's git status for `entry`, over its remote's control
/// channel only when it is already open: a disconnected or stopped remote
/// is never dialled for this.
fn ask_remote_git_status(entry: &RemoteGitPath) -> Result<GitStatus> {
    let project = super::router::remote_project_on(&entry.remote, &entry.project)?;
    let answer = super::channels().dashboard(
        project.host(),
        &project.site,
        &super::dashboard::DashboardRequest::GitStatus {
            worktree: entry.worktree.clone(),
        },
        false,
    )?;
    Ok(serde_json::from_value(answer)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn remote_agent(path: &Path) -> AgentPane {
        AgentPane {
            session: String::new(),
            window_name: String::new(),
            pane_id: "%7".to_string(),
            window_id: String::new(),
            window_index: None,
            path: path.to_path_buf(),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: None,
            remote: Some(crate::multiplexer::RemoteAgentPane {
                remote: "devbox".to_string(),
                project: "api".to_string(),
                worktree: "feature".to_string(),
                stale: false,
            }),
        }
    }

    fn status_on(branch: &str) -> GitStatus {
        GitStatus {
            branch: Some(branch.to_string()),
            ..GitStatus::default()
        }
    }

    #[test]
    fn remote_git_status_keeps_the_last_known_while_unreachable_and_drops_gone_worktrees() {
        let cache: GitStatuses = Arc::new(Mutex::new(HashMap::new()));
        let shown = RemoteGitPath::for_agent(&remote_agent(Path::new("/w/feature"))).unwrap();
        let gone = RemoteGitPath::for_agent(&remote_agent(Path::new("/w/gone"))).unwrap();
        let both = [shown.clone(), gone];
        let same: SameGitStatus = |a, b| a == b;

        assert!(refresh_remote_git_statuses(
            &both,
            &cache,
            same,
            &mut |_| Ok(status_on("feature"))
        ));
        let unchanged = refresh_remote_git_statuses(&both, &cache, same, &mut |_| {
            Err(anyhow::anyhow!("remote devbox is not connected"))
        });
        let dropped =
            refresh_remote_git_statuses(std::slice::from_ref(&shown), &cache, same, &mut |_| {
                Err(anyhow::anyhow!("remote devbox is not connected"))
            });

        assert!(!unchanged);
        assert!(dropped);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[&shown.key], status_on("feature"));
    }

    #[test]
    fn a_remote_agents_git_entry_never_uses_its_path() {
        // The remote worktree's path is also a checkout on the workstation.
        let temp = tempfile::tempdir().unwrap();
        crate::test_support::init_repo(temp.path());
        let agent = remote_agent(temp.path());

        let git = RemoteGitPath::for_agent(&agent).unwrap();

        assert_ne!(git.key, temp.path());
        assert_eq!(git.key, agent.git_key());
        assert_eq!(git.worktree, temp.path());
        let mut local = agent.clone();
        local.remote = None;
        assert!(RemoteGitPath::for_agent(&local).is_none());
    }
}
