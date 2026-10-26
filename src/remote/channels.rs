//! The control channels the workstation daemon holds, one per remote.
//!
//! A channel opens when something needs its remote, stays open while it is
//! in use, and reconnects with backoff after a drop. A remote whose
//! transport reports it stopped is never started to fetch status.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::channel::Handler;
use super::connection::{Connection, HELLO_TIMEOUT};
use super::dashboard::{self, DashboardRequest};
use super::forwarded;
use super::forwarding::{FORWARD_METHOD, Forwarded};
use super::pane::RemotePaneId;
use super::router::RemoteProject;
use super::session::{self, CommandRequest, ProjectSite, SessionParams, Sessions, Terminal};
use super::status::{self, StatusSink};
use super::transport::{RemoteState, Transport};
use super::{Remote, shell, shims};
use crate::config::{RemoteConfig, TransportKind};
use crate::multiplexer::{InvocationContext, TmuxBackend};
use crate::project::{ProjectTag, RemoteProjectId};

/// Builds the transport for a configured remote.
pub type TransportFactory =
    Arc<dyn Fn(&str, &RemoteConfig) -> Result<Arc<dyn Transport>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ChannelPolicy {
    pub hello_timeout: Duration,
    /// First reconnect delay after a drop; doubles per failed attempt.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// How long an unused channel stays open (and is reconnected).
    pub linger: Duration,
    /// The workstation workmux that panes run `remote attach` with.
    pub workmux: PathBuf,
    /// The workstation's tmux server (its socket path); `None` for tmux's
    /// default.
    pub tmux_socket: Option<String>,
    /// Whether a tagged window's clone still puts its project on its
    /// remote, by the clone's config now.
    pub still_tagged: fn(tag: &ProjectTag) -> bool,
    /// Reads the workstation clipboard's PNG image, for clipboard shims.
    pub clipboard: fn() -> Result<Option<Vec<u8>>>,
}

impl Default for ChannelPolicy {
    fn default() -> Self {
        Self {
            hello_timeout: HELLO_TIMEOUT,
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            linger: Duration::from_secs(60),
            workmux: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("workmux")),
            tmux_socket: InvocationContext::current()
                .tmux_socket_path()
                .map(str::to_string),
            still_tagged,
            clipboard: crate::sandbox::clipboard::read_png_from_clipboard,
        }
    }
}

fn still_tagged(tag: &ProjectTag) -> bool {
    matches!(super::resolve::tagged(tag), Ok(Some(_)))
}

/// How long a remote may take to install its shims.
const SHIMS_TIMEOUT: Duration = Duration::from_secs(10);

