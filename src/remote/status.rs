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
use crate::config::{Config, StatusIcons};
use crate::multiplexer::{AgentStatus, Multiplexer, TmuxBackend};
use crate::project::RemoteProjectId;
use crate::state::{AgentState, PaneKey, RemoteAgent, StateStore};

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
            sent.extend(
                changed
                    .into_iter()
                    .map(|record| (record.pane.clone(), record)),
            );
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
    let now = crate::util::now_unix_secs();
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
        Ok(super::pane::SessionHolder::for_this_remote()?.status_files())
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
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        // Written whole and renamed into place, so a reader never sees half.
        let temp = dir.join(format!(".{}.{}.tmp", pane.slot, std::process::id()));
        std::fs::write(&temp, serde_json::to_vec(&record)?)
            .with_context(|| format!("Failed to write {}", temp.display()))?;
        std::fs::rename(&temp, &path)
            .with_context(|| format!("Failed to write {}", path.display()))?;
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

/// Applies the status records remotes send to the workstation: the tmux
/// status options of the panes showing each remote pane, their state-store
/// entries, and snooze.
pub struct StatusApplier {
    pub tmux: TmuxBackend,
    pub store: StateStore,
    pub icons: StatusIcons,
    /// Whether workmux keeps the status icon in tmux's window status format.
    pub status_format: bool,
}

impl StatusApplier {
    /// Apply records to the tmux server at `socket` and the state store.
    pub fn for_tmux(socket: &str, config: &Config) -> Result<Self> {
        Ok(Self {
            tmux: TmuxBackend::for_socket(socket),
            store: StateStore::new()?,
            icons: config.status_icons.clone(),
            status_format: config.status_format.unwrap_or(true),
        })
    }

    /// Apply `remote`'s records to every workstation pane showing their
    /// remote panes. Records keep the times the remote gave them.
    pub fn apply(&self, remote: &str, records: &[StatusRecord]) {
        let views = match self.views() {
            Ok(views) => views,
            Err(error) => {
                tracing::warn!(remote, error = %format!("{error:#}"), "cannot apply remote status");
                return;
            }
        };
        let mut changed = false;
        for record in records {
            let tag = RemoteProjectId::new(remote, &record.pane.project).tag();
            for (pane_id, _) in views
                .iter()
                .filter(|(_, view)| view.0 == tag && view.1 == record.pane)
            {
                match self.apply_to_pane(remote, pane_id, record) {
                    Ok(applied) => changed |= applied,
                    Err(error) => tracing::warn!(
                        remote,
                        pane = %record.pane,
                        %pane_id,
                        error = %format!("{error:#}"),
                        "failed to apply remote status"
                    ),
                }
            }
        }
        if changed {
            crate::command::sidebar::request_refresh_for(&self.tmux);
        }
    }

