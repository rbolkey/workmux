//! Remote agent status travels as files (ADR 0002).
//!
//! On a remote, agent hooks write each remote pane's latest status to a file
//! under `<state dir>/status/<project>/<worktree>/<slot>.json`: the status,
//! the time the agent entered it, and a work generation counter incremented
//! on every `working`. Writing never touches the network.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::channel::Peer;
use super::pane::RemotePaneId;
use crate::multiplexer::AgentStatus;

/// Notification remote workmux sends with status records: every file on
/// connect, then each change.
pub const RECORDS_METHOD: &str = "status.records";

/// How often remote workmux looks for changed status files.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Serialize, Deserialize)]
pub struct Records {
    pub records: Vec<StatusRecord>,
}

/// Where the workstation hands the status records a remote sends.
pub type StatusSink = Arc<dyn Fn(&str, Vec<StatusRecord>) + Send + Sync>;

/// Send every status file over `peer`, then each change, until the channel
/// closes.
pub fn send_until_closed(peer: &Peer, files: &StatusFiles) {
    let mut sent: HashMap<RemotePaneId, StatusRecord> = HashMap::new();
    loop {
        let changed: Vec<StatusRecord> = files
            .read_all()
            .into_iter()
            .filter(|record| sent.get(&record.pane) != Some(record))
            .collect();
        if !changed.is_empty() {
            let params = serde_json::to_value(Records {
                records: changed.clone(),
            })
            .expect("status records serialize");
            if peer.notify(RECORDS_METHOD, params).is_err() {
                return;
            }
            sent.extend(changed.into_iter().map(|record| (record.pane.clone(), record)));
        }
        if peer.wait_closed(POLL_INTERVAL) {
            return;
        }
    }
}

/// The remote pane this process runs in, when it runs on a remote.
pub fn this_remote_pane() -> Option<RemotePaneId> {
    std::env::var(super::pane::PANE_ENV).ok()?.parse().ok()
}

/// Record a hook's `status` for `pane` in its status file. Remote workmux
/// sends the change on to the workstation.
pub fn record(pane: &RemotePaneId, status: Option<AgentStatus>) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    StatusFiles::for_this_remote()?.write(pane, status, std::env::current_dir().ok(), now)?;
    Ok(())
}

/// A remote pane's latest status, as its status file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    pub pane: RemotePaneId,
    /// `None` once the status is cleared.
    pub status: Option<AgentStatus>,
    /// Unix time the agent entered `status`, on the remote's clock.
    pub event_ts: u64,
    /// Incremented on every `working`: new work started.
    pub work_gen: u64,
    /// The agent's directory on the remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
}

/// The status files of this remote's panes.
pub struct StatusFiles {
    dir: PathBuf,
}

impl StatusFiles {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The status files under this remote's state dir.
    pub fn for_this_remote() -> Result<Self> {
        Ok(Self::new(crate::xdg::state_dir()?.join("status")))
    }

    fn path(&self, pane: &RemotePaneId) -> PathBuf {
        self.dir
            .join(&pane.project)
            .join(&pane.worktree)
            .join(format!("{}.json", pane.slot))
    }

    /// The status file of `pane`, if it has one.
    pub fn read(&self, pane: &RemotePaneId) -> Option<StatusRecord> {
        read_record(&self.path(pane))
    }

    /// Record that `pane`'s agent is now in `status` at `now`. A repeated
    /// status keeps the time the agent entered it, as the workstation's
    /// state store does.
    pub fn write(
        &self,
        pane: &RemotePaneId,
        status: Option<AgentStatus>,
        workdir: Option<PathBuf>,
        now: u64,
    ) -> Result<StatusRecord> {
        let previous = self.read(pane);
        let event_ts = match &previous {
            Some(previous) if previous.status == status => previous.event_ts,
            _ => now,
        };
        let work_gen = previous.as_ref().map_or(0, |previous| previous.work_gen)
            + u64::from(status == Some(AgentStatus::Working));
        let record = StatusRecord {
            pane: pane.clone(),
            status,
            event_ts,
            work_gen,
            workdir: workdir.or_else(|| previous.and_then(|previous| previous.workdir)),
        };
        let path = self.path(pane);
        let dir = path.parent().expect("a status file has a directory");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        // Written whole and renamed into place, so a reader never sees half.
        let temp = dir.join(format!(".{}.{}.tmp", pane.slot, std::process::id()));
        std::fs::write(&temp, serde_json::to_vec(&record)?)
            .with_context(|| format!("failed to write {}", temp.display()))?;
        std::fs::rename(&temp, &path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(record)
    }

    /// Remove `pane`'s status file: its slot now holds a new remote pane.
    pub fn remove(&self, pane: &RemotePaneId) {
        let _ = std::fs::remove_file(self.path(pane));
    }

    /// Every status file's record.
    pub fn read_all(&self) -> Vec<StatusRecord> {
        let mut records = Vec::new();
        for project in read_dir(&self.dir) {
            for worktree in read_dir(&project) {
                for file in read_dir(&worktree) {
                    if file.extension().is_some_and(|ext| ext == "json")
                        && let Some(record) = read_record(&file)
                    {
                        records.push(record);
                    }
                }
            }
        }
        records
    }
}

fn read_dir(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

fn read_record(path: &Path) -> Option<StatusRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(slot: u32) -> RemotePaneId {
        RemotePaneId {
            project: "api".to_string(),
            worktree: "feature".to_string(),
            slot,
        }
    }

    #[test]
    fn a_status_file_keeps_the_time_the_status_began_and_counts_work() {
        let temp = tempfile::tempdir().unwrap();
        let files = StatusFiles::new(temp.path());

        files
            .write(&pane(0), Some(AgentStatus::Working), None, 100)
            .unwrap();
        files
            .write(&pane(0), Some(AgentStatus::Working), None, 105)
            .unwrap();
        let working = files.read(&pane(0)).unwrap();
        assert_eq!((working.event_ts, working.work_gen), (100, 2));

        files
            .write(&pane(0), Some(AgentStatus::Done), None, 110)
            .unwrap();
        let done = files.read(&pane(0)).unwrap();
        assert_eq!(done.status, Some(AgentStatus::Done));
        assert_eq!((done.event_ts, done.work_gen), (110, 2));
        assert_eq!(files.read_all(), vec![done]);
    }
}