/// A remote's reachability as `workmux remote status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub name: String,
    pub transport: TransportKind,
    pub target: String,
    #[serde(flatten)]
    pub reach: Reach,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Reach {
    Reachable {
        workmux: String,
        protocol: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
        /// Problems remote workmux reports with the remote's setup.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    /// The transport reports the remote stopped; not started.
    Stopped,
    /// The transport reports no remote yet; not created.
    Missing,
    Unreachable {
        error: String,
    },
}

/// Answers requests remote workmux sends over its control channel.
struct WorkstationHandler {
    /// The remote at the other end, which every request is scoped to.
    remote: String,
    workmux: PathBuf,
    sessions: Arc<Sessions>,
    /// The remote's settings, which its shims are checked against.
    config: RemoteConfig,
    tmux_socket: Option<String>,
    still_tagged: fn(&ProjectTag) -> bool,
    clipboard: fn() -> Result<Option<Vec<u8>>>,
    status: Arc<Mutex<Option<StatusSink>>>,
}

impl WorkstationHandler {
    /// The scope of `session`, which must be open on this remote.
    fn scope(&self, session: u64) -> Result<forwarded::SessionScope> {
        self.sessions
            .scope(session)
            .filter(|scope| scope.remote == self.remote)
            .ok_or_else(|| {
                anyhow!(
                    "Refused: no command session {session} on remote {}",
                    self.remote
                )
            })
    }

    fn forwarded(&self, params: Value) -> Result<Value> {
        let Forwarded { session, request } = serde_json::from_value(params)?;
        let scope = self.scope(session)?;
        forwarded::answer(&scope, &scope.tmux(), &self.workmux, request)
    }

    /// Open a shell session for a remote pane of a project on this remote,
    /// invoked from the workstation pane showing it, if one does.
    fn open_shell(&self, params: Value) -> Result<Value> {
        let shell::OpenParams { pane } = serde_json::from_value(params)?;
        let (invocation, tag) = self.viewing(&pane);
        let Some(tag) = tag.filter(|tag| (self.still_tagged)(tag)) else {
            bail!(
                "Refused: project {} is not on remote {}",
                pane.project,
                self.remote
            );
        };
        let session = self.sessions.open_shell(forwarded::SessionScope {
            remote: self.remote.clone(),
            project: pane.project,
            root: tag.root,
            invocation,
        });
        Ok(serde_json::to_value(shell::Opened { session })?)
    }

    /// The invocation context of a command run in remote pane `pane`'s
    /// shell: the workstation pane in one of the project's windows showing
    /// it, else none in the workstation's tmux. With it, the tag of a window
    /// of the project, preferring the one showing the pane.
    fn viewing(&self, pane: &RemotePaneId) -> (InvocationContext, Option<ProjectTag>) {
        let socket = self.tmux_socket.as_deref().unwrap_or("default");
        let id = RemoteProjectId::new(&self.remote, &pane.project);
        let listing = TmuxBackend::for_socket(socket)
            .query(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{@workmux_remote_pane}\t#{@workmux_project_window}\t#{socket_path},#{pid},#{session_id}",
            ])
            .unwrap_or_default();
        let shown = pane.to_string();
        let panes: Vec<_> = listing
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let (pane_id, remote_pane, tag, client) = (
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                );
                let tag = ProjectTag::parse(tag).filter(|tag| tag.id == id)?;
                Some((pane_id, remote_pane, tag, client))
            })
            .collect();
        let showing = panes
            .iter()
            .find(|(_, remote_pane, _, _)| *remote_pane == shown);
        let tag = showing.or(panes.first()).map(|(_, _, tag, _)| tag.clone());
        let invocation = showing
            .map(|(pane_id, _, _, client)| InvocationContext {
                pane_id: Some(pane_id.to_string()),
                tmux: Some(client.replace('$', "")),
                cwd: None,
            })
            .unwrap_or_else(|| InvocationContext {
                tmux: Some(socket.to_string()),
                ..InvocationContext::default()
            });
        (invocation, tag)
    }

    fn prompt(&self, params: Value) -> Result<Value> {
        let SessionParams { session } = serde_json::from_value(params)?;
        self.scope(session)?;
        let line = self.sessions.prompt(session)?;
        Ok(serde_json::to_value(session::Answer { line })?)
    }
}

impl Handler for WorkstationHandler {
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            FORWARD_METHOD => self.forwarded(params).map_err(|error| format!("{error:#}")),
            session::PROMPT_METHOD => self.prompt(params).map_err(|error| format!("{error:#}")),
            shell::OPEN_METHOD => self
                .open_shell(params)
                .map_err(|error| format!("{error:#}")),
            shims::CLIPBOARD_METHOD => {
                shims::answer_clipboard(&self.remote, &self.config, self.clipboard, params)
                    .map_err(|error| format!("{error:#}"))
            }
            shims::EXEC_METHOD => shims::answer_exec(&self.remote, &self.config, params)
                .map_err(|error| format!("{error:#}")),
            shell::RELAY_METHOD => shell::answer_relay(
                &self.remote,
                &self.workmux,
                self.tmux_socket.as_deref(),
                params,
            )
            .map_err(|error| format!("{error:#}")),
            method => Err(format!("unknown method: {method}")),
        }
    }

    fn notification(&self, method: &str, params: Value) {
        match method {
            session::OUTPUT_METHOD => self.sessions.output(params),
            shell::CLOSE_METHOD => match serde_json::from_value::<SessionParams>(params) {
                Ok(params) => self.sessions.close_shell(params.session, &self.remote),
                Err(error) => tracing::warn!(%error, "ignoring an unreadable shell close"),
            },
            status::RECORDS_METHOD => match serde_json::from_value::<status::Records>(params) {
                Ok(status::Records { records }) => {
                    let sink = self.status.lock().unwrap().clone();
                    if let Some(sink) = sink {
                        sink(&self.remote, records);
                    }
                }
                Err(error) => tracing::warn!(%error, "ignoring unreadable status records"),
            },
            method => tracing::warn!(method, "ignoring unknown notification"),
        }
    }
}