    /// Workstation panes showing a remote pane: pane id to (window's
    /// project tag, remote pane).
    fn views(&self) -> Result<Vec<(String, (String, RemotePaneId))>> {
        let format = format!(
            "#{{pane_id}}\t#{{{}}}\t#{{{}}}",
            super::forwarded::WINDOW_TAG,
            super::forwarded::REMOTE_PANE_TAG
        );
        let output = self.tmux.query(&["list-panes", "-a", "-F", &format])?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let (pane_id, tag, pane) = (fields.next()?, fields.next()?, fields.next()?);
                Some((pane_id.to_string(), (tag.to_string(), pane.parse().ok()?)))
            })
            .collect())
    }

    /// Returns whether anything changed.
    fn apply_to_pane(&self, remote: &str, pane_id: &str, record: &StatusRecord) -> Result<bool> {
        let Some(live) = self.tmux.get_live_pane_info(pane_id)? else {
            return Ok(false);
        };
        let Some(pane_pid) = live.pid else {
            return Ok(false);
        };
        let boot_id = self.tmux.server_boot_id()?;
        let key = PaneKey {
            backend: self.tmux.name().to_string(),
            instance: self.tmux.instance_id(),
            pane_id: pane_id.to_string(),
        };
        let status_ts = record.status.map(|_| record.event_ts);
        self.store.with_agent_lock(|store| {
            // Only an entry for this same view of this remote pane carries over.
            let existing = store.get_agent(&key)?.filter(|state| {
                state.pane_pid == pane_pid
                    && state.boot_id == boot_id
                    && state
                        .remote
                        .as_ref()
                        .is_some_and(|known| known.remote == remote && known.pane == record.pane)
            });
            let previous_gen = existing
                .as_ref()
                .and_then(|state| state.remote.as_ref())
                .map(|known| known.work_gen);
            if let Some(existing) = &existing
                && existing.status == record.status
                && existing.status_ts == status_ts
                && previous_gen == Some(record.work_gen)
            {
                // Already applied: sent again on reconnect.
                return Ok(false);
            }

            if record.work_gen > previous_gen.unwrap_or(0)
                && let Err(error) = self.tmux.wake_agent(pane_id)
            {
                tracing::warn!(%pane_id, %error, "failed to clear agent sleep state");
            }
            match record.status {
                Some(status) => {
                    let (icon, auto_clear) = match status {
                        AgentStatus::Working => (self.icons.working(), false),
                        AgentStatus::Waiting => (self.icons.waiting(), true),
                        AgentStatus::Done => (self.icons.done(), true),
                    };
                    if self.status_format {
                        let _ = self.tmux.ensure_status_format(pane_id);
                    }
                    self.tmux.set_status(pane_id, icon, auto_clear)?;
                }
                None => self.tmux.clear_status(pane_id)?,
            }

            let now = crate::util::now_unix_secs();
            let activity_ts = match &existing {
                Some(existing) if existing.status == record.status => existing.activity_ts,
                _ => Some(record.event_ts),
            };
            let state = AgentState {
                pane_key: key.clone(),
                workdir: record.workdir.clone().unwrap_or(live.working_dir),
                status: record.status,
                status_ts,
                activity_ts,
                pane_title: live
                    .title
                    .or_else(|| existing.as_ref().and_then(|state| state.pane_title.clone())),
                pane_pid,
                command: live.current_command.unwrap_or_default(),
                updated_ts: now,
                window_name: live.window,
                session_name: live.session,
                boot_id,
                agent_kind: existing.as_ref().and_then(|state| state.agent_kind.clone()),
                agent_session_id: None,
                prompt: None,
                remote: Some(RemoteAgent {
                    remote: remote.to_string(),
                    pane: record.pane.clone(),
                    work_gen: record.work_gen,
                }),
            };
            store.upsert_agent_locked(&state)?;
            Ok(true)
        })
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

    use crate::multiplexer::Multiplexer;
    use crate::multiplexer::TmuxBackend;
    use crate::remote::forwarded::{REMOTE_PANE_TAG, WINDOW_TAG};
    use crate::state::StateStore;
    use crate::test_support::PrivateTmuxServer;

    fn record(slot: u32, status: AgentStatus, event_ts: u64, work_gen: u64) -> StatusRecord {
        StatusRecord {
            pane: pane(slot),
            status: Some(status),
            event_ts,
            work_gen,
            workdir: Some(PathBuf::from("/remote/api__worktrees/feature")),
        }
    }

    /// A workstation pane showing remote pane `slot`, in a window tagged
    /// for `remote`'s project `api`.
    fn remote_pane_view(server: &PrivateTmuxServer, remote: &str, slot: u32) -> String {
        let session = format!("{remote}-{slot}");
        let pane = server.new_session(&session, Path::new("/"));
        server.query(&[
            "set-option",
            "-w",
            "-t",
            &pane,
            WINDOW_TAG,
            &format!("{remote}:api"),
        ]);
        server.query(&[
            "set-option",
            "-p",
            "-t",
            &pane,
            REMOTE_PANE_TAG,
            &pane_name(slot),
        ]);
        pane
    }

    fn pane_name(slot: u32) -> String {
        pane(slot).to_string()
    }

    fn applier(server: &PrivateTmuxServer, state: &Path) -> StatusApplier {
        StatusApplier {
            tmux: TmuxBackend::for_socket(&server.socket),
            store: StateStore::with_path(state.to_path_buf()).unwrap(),
            icons: crate::config::StatusIcons::default(),
            status_format: false,
        }
    }

    fn stored(applier: &StatusApplier, pane_id: &str) -> crate::state::AgentState {
        applier
            .store
            .get_agent(&crate::state::PaneKey {
                backend: "tmux".to_string(),
                instance: applier.tmux.instance_id(),
                pane_id: pane_id.to_string(),
            })
            .unwrap()
            .expect("a state-store entry")
    }

    #[test]
    fn a_record_reaches_the_workstation_pane_showing_its_remote_pane() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let shown = remote_pane_view(&server, "devbox", 0);
        // Another remote's pane with the same remote pane name.
        let other = remote_pane_view(&server, "other", 0);
        let applier = applier(&server, temp.path());

        applier.apply("devbox", &[record(0, AgentStatus::Waiting, 100, 1)]);

        let state = stored(&applier, &shown);
        assert_eq!(state.status, Some(AgentStatus::Waiting));
        assert_eq!(state.status_ts, Some(100));
        let remote = state.remote.unwrap();
        assert_eq!((remote.remote.as_str(), remote.pane), ("devbox", pane(0)));
        assert_eq!(
            server.query(&["show-options", "-pqv", "-t", &shown, "@workmux_pane_status"]),
            crate::config::StatusIcons::default().waiting()
        );
        assert!(
            applier
                .store
                .get_agent(&crate::state::PaneKey {
                    backend: "tmux".to_string(),
                    instance: applier.tmux.instance_id(),
                    pane_id: other.clone(),
                })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn statuses_from_a_disconnect_keep_their_times_and_order_last_done() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let first = remote_pane_view(&server, "devbox", 0);
        let second = remote_pane_view(&server, "devbox", 1);
        let applier = applier(&server, temp.path());
        applier.apply(
            "devbox",
            &[
                record(0, AgentStatus::Working, 100, 1),
                record(1, AgentStatus::Working, 100, 1),
            ],
        );

        // Replayed on reconnect: pane 1 finished at 130, pane 0 at 140.
        applier.apply(
            "devbox",
            &[
                record(1, AgentStatus::Done, 130, 2),
                record(0, AgentStatus::Done, 140, 2),
            ],
        );

        assert_eq!(stored(&applier, &first).status_ts, Some(140));
        assert_eq!(stored(&applier, &second).status_ts, Some(130));
        let mut done = applier.store.list_all_agents().unwrap();
        crate::command::last_done::sort_by_recency(&mut done);
        let order: Vec<_> = done.iter().map(|a| a.pane_key.pane_id.clone()).collect();
        assert_eq!(order, [first, second]);
    }

    #[test]
    fn a_snoozed_agent_wakes_only_when_new_work_started_on_the_remote() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let shown = remote_pane_view(&server, "devbox", 0);
        let applier = applier(&server, temp.path());
        applier.apply("devbox", &[record(0, AgentStatus::Done, 100, 1)]);
        server.query(&["set-option", "-g", "@workmux_sleeping_panes", &shown]);
        let sleeping = || server.query(&["show-option", "-gqv", "@workmux_sleeping_panes"]);

        // The same work, sent again on reconnect.
        applier.apply("devbox", &[record(0, AgentStatus::Done, 100, 1)]);
        assert_eq!(sleeping(), shown);

        // Working, then done again, while disconnected.
        applier.apply("devbox", &[record(0, AgentStatus::Done, 150, 2)]);
        assert_eq!(sleeping(), "");
        assert_eq!(stored(&applier, &shown).status_ts, Some(150));
    }

    #[test]
    fn a_remote_agent_survives_its_views_foreground_command_changing() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let shown = remote_pane_view(&server, "devbox", 0);
        let applier = applier(&server, temp.path());
        applier.apply("devbox", &[record(0, AgentStatus::Working, 100, 1)]);
        // The view's foreground moves between the attach client and the
        // reconnecting screen; the agent runs on the remote throughout.
        let mut state = stored(&applier, &shown);
        state.command = "dtach".to_string();
        applier.store.upsert_agent(&state).unwrap();

        let agents = applier.store.load_reconciled_agents(&applier.tmux).unwrap();

        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].project(),
            crate::project::Project::remote("devbox", "api")
        );
    }
}