struct Slot {
    config: RemoteConfig,
    connection: Option<Arc<Connection>>,
    last_used: Instant,
    /// Command sessions running on this remote and workstation panes
    /// attached to its remote panes; the channel is wanted while any is.
    holds: usize,
    /// Set while a background reconnect loop runs for this remote.
    reconnecting: bool,
}

struct Shared {
    factory: TransportFactory,
    policy: ChannelPolicy,
    slots: Mutex<HashMap<String, Slot>>,
    /// Serializes connection attempts to each remote, so a burst of
    /// requests opens one channel per remote while a remote that hangs
    /// holds up no other.
    connecting: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    sessions: Arc<Sessions>,
    /// Receives the status records remotes send.
    status: Arc<Mutex<Option<StatusSink>>>,
}

/// Keeps a remote's channel wanted while a workstation pane is attached to
/// one of its remote panes.
pub struct PaneHold {
    channels: Channels,
    remote: String,
}

impl Drop for PaneHold {
    fn drop(&mut self) {
        self.channels.hold(&self.remote, -1);
    }
}

/// The workstation daemon's control channels.
#[derive(Clone)]
pub struct Channels {
    shared: Arc<Shared>,
}

impl Channels {
    pub fn new(factory: TransportFactory, policy: ChannelPolicy) -> Self {
        Self {
            shared: Arc::new(Shared {
                factory,
                policy,
                slots: Mutex::new(HashMap::new()),
                connecting: Mutex::new(HashMap::new()),
                sessions: Arc::new(Sessions::default()),
                status: Arc::new(Mutex::new(None)),
            }),
        }
    }

    /// Hand the status records every remote sends to `sink`.
    pub fn set_status_sink(&self, sink: StatusSink) {
        *self.shared.status.lock().unwrap() = Some(sink);
    }

    /// Report `remote`'s reachability, opening its channel if needed.
    pub fn status(&self, remote: Remote<'_>) -> RemoteStatus {
        RemoteStatus {
            name: remote.name.to_string(),
            transport: remote.config.transport,
            target: remote.config.target.clone(),
            reach: self.reach(remote),
        }
    }

    fn reach(&self, remote: Remote<'_>) -> Reach {
        let unreachable = |error: anyhow::Error| Reach::Unreachable {
            error: format!("{error:#}"),
        };
        let connection = match self.open_channel_for(remote, true) {
            Some(connection) => connection,
            None => {
                let transport = match (self.shared.factory)(remote.name, remote.config) {
                    Ok(transport) => transport,
                    Err(error) => return unreachable(error),
                };
                if let Some(provisioning) = transport.provisioning() {
                    match provisioning.status() {
                        Ok(RemoteState::Running) => {}
                        Ok(RemoteState::Stopped) => return Reach::Stopped,
                        Ok(RemoteState::Missing) => return Reach::Missing,
                        Err(error) => return unreachable(error),
                    }
                }
                match self.channel_with(remote, transport, true) {
                    Ok(connection) => connection,
                    Err(error) => return unreachable(error),
                }
            }
        };
        match connection.ping() {
            Ok(ping) => Reach::Reachable {
                workmux: ping.workmux,
                protocol: ping.protocol,
                warning: connection.warning.clone(),
                warnings: ping.warnings,
            },
            Err(error) => unreachable(error),
        }
    }

    /// The open channel to `remote`, opened on demand.
    pub fn channel(&self, remote: Remote<'_>) -> Result<Arc<Connection>> {
        self.channel_for(remote, true)
    }

    /// `use_now` marks the channel used; a background reconnect does not, so
    /// it never extends the linger period by itself.
    fn channel_for(&self, remote: Remote<'_>, use_now: bool) -> Result<Arc<Connection>> {
        self.channel_checked(remote, use_now, false)
    }

    /// Like [`Self::channel_for`]; `running` says the caller has just found
    /// the remote running, so its state is not checked again.
    fn channel_checked(
        &self,
        remote: Remote<'_>,
        use_now: bool,
        running: bool,
    ) -> Result<Arc<Connection>> {
        // An open channel needs no word from the transport: a remote that
        // stopped has closed it.
        if let Some(connection) = self.open_channel_for(remote, use_now) {
            return Ok(connection);
        }
        let transport = (self.shared.factory)(remote.name, remote.config)?;
        if !running && let Some(provisioning) = transport.provisioning() {
            match provisioning.status()? {
                RemoteState::Running => {}
                RemoteState::Stopped => return Err(anyhow!("Remote {} is stopped", remote.name)),
                RemoteState::Missing => {
                    return Err(anyhow!("Remote {} does not exist yet", remote.name));
                }
            }
        }
        self.channel_with(remote, transport, use_now)
    }

    fn channel_with(
        &self,
        remote: Remote<'_>,
        transport: Arc<dyn Transport>,
        use_now: bool,
    ) -> Result<Arc<Connection>> {
        let Remote { name, config } = remote;
        let lock = self
            .shared
            .connecting
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone();
        let _connecting = lock.lock().unwrap();
        if let Some(connection) = self.open_channel_for(remote, use_now) {
            return Ok(connection);
        }
        if let Some(slot) = self.shared.slots.lock().unwrap().get_mut(name)
            && slot.config != *config
        {
            // Re-registered under different settings: drop the old channel.
            slot.connection = None;
            slot.config = config.clone();
        }

        let connection = Arc::new(Connection::open(
            name,
            transport.as_ref(),
            WorkstationHandler {
                remote: name.to_string(),
                workmux: self.shared.policy.workmux.clone(),
                sessions: self.shared.sessions.clone(),
                config: config.clone(),
                tmux_socket: self.shared.policy.tmux_socket.clone(),
                still_tagged: self.shared.policy.still_tagged,
                clipboard: self.shared.policy.clipboard,
                status: self.shared.status.clone(),
            },
            self.shared.policy.hello_timeout,
        )?);
        tracing::info!(remote = name, workmux = %connection.hello.workmux, "control channel open");
        let install = shims::Install {
            commands: shims::allowed(config),
        };
        if let Err(error) = connection.peer.request(
            shims::INSTALL_METHOD,
            serde_json::to_value(install)?,
            SHIMS_TIMEOUT,
        ) {
            tracing::warn!(remote = name, error = %format!("{error:#}"), "failed to install shims");
        }
        let mut slots = self.shared.slots.lock().unwrap();
        let slot = slots.entry(name.to_string()).or_insert_with(|| Slot {
            config: config.clone(),
            connection: None,
            last_used: Instant::now(),
            holds: 0,
            reconnecting: false,
        });
        slot.connection = Some(connection.clone());
        if use_now {
            slot.last_used = Instant::now();
        }
        drop(slots);
        self.watch(name, &connection);
        Ok(connection)
    }

    /// Reconnect `name` in the background once its channel drops.
    fn watch(&self, name: &str, connection: &Arc<Connection>) {
        let shared = Arc::downgrade(&self.shared);
        let watched = Arc::downgrade(connection);
        let peer = connection.peer.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            while !peer.wait_closed(Duration::from_secs(3600)) {}
            let Some(shared) = shared.upgrade() else {
                return;
            };
            Channels { shared }.reconnect_after_drop(&name, &watched);
        });
    }

    fn reconnect_after_drop(&self, name: &str, watched: &Weak<Connection>) {
        let dropped = {
            let mut slots = self.shared.slots.lock().unwrap();
            let Some(slot) = slots.get_mut(name) else {
                return;
            };
            // Closed on purpose (idle, re-registered) rather than dropped.
            if !slot
                .connection
                .as_ref()
                .is_some_and(|current| Arc::as_ptr(current) == watched.as_ptr())
            {
                return;
            }
            let dropped = slot.connection.take();
            if slot.reconnecting || !self.wanted(slot) {
                None
            } else {
                slot.reconnecting = true;
                Some((dropped, slot.config.clone()))
            }
        };
        let Some((dropped, config)) = dropped else {
            tracing::warn!(remote = name, "control channel dropped");
            return;
        };
        if let Some(connection) = dropped {
            tracing::warn!(
                remote = name,
                reason = connection.failure(),
                "control channel dropped"
            );
        }

        let mut backoff = self.shared.policy.backoff_initial;
        loop {
            std::thread::sleep(backoff);
            {
                let slots = self.shared.slots.lock().unwrap();
                let Some(slot) = slots.get(name) else {
                    break;
                };
                if slot.connection.is_some() || !self.wanted(slot) || slot.config != config {
                    break;
                }
            }
            match self.channel_for(Remote::new(name, &config), false) {
                Ok(_) => break,
                Err(error) => {
                    tracing::info!(remote = name, error = %format!("{error:#}"), "control channel reconnect failed");
                    backoff = (backoff * 2).min(self.shared.policy.backoff_max);
                }
            }
        }
        if let Some(slot) = self.shared.slots.lock().unwrap().get_mut(name) {
            slot.reconnecting = false;
        }
    }

    fn wanted(&self, slot: &Slot) -> bool {
        slot.holds > 0 || slot.last_used.elapsed() < self.shared.policy.linger
    }

    /// Run a command session on `remote`, passing its output to `terminal`
    /// as it arrives and reading its prompts' answers there. Multiplexer
    /// requests the command forwards are answered from `invocation`.
    /// `running` says the caller has just found the remote running.
    /// Returns the command's exit code.
    pub fn run_command(
        &self,
        project: &RemoteProject,
        request: &CommandRequest,
        invocation: &InvocationContext,
        terminal: &mut dyn Terminal,
        running: bool,
    ) -> Result<i32> {
        let remote = project.host();
        let connection = self.channel_checked(remote, true, running)?;
        self.hold(remote.name, 1);
        let exit = self.shared.sessions.run(
            &connection.peer,
            &project.root,
            request,
            invocation,
            terminal,
        );
        self.hold(remote.name, -1);
        exit
    }

    /// Run a dashboard operation on `project`, a project of `remote`.
    /// Without `connect` only an already open channel is used.
    pub fn dashboard(
        &self,
        remote: Remote<'_>,
        project: &ProjectSite,
        request: &DashboardRequest,
        connect: bool,
    ) -> Result<Value> {
        let connection = if connect {
            self.channel(remote)?
        } else {
            self.open_channel(remote.name)
                .ok_or_else(|| anyhow!("Remote {} is not connected", remote.name))?
        };
        connection.peer.request(
            dashboard::METHOD,
            dashboard::params(project, request),
            dashboard::TIMEOUT,
        )
    }

    /// `remote`'s channel, when it is open under its current settings;
    /// `use_now` marks it used.
    fn open_channel_for(&self, remote: Remote<'_>, use_now: bool) -> Option<Arc<Connection>> {
        let mut slots = self.shared.slots.lock().unwrap();
        let slot = slots
            .get_mut(remote.name)
            .filter(|slot| slot.config == *remote.config)?;
        let connection = slot
            .connection
            .clone()
            .filter(|connection| !connection.is_closed())?;
        if use_now {
            slot.last_used = Instant::now();
        }
        Some(connection)
    }

    /// `name`'s channel, when it is open.
    fn open_channel(&self, name: &str) -> Option<Arc<Connection>> {
        self.shared
            .slots
            .lock()
            .unwrap()
            .get(name)?
            .connection
            .clone()
            .filter(|connection| !connection.is_closed())
    }

    /// Keep `remote`'s channel open, and reconnecting after drops, while the
    /// returned hold lives: a workstation pane is attached to one of its
    /// remote panes.
    pub fn hold_for_pane(&self, remote: Remote<'_>) -> Result<PaneHold> {
        self.channel(remote)?;
        self.hold(remote.name, 1);
        Ok(PaneHold {
            channels: self.clone(),
            remote: remote.name.to_string(),
        })
    }

    fn hold(&self, name: &str, change: isize) {
        if let Some(slot) = self.shared.slots.lock().unwrap().get_mut(name) {
            slot.holds = slot.holds.saturating_add_signed(change);
            slot.last_used = Instant::now();
        }
    }

    /// Close channels that have gone unused for the linger period.
    pub fn close_idle(&self) {
        let mut slots = self.shared.slots.lock().unwrap();
        for (name, slot) in slots.iter_mut() {
            if slot.connection.is_some() && !self.wanted(slot) {
                tracing::info!(remote = name, "closing idle control channel");
                slot.connection = None;
            }
        }
    }

    /// Remotes whose control channel is open. Every other remote's agents
    /// are stale.
    pub fn connected_remotes(&self) -> std::collections::HashSet<String> {
        self.shared
            .slots
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, slot)| slot.connection.as_ref().is_some_and(|c| !c.is_closed()))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Number of open channels.
    pub fn open_count(&self) -> usize {
        self.shared
            .slots
            .lock()
            .unwrap()
            .values()
            .filter(|slot| slot.connection.as_ref().is_some_and(|c| !c.is_closed()))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::handshake::{PROTOCOL_VERSION, SENTINEL};
    use crate::remote::testing::{ScriptTransport, ServeTransport, StoppedTransport};
    use std::sync::atomic::Ordering;

    fn config() -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Loopback,
            target: "/remote".to_string(),
            builtin_shims: None,
            host_commands: vec![],
            openshell: None,
        }
    }

    fn channels_over<T: Transport + 'static>(transport: Arc<T>, policy: ChannelPolicy) -> Channels {
        Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            policy,
        )
    }

    fn fast_policy() -> ChannelPolicy {
        ChannelPolicy {
            hello_timeout: Duration::from_secs(10),
            backoff_initial: Duration::from_millis(50),
            backoff_max: Duration::from_millis(400),
            linger: Duration::from_secs(60),
            ..ChannelPolicy::default()
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn hello_then(after: &str) -> String {
        format!(
            "printf '\\n{SENTINEL} {{\"workmux\":\"{}\",\"protocol\":{PROTOCOL_VERSION}}}\\n'; {after}",
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn status_of_a_reachable_remote_shows_its_workmux_and_protocol() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());

        let mut status = channels.status(Remote::new("devbox", &config()));
        // Setup warnings depend on this machine's agents and dtach.
        if let Reach::Reachable { warnings, .. } = &mut status.reach {
            warnings.clear();
        }

        assert_eq!(
            status,
            RemoteStatus {
                name: "devbox".to_string(),
                transport: TransportKind::Loopback,
                target: "/remote".to_string(),
                reach: Reach::Reachable {
                    workmux: env!("CARGO_PKG_VERSION").to_string(),
                    protocol: PROTOCOL_VERSION,
                    warning: None,
                    warnings: Vec::new(),
                },
            }
        );
        assert_eq!(channels.open_count(), 1);
    }

    #[test]
    fn one_channel_serves_every_request_for_a_remote() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let channels = channels.clone();
                std::thread::spawn(move || channels.status(Remote::new("devbox", &config())))
            })
            .collect();
        for thread in threads {
            assert!(matches!(
                thread.join().unwrap().reach,
                Reach::Reachable { .. }
            ));
        }

        assert_eq!(transport.exec_count(), 1);
    }

    #[test]
    fn an_unreachable_remote_shows_the_transports_error() {
        let transport = Arc::new(ScriptTransport::new([
            "echo 'ssh: connect to host devbox port 22: Connection refused' >&2; exit 255",
        ]));
        let channels = channels_over(transport, fast_policy());

        let status = channels.status(Remote::new("devbox", &config()));

        assert_eq!(
            status.reach,
            Reach::Unreachable {
                error: "ssh: connect to host devbox port 22: Connection refused".to_string()
            }
        );
        assert_eq!(channels.open_count(), 0);
    }

    #[test]
    fn a_stopped_remote_is_never_started_to_fetch_status() {
        let transport = Arc::new(StoppedTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());

        let status = channels.status(Remote::new("sandbox", &config()));

        assert_eq!(status.reach, Reach::Stopped);
        assert_eq!(transport.starts.load(Ordering::SeqCst), 0);
        assert_eq!(transport.execs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_missing_remote_is_never_created_to_fetch_status() {
        let transport = Arc::new(StoppedTransport {
            missing: true,
            ..StoppedTransport::default()
        });
        let channels = channels_over(transport.clone(), fast_policy());

        let status = channels.status(Remote::new("sandbox", &config()));

        assert_eq!(status.reach, Reach::Missing);
        assert_eq!(transport.execs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_killed_remote_is_reconnected() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());
        channels.status(Remote::new("devbox", &config()));

        transport.kill_latest();

        wait_until("a second channel", || {
            transport.exec_count() == 2 && channels.open_count() == 1
        });
        assert!(matches!(
            channels.status(Remote::new("devbox", &config())).reach,
            Reach::Reachable { .. }
        ));
        assert_eq!(transport.exec_count(), 2);
    }

    #[test]
    fn reconnect_attempts_back_off() {
        // The first channel drops at once; every reconnect then fails.
        let transport = Arc::new(ScriptTransport::new([
            hello_then("exec sleep 0.1"),
            "echo 'Connection refused' >&2; exit 255".to_string(),
        ]));
        let channels = channels_over(transport.clone(), fast_policy());
        channels.status(Remote::new("devbox", &config()));

        wait_until("four attempts", || transport.exec_count() >= 4);

        let calls = transport.calls.lock().unwrap().clone();
        let gaps: Vec<Duration> = calls.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps[1] >= Duration::from_millis(100), "{gaps:?}");
        assert!(gaps[2] >= Duration::from_millis(200), "{gaps:?}");
        assert!(gaps[2] > gaps[1], "{gaps:?}");
        assert_eq!(channels.open_count(), 0);
    }

    #[test]
    fn an_idle_channel_is_closed_and_not_reconnected() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(
            transport.clone(),
            ChannelPolicy {
                linger: Duration::from_millis(100),
                ..fast_policy()
            },
        );
        channels.status(Remote::new("devbox", &config()));
        std::thread::sleep(Duration::from_millis(150));

        channels.close_idle();

        assert_eq!(channels.open_count(), 0);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(transport.exec_count(), 1);
    }

    #[test]
    fn a_channel_stays_open_and_reconnects_while_a_pane_is_attached() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(
            transport.clone(),
            ChannelPolicy {
                linger: Duration::from_millis(100),
                ..fast_policy()
            },
        );

        let hold = channels
            .hold_for_pane(Remote::new("devbox", &config()))
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        channels.close_idle();
        assert_eq!(channels.open_count(), 1);

        transport.kill_latest();
        wait_until("a reconnect", || {
            transport.exec_count() == 2 && channels.open_count() == 1
        });

        drop(hold);
        std::thread::sleep(Duration::from_millis(150));
        channels.close_idle();
        assert_eq!(channels.open_count(), 0);
    }

    #[test]
    fn status_files_arrive_on_connect_as_they_change_and_again_after_a_drop() {
        use crate::multiplexer::AgentStatus;
        use crate::remote::status::{StatusFiles, StatusRecord};

        let state = tempfile::tempdir().unwrap();
        let files = StatusFiles::new(state.path().join("workmux/status"));
        let pane = "api/feature/0".parse().unwrap();
        files
            .write(&pane, Some(AgentStatus::Working), None, 100)
            .unwrap();
        let transport = Arc::new(ServeTransport {
            env: vec![("XDG_STATE_HOME".to_string(), state.path().to_path_buf())],
            ..ServeTransport::default()
        });
        let channels = channels_over(
            transport.clone(),
            ChannelPolicy {
                backoff_initial: Duration::from_millis(500),
                ..fast_policy()
            },
        );
        let (sent, received) = std::sync::mpsc::channel::<(String, StatusRecord)>();
        let sent = Mutex::new(sent);
        channels.set_status_sink(Arc::new(move |remote: &str, records: Vec<StatusRecord>| {
            for record in records {
                let _ = sent.lock().unwrap().send((remote.to_string(), record));
            }
        }));
        let next = || {
            received
                .recv_timeout(Duration::from_secs(10))
                .expect("a status record")
        };

        let _hold = channels
            .hold_for_pane(Remote::new("devbox", &config()))
            .unwrap();
        assert!(channels.connected_remotes().contains("devbox"));
        let (remote, first) = next();
        assert_eq!(remote, "devbox");
        assert_eq!(
            (first.status, first.event_ts),
            (Some(AgentStatus::Working), 100)
        );

        files
            .write(&pane, Some(AgentStatus::Waiting), None, 110)
            .unwrap();
        let (_, waiting) = next();
        assert_eq!(
            (waiting.status, waiting.event_ts),
            (Some(AgentStatus::Waiting), 110)
        );

        // Disconnected: work starts and finishes on the remote meanwhile.
        transport.kill_latest();
        wait_until("the drop", || channels.open_count() == 0);
        assert!(channels.connected_remotes().is_empty());
        files
            .write(&pane, Some(AgentStatus::Working), None, 120)
            .unwrap();
        files
            .write(&pane, Some(AgentStatus::Done), None, 130)
            .unwrap();
        let (_, replayed) = next();
        assert_eq!(transport.exec_count(), 2);
        assert_eq!(replayed.status, Some(AgentStatus::Done));
        assert_eq!((replayed.event_ts, replayed.work_gen), (130, 2));
    }

    #[test]
    fn a_hanging_remote_does_not_hold_up_requests_to_another() {
        let serving = Arc::new(ServeTransport::default());
        let hanging = Arc::new(ScriptTransport::new(["exec sleep 30"]));
        let channels = Channels::new(
            Arc::new(move |name, _| {
                Ok(match name {
                    "hanging" => hanging.clone() as Arc<dyn Transport>,
                    _ => serving.clone() as Arc<dyn Transport>,
                })
            }),
            ChannelPolicy {
                hello_timeout: Duration::from_secs(20),
                ..fast_policy()
            },
        );
        channels.channel(Remote::new("devbox", &config())).unwrap();
        let connecting = channels.clone();
        std::thread::spawn(move || connecting.channel(Remote::new("hanging", &config())));
        std::thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        channels.channel(Remote::new("devbox", &config())).unwrap();
        assert!(channels.status(Remote::new("devbox", &config())).reach != Reach::Stopped);
        // A remote not yet connected connects meanwhile too.
        channels
            .channel(Remote::new("buildbox", &config()))
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// A running remote that counts how often its state is asked for.
    #[derive(Default)]
    struct CountingRunning {
        serving: ServeTransport,
        statuses: std::sync::atomic::AtomicUsize,
    }

    impl Transport for CountingRunning {
        fn exec_captured(&self, argv: &[String]) -> Result<std::process::Output> {
            self.serving.exec_captured(argv)
        }
        fn exec_stdio(&self, argv: &[String]) -> Result<crate::remote::transport::StdioProcess> {
            self.serving.exec_stdio(argv)
        }
        fn tty_command(
            &self,
            exec: &crate::remote::transport::TtyExec,
        ) -> Result<std::process::Command> {
            self.serving.tty_command(exec)
        }
        fn provisioning(&self) -> Option<&dyn crate::remote::transport::Provisioning> {
            Some(self)
        }
    }

    impl crate::remote::transport::Provisioning for CountingRunning {
        fn status(&self) -> Result<RemoteState> {
            self.statuses.fetch_add(1, Ordering::SeqCst);
            Ok(RemoteState::Running)
        }
        fn create(&self) -> Result<()> {
            unimplemented!()
        }
        fn start(&self) -> Result<()> {
            unimplemented!()
        }
        fn stop(&self) -> Result<()> {
            unimplemented!()
        }
        fn delete(&self) -> Result<()> {
            unimplemented!()
        }
    }

    #[test]
    fn an_open_channel_is_used_without_asking_the_remote_for_its_state() {
        let transport = Arc::new(CountingRunning::default());
        let channels = channels_over(transport.clone(), fast_policy());

        for _ in 0..3 {
            channels.channel(Remote::new("sandbox", &config())).unwrap();
        }
        channels.status(Remote::new("sandbox", &config()));

        assert_eq!(transport.statuses.load(Ordering::SeqCst), 1);
    }
}
