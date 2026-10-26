//! The forwarding multiplexer: the `Multiplexer` remote workmux runs
//! workflows against. It forwards the subset of operations workflows use to
//! the workstation, which owns tmux (ADR 0003), and refuses the rest as
//! unsupported on remote.
//!
//! A command remote workmux runs in a command session reaches the control
//! channel through a Unix socket that `remote serve` listens on; each request
//! names its command session, so the workstation can answer context
//! questions (current pane, window, session) from the invocation context the
//! session was started with.

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use super::channel::Peer;
use super::pane::{RemotePaneId, SessionHolder, worktree_name};
use super::{session, shell, shims};
use crate::config::{Config, PaneConfig, SplitDirection};
use crate::multiplexer::types::{
    CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams, LivePaneInfo,
    OwnedWindowTarget, PaneSetupOptions, PaneSetupResult, WindowTarget,
};
use crate::multiplexer::{Multiplexer, setup_configured_panes, util};

/// Control channel method carrying one forwarded multiplexer request.
pub const FORWARD_METHOD: &str = "mux.call";

/// One multiplexer operation forwarded from a remote. Windows are named by
/// their full (prefixed) names; a new or respawned pane attaches to the
/// remote pane the remote has already started for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MuxRequest {
    IsRunning,
    InstanceId,
    CurrentPaneId,
    CurrentWindowId,
    RightmostWindowId,
    CurrentSessionId,
    CurrentWindowName,
    CurrentSession,
    ClientSession,
    SessionExists {
        name: String,
    },
    AllWindowNames,
    WindowNamesInSession {
        session: String,
    },
    AllWindowsWithSessions,
    AllSessionNames,
    CreateWindow {
        name: String,
        after_window: Option<String>,
        pane: RemotePaneId,
    },
    CreateSession {
        name: String,
        initial_window_name: Option<String>,
        pane: RemotePaneId,
    },
    CreateWindowInSession {
        session: String,
        name: Option<String>,
        pane: RemotePaneId,
    },
    RespawnPane {
        pane_id: String,
        pane: RemotePaneId,
    },
    SplitPane {
        target_pane_id: String,
        direction: SplitDirection,
        size: Option<u16>,
        percentage: Option<u8>,
        pane: RemotePaneId,
    },
    SetWindowOwnership {
        pane_id: String,
        token: String,
        is_primary: bool,
    },
    SetPaneName {
        pane_id: String,
        name: String,
    },
    SelectPane {
        pane_id: String,
    },
    ZoomPane {
        pane_id: String,
    },
    SelectWindow {
        name: String,
    },
    SwitchToPane {
        pane_id: String,
    },
    SwitchToSession {
        name: String,
    },
    SendText {
        pane_id: String,
        text: String,
    },
    SendEnter {
        pane_id: String,
    },
    SetStatus {
        pane_id: String,
        icon: String,
        auto_clear_on_focus: bool,
    },
    ClearStatus {
        pane_id: String,
    },
    EnsureStatusFormat {
        pane_id: String,
    },
    KillWindow {
        name: String,
    },
    RenameWindow {
        old: String,
        new: String,
    },
    RenameSession {
        old: String,
        new: String,
    },
    OwnedWindowTargets {
        token: String,
    },
    KillWindowTarget {
        target: WindowTarget,
    },
    WindowTargetExists {
        target: WindowTarget,
    },
    KillSession {
        name: String,
    },
    KillPane {
        pane_id: String,
    },
    /// The remote panes of the requesting project that workstation panes
    /// show.
    ShownRemotePanes,
}

/// Environment remote workmux sets on a command it runs in a command
/// session: the socket that reaches the control channel, and the session.
pub const SOCKET_ENV: &str = "WORKMUX_FORWARDING_SOCKET";
pub const SESSION_ENV: &str = "WORKMUX_COMMAND_SESSION";

/// How long one forwarded request may take.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a prompt waits for the user's answer.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// How long the workstation may take to read its clipboard.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a host command run for a remote shell may take.
const HOST_EXEC_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// How long an agent command run on the workstation may take: `wait` may
/// wait for days.
const RELAY_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Params of a forwarded multiplexer request.
#[derive(Debug, Serialize, Deserialize)]
pub struct Forwarded {
    pub session: u64,
    pub request: MuxRequest,
}

/// One line a command sends to `remote serve`'s forwarding socket: a
/// request for the workstation.
#[derive(Debug, Serialize, Deserialize)]
struct SocketRequest {
    method: String,
    params: Value,
}

/// `remote serve`'s answer to a [`SocketRequest`] line.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Reply {
    Result(Value),
    Error(String),
}

/// Listen for forwarded requests on a Unix socket on the remote and relay
/// each over `peer` to the workstation. Returns the socket's path; it is
/// removed when the returned guard drops.
pub fn listen(peer: Peer) -> Result<SocketGuard> {
    let dir = std::env::temp_dir().join(format!("workmux-{}", unsafe { libc::getuid() }));
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(format!("serve-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    listen_at(peer, path)
}

/// Listen for forwarded requests on the Unix socket `path` and relay each
/// over `peer` to the workstation. The socket is removed when the returned
/// guard drops, unless another listener has replaced it.
pub fn listen_at(peer: Peer, path: PathBuf) -> Result<SocketGuard> {
    let (listener, guard) = bind(path)?;
    relay_from(listener, peer);
    Ok(guard)
}

/// Bind the Unix socket `path`, readable by this user only.
pub fn bind(path: PathBuf) -> Result<(UnixListener, SocketGuard)> {
    use std::os::unix::fs::MetadataExt;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("Failed to listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let inode = std::fs::metadata(&path)?.ino();
    Ok((listener, SocketGuard { path, inode }))
}

/// Relay the requests of every connection to `listener` over `peer`.
pub fn relay_from(listener: UnixListener, peer: Peer) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let peer = peer.clone();
            std::thread::spawn(move || relay(stream, &peer));
        }
    });
}

/// Removes the forwarding socket on drop.
pub struct SocketGuard {
    pub path: PathBuf,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&self.path).is_ok_and(|meta| meta.ino() == self.inode) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// How long the workstation may take to answer `method`, for the methods a
/// command may send through the socket.
fn method_timeout(method: &str) -> Option<Duration> {
    match method {
        FORWARD_METHOD | shell::OPEN_METHOD => Some(FORWARD_TIMEOUT),
        session::PROMPT_METHOD => Some(PROMPT_TIMEOUT),
        shims::CLIPBOARD_METHOD => Some(CLIPBOARD_TIMEOUT),
        shims::EXEC_METHOD => Some(HOST_EXEC_TIMEOUT),
        shell::RELAY_METHOD => Some(RELAY_TIMEOUT),
        _ => None,
    }
}

fn relay(stream: UnixStream, peer: &Peer) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    // Shell sessions opened on this connection, closed when it is.
    let mut shells = Vec::new();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let reply = serde_json::from_str::<SocketRequest>(&line)
            .map_err(anyhow::Error::from)
            .and_then(|SocketRequest { method, params }| {
                let Some(timeout) = method_timeout(&method) else {
                    bail!("Unknown method: {method}");
                };
                let value = peer.request(&method, params, timeout)?;
                if method == shell::OPEN_METHOD
                    && let Ok(opened) = serde_json::from_value::<shell::Opened>(value.clone())
                {
                    shells.push(opened.session);
                }
                Ok(value)
            });
        let reply = match reply {
            Ok(value) => Reply::Result(value),
            Err(error) => Reply::Error(format!("{error:#}")),
        };
        let mut line = serde_json::to_string(&reply).expect("serializes");
        line.push('\n');
        if writer.write_all(line.as_bytes()).is_err() {
            break;
        }
    }
    for session in shells {
        let _ = peer.notify(
            shell::CLOSE_METHOD,
            serde_json::to_value(session::SessionParams { session }).expect("serializes"),
        );
    }
}

/// A connection to `remote serve`'s forwarding or shell socket.
struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// The command or shell session requests on this connection belong to.
    session: u64,
}

impl Connection {
    fn open(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .context("Workstation not connected: remote workmux's control channel is gone")?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            session: 0,
        })
    }

    /// Send one request and read its reply. An error means the connection
    /// is gone; a refusal from the workstation is an `Ok` reply.
    fn exchange(&mut self, method: &str, params: Value) -> Result<Reply> {
        let mut line = serde_json::to_string(&SocketRequest {
            method: method.to_string(),
            params,
        })?;
        line.push('\n');
        let mut reply = String::new();
        let sent = self
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| self.reader.read_line(&mut reply));
        if !matches!(sent, Ok(read) if read > 0) {
            bail!("Workstation not connected: the control channel closed");
        }
        Ok(serde_json::from_str(&reply)?)
    }
}

impl Reply {
    fn into_result(self) -> Result<Value> {
        match self {
            Reply::Result(value) => Ok(value),
            Reply::Error(error) => Err(anyhow!("Workstation: {error}")),
        }
    }
}

/// Send one request outside any session through the socket at `socket`.
pub fn request_once(socket: &Path, method: &str, params: Value) -> Result<Value> {
    Connection::open(socket)?
        .exchange(method, params)?
        .into_result()
}

/// Whose requests a forwarding multiplexer sends.
enum Caller {
    /// A command remote workmux runs in a command session.
    Command(u64),
    /// A command run in a remote pane's shell; each connection opens a
    /// shell session for the pane.
    Shell(RemotePaneId),
}

/// The multiplexer of a command remote workmux runs, in a command session
/// or in a remote pane's shell: it forwards to the workstation's tmux over
/// the control channel.
pub struct ForwardingMux {
    socket: PathBuf,
    caller: Caller,
    connection: Mutex<Option<Connection>>,
    /// Project whose remote panes this command starts.
    project: String,
    holder: SessionHolder,
    /// The remote pane each workstation pane this command made shows, so a
    /// respawn can end the one it replaces.
    panes: Mutex<HashMap<String, RemotePaneId>>,
    /// Workstation panes this command made whose remote pane is a login
    /// shell nothing has been typed into yet, with its workdir: a command
    /// for one starts in a remote pane of its own instead of being typed.
    fresh: Mutex<HashMap<String, PathBuf>>,
}

/// Environment carrying a pane's command to the shell that runs it.
const PANE_COMMAND_ENV: &str = "WORKMUX_PANE_COMMAND";

/// What a pane is started with while it awaits its command: a login shell
/// that the command, once known, replaces (see [`ForwardingMux::send_keys`]).
const AWAITS_COMMAND: &str = "# workmux: remote pane awaits its command";

/// The handshake of a forwarding multiplexer: none is needed, since a
/// command never goes to a pane's shell as keystrokes.
struct AwaitCommand;

impl crate::multiplexer::handshake::PaneHandshake for AwaitCommand {
    fn wrapper_command(&self, _shell: &str) -> String {
        AWAITS_COMMAND.to_string()
    }

    fn wait(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

impl ForwardingMux {
    pub fn new(socket: PathBuf, session: u64, project: String, holder: SessionHolder) -> Self {
        Self {
            socket,
            caller: Caller::Command(session),
            connection: Mutex::new(None),
            project,
            holder,
            panes: Mutex::new(HashMap::new()),
            fresh: Mutex::new(HashMap::new()),
        }
    }

    /// The multiplexer of a command run in the shell of remote pane `pane`.
    pub fn for_shell(socket: PathBuf, pane: RemotePaneId, holder: SessionHolder) -> Self {
        Self {
            socket,
            project: pane.project.clone(),
            caller: Caller::Shell(pane),
            connection: Mutex::new(None),
            holder,
            panes: Mutex::new(HashMap::new()),
            fresh: Mutex::new(HashMap::new()),
        }
    }

    /// The forwarding multiplexer of this process, when remote workmux runs
    /// it in a command session or it runs in a remote pane's shell.
    pub fn from_env() -> Option<Self> {
        if let Some(mux) = Self::for_command_session() {
            return Some(mux);
        }
        let shell = shell::Shell::current()?;
        // A remote pane's shell never falls back to a local tmux.
        let holder = SessionHolder::for_this_remote()
            .unwrap_or_else(|_| SessionHolder::new(std::env::temp_dir().join("workmux-panes")));
        Some(Self::for_shell(shell.socket, shell.pane, holder))
    }

    /// The forwarding multiplexer of this process, when remote workmux runs
    /// it in a command session.
    pub fn for_command_session() -> Option<Self> {
        let socket = std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty())?;
        let session = std::env::var(SESSION_ENV).ok()?.parse().ok()?;
        let project = std::env::var(super::session::PROJECT_ENV).ok()?;
        let holder = SessionHolder::for_this_remote()
            .inspect_err(|error| tracing::warn!(error = %format!("{error:#}"), "no session holder"))
            .ok()?;
        Some(Self::new(PathBuf::from(socket), session, project, holder))
    }

    fn call(&self, request: MuxRequest) -> Result<Value> {
        self.request(FORWARD_METHOD, |session| {
            Ok(serde_json::to_value(Forwarded { session, request })?)
        })
    }

    /// Read one line the user types in the terminal on the workstation the
    /// command session was invoked from; `None` at the end of input.
    pub fn read_line(&self) -> Result<Option<String>> {
        let answer: session::Answer =
            serde_json::from_value(self.request(session::PROMPT_METHOD, |session| {
                Ok(serde_json::to_value(session::SessionParams { session })?)
            })?)?;
        Ok(answer.line)
    }

    fn connect(&self) -> Result<Connection> {
        let mut connection = Connection::open(&self.socket)?;
        connection.session = match &self.caller {
            Caller::Command(session) => *session,
            Caller::Shell(pane) => {
                let opened: shell::Opened = serde_json::from_value(
                    connection
                        .exchange(
                            shell::OPEN_METHOD,
                            serde_json::to_value(shell::OpenParams { pane: pane.clone() })?,
                        )?
                        .into_result()?,
                )?;
                opened.session
            }
        };
        Ok(connection)
    }

    fn request(&self, method: &str, params: impl FnOnce(u64) -> Result<Value>) -> Result<Value> {
        let mut connection = self.connection.lock().unwrap();
        if connection.is_none() {
            *connection = Some(self.connect()?);
        }
        let open = connection.as_mut().expect("connected");
        let params = params(open.session)?;
        match open.exchange(method, params) {
            Ok(reply) => reply.into_result(),
            Err(error) => {
                *connection = None;
                Err(error)
            }
        }
    }

    fn get<T: DeserializeOwned>(&self, request: MuxRequest) -> Result<T> {
        Ok(serde_json::from_value(self.call(request)?)?)
    }

    fn act(&self, request: MuxRequest) -> Result<()> {
        self.call(request).map(drop)
    }

    /// Forward a kill, then end the remote panes the killed workstation
    /// panes showed.
    fn kill(&self, request: MuxRequest) -> Result<()> {
        let ended: Vec<RemotePaneId> = self.get(request)?;
        for pane in ended {
            if let Err(error) = self.holder.kill(&pane) {
                tracing::warn!(%pane, error = %format!("{error:#}"), "failed to end remote pane");
            }
        }
        Ok(())
    }

    /// Start `command` (else a login shell) in a new remote pane in
    /// `workdir`, then forward the request that attaches a workstation pane
    /// to it. The remote pane a respawned workstation pane showed is ended
    /// once the new one is attached; a refused request ends the new one.
    fn attach_new_pane(
        &self,
        workdir: &Path,
        command: Option<&str>,
        request: impl FnOnce(RemotePaneId) -> MuxRequest,
    ) -> Result<String> {
        self.attach_new_pane_with_env(workdir, command, &[], request)
    }

    /// [`attach_new_pane`](Self::attach_new_pane), with `env` added to the
    /// remote pane's environment.
    fn attach_new_pane_with_env(
        &self,
        workdir: &Path,
        command: Option<&str>,
        env: &[(&str, &str)],
        request: impl FnOnce(RemotePaneId) -> MuxRequest,
    ) -> Result<String> {
        let shell_only = command.is_none_or(|script| script == AWAITS_COMMAND);
        let argv = match command {
            Some(script) if !shell_only => ["sh", "-c", script].map(String::from).to_vec(),
            _ => vec![self.get_default_shell()?, "-l".to_string()],
        };
        let pane = self
            .holder
            .create_with_env(&self.project, workdir, &argv, env)?;
        let pane_id = self.attach(pane.clone(), request).inspect_err(|_| {
            let _ = self.holder.kill(&pane);
        })?;
        let mut fresh = self.fresh.lock().unwrap();
        if shell_only {
            fresh.insert(pane_id.clone(), workdir.to_path_buf());
        } else {
            fresh.remove(&pane_id);
        }
        Ok(pane_id)
    }

    /// Forward the request that attaches a workstation pane to remote pane
    /// `pane`, ending the remote pane a respawned workstation pane showed.
    fn attach(
        &self,
        pane: RemotePaneId,
        request: impl FnOnce(RemotePaneId) -> MuxRequest,
    ) -> Result<String> {
        let pane_id = self.get::<String>(request(pane.clone()))?;
        let replaced = self.panes.lock().unwrap().insert(pane_id.clone(), pane);
        if let Some(replaced) = replaced {
            let _ = self.holder.kill(&replaced);
        }
        Ok(pane_id)
    }

    /// The project's remote panes that still run but no workstation pane
    /// shows, in the order they were started: those of a window that was
    /// closed outside workmux or lost with the workstation's tmux.
    pub fn surviving_panes(&self) -> Result<Vec<RemotePaneId>> {
        let shown: HashSet<RemotePaneId> = self.get(MuxRequest::ShownRemotePanes)?;
        Ok(self
            .holder
            .running(&self.project)?
            .into_iter()
            .filter(|pane| !shown.contains(pane))
            .collect())
    }

    /// Lay out `panes` as configured, each showing the next of `surviving`
    /// instead of starting its command. A layout with fewer panes than
    /// survive shows only as many.
    fn show_surviving_panes(
        &self,
        initial_pane_id: &str,
        panes: &[PaneConfig],
        surviving: Vec<RemotePaneId>,
    ) -> Result<PaneSetupResult> {
        let single = [PaneConfig::default()];
        let panes = if panes.is_empty() { &single[..] } else { panes };
        let layout = panes
            .iter()
            .enumerate()
            .filter(|(i, pane)| *i == 0 || pane.split.is_some())
            .map(|(_, pane)| pane);
        let mut pane_ids = vec![initial_pane_id.to_string()];
        let mut focus_pane_id = None;
        let mut zoom_pane_id = None;
        for (i, (config, pane)) in layout.zip(surviving).enumerate() {
            let pane_id = match &config.split {
                Some(direction) if i > 0 => {
                    let target_idx = config.target.unwrap_or(pane_ids.len() - 1);
                    let target = pane_ids
                        .get(target_idx)
                        .ok_or_else(|| anyhow!("Invalid target pane index: {}", target_idx))?
                        .clone();
                    let pane_id = self.attach(pane, |pane| MuxRequest::SplitPane {
                        target_pane_id: target,
                        direction: direction.clone(),
                        size: config.size,
                        percentage: config.percentage,
                        pane,
                    })?;
                    pane_ids.push(pane_id.clone());
                    pane_id
                }
                _ => {
                    let pane_id = self.attach(pane, |pane| MuxRequest::RespawnPane {
                        pane_id: pane_ids[0].clone(),
                        pane,
                    })?;
                    pane_ids[0] = pane_id.clone();
                    pane_id
                }
            };
            if let Some(name) = config.name.as_deref() {
                self.set_pane_name(&pane_id, name)?;
            }
            if config.zoom || config.focus {
                focus_pane_id = Some(pane_id.clone());
            }
            if config.zoom {
                zoom_pane_id = Some(pane_id);
            }
        }
        Ok(PaneSetupResult {
            focus_pane_id: focus_pane_id.unwrap_or_else(|| pane_ids[0].clone()),
            zoom_pane_id,
        })
    }
}

impl ForwardingMux {
    /// Forward `request`, ending the remote panes a kill leaves without a
    /// workstation pane.
    pub fn forward(&self, request: MuxRequest) -> Result<()> {
        match request {
            MuxRequest::KillWindow { .. }
            | MuxRequest::KillWindowTarget { .. }
            | MuxRequest::KillSession { .. }
            | MuxRequest::KillPane { .. } => self.kill(request),
            request => self.act(request),
        }
    }

    /// Refuse `what` outside a remote shell: what a command session defers
    /// would run after its session has closed.
    fn in_shell(&self, what: &str) -> Result<()> {
        match self.caller {
            Caller::Shell(_) => Ok(()),
            Caller::Command(_) => Err(unsupported(what)),
        }
    }

    /// A shell command, for a deferred script, that forwards `request`.
    fn forward_cmd(&self, request: MuxRequest) -> Result<String> {
        self.in_shell("a shell command for the workstation's tmux")?;
        let workmux = std::env::current_exe().context("Failed to locate the workmux binary")?;
        let quote = |value: &str| format!("'{}'", value.replace('\'', r#"'\''"#));
        Ok(format!(
            "{} remote forward {}",
            quote(&workmux.to_string_lossy()),
            quote(&serde_json::to_string(&request)?)
        ))
    }
}

fn unsupported(what: &str) -> anyhow::Error {
    anyhow!("{what} is not supported on a remote")
}

impl Multiplexer for ForwardingMux {
    fn name(&self) -> &'static str {
        "tmux"
    }

    fn runs_on_remote(&self) -> bool {
        true
    }

    fn is_running(&self) -> Result<bool> {
        self.get(MuxRequest::IsRunning)
    }

    fn current_window_id(&self) -> Result<Option<String>> {
        self.get(MuxRequest::CurrentWindowId)
    }

    fn rightmost_window_id(&self) -> Result<Option<String>> {
        self.get(MuxRequest::RightmostWindowId)
    }

    fn current_session_id(&self) -> Result<Option<String>> {
        self.get(MuxRequest::CurrentSessionId)
    }

    fn current_pane_id(&self) -> Option<String> {
        self.get(MuxRequest::CurrentPaneId).ok().flatten()
    }

    fn active_pane_id(&self) -> Option<String> {
        self.current_pane_id()
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        Err(unsupported("the workstation pane's path"))
    }

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        self.attach_new_pane(params.cwd, None, |pane| MuxRequest::CreateWindow {
            name: util::prefixed(params.prefix, params.name),
            after_window: params.after_window.map(str::to_string),
            pane,
        })
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        self.attach_new_pane(params.cwd, None, |pane| MuxRequest::CreateSession {
            name: util::prefixed(params.prefix, params.name),
            initial_window_name: params.initial_window_name.map(str::to_string),
            pane,
        })
    }

    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        self.attach_new_pane(params.cwd, None, |pane| MuxRequest::CreateWindowInSession {
            session: params.session_name.to_string(),
            name: params.name.map(str::to_string),
            pane,
        })
    }

    fn supports_window_ownership(&self) -> bool {
        true
    }

    fn set_window_ownership(&self, pane_id: &str, token: &str, is_primary: bool) -> Result<()> {
        self.act(MuxRequest::SetWindowOwnership {
            pane_id: pane_id.to_string(),
            token: token.to_string(),
            is_primary,
        })
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        self.act(MuxRequest::SwitchToSession {
            name: util::prefixed(prefix, name),
        })
    }

    fn session_exists(&self, full_name: &str) -> Result<bool> {
        self.get(MuxRequest::SessionExists {
            name: full_name.to_string(),
        })
    }

    fn kill_session(&self, full_name: &str) -> Result<()> {
        self.kill(MuxRequest::KillSession {
            name: full_name.to_string(),
        })
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        self.kill(MuxRequest::KillWindow {
            name: full_name.to_string(),
        })
    }

    fn rename_window(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        self.act(MuxRequest::RenameWindow {
            old: old_full_name.to_string(),
            new: new_full_name.to_string(),
        })
    }

    fn rename_session(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        self.act(MuxRequest::RenameSession {
            old: old_full_name.to_string(),
            new: new_full_name.to_string(),
        })
    }

    fn kill_window_target(&self, target: &WindowTarget) -> Result<()> {
        self.kill(MuxRequest::KillWindowTarget {
            target: target.clone(),
        })
    }

    fn window_target_exists(&self, target: &WindowTarget) -> Result<bool> {
        self.get(MuxRequest::WindowTargetExists {
            target: target.clone(),
        })
    }

    fn owned_window_targets(&self, token: &str) -> Result<Vec<OwnedWindowTarget>> {
        self.get(MuxRequest::OwnedWindowTargets {
            token: token.to_string(),
        })
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        self.in_shell("scheduling a window close")?;
        let kill = self.shell_kill_window_cmd(full_name)?;
        self.run_deferred_script(&format!("sleep {:.3}; {kill}", delay.as_secs_f64()))
    }

    fn schedule_window_target_close(&self, target: &WindowTarget, delay: Duration) -> Result<()> {
        self.in_shell("scheduling a window close")?;
        let kill = self.shell_kill_window_target_cmd(target)?;
        self.run_deferred_script(&format!("sleep {:.3}; {kill}", delay.as_secs_f64()))
    }

    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        self.in_shell("scheduling a session close")?;
        let kill = self.shell_kill_session_cmd(full_name)?;
        self.run_deferred_script(&format!("sleep {:.3}; {kill}", delay.as_secs_f64()))
    }

    /// Run `script` on the remote once this command has exited, as a
    /// process of its own. Its steps are `remote forward` commands, so what
    /// it does to workstation tmux is checked like any forwarded request.
    /// Only a remote shell can: a command session ends with its command.
    fn run_deferred_script(&self, script: &str) -> Result<()> {
        use std::os::unix::process::CommandExt;
        self.in_shell("running a deferred script")?;
        std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Outlives the remote pane it was started from.
            .process_group(0)
            .spawn()
            .context("Failed to start a deferred script")?;
        Ok(())
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        self.forward_cmd(MuxRequest::SelectWindow {
            name: full_name.to_string(),
        })
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        self.forward_cmd(MuxRequest::KillWindow {
            name: full_name.to_string(),
        })
    }

    fn shell_kill_window_target_cmd(&self, target: &WindowTarget) -> Result<String> {
        self.forward_cmd(MuxRequest::KillWindowTarget {
            target: target.clone(),
        })
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        self.forward_cmd(MuxRequest::SwitchToSession {
            name: full_name.to_string(),
        })
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        self.forward_cmd(MuxRequest::KillSession {
            name: full_name.to_string(),
        })
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        self.act(MuxRequest::SelectWindow {
            name: util::prefixed(prefix, name),
        })
    }

    fn select_window_target(&self, target: &WindowTarget) -> Result<()> {
        self.act(MuxRequest::SelectWindow {
            name: target.full_name.clone(),
        })
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        self.get(MuxRequest::CurrentWindowName)
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        self.get(MuxRequest::AllWindowNames)
    }

    fn get_window_names_in_session(&self, session_name: &str) -> Result<HashSet<String>> {
        self.get(MuxRequest::WindowNamesInSession {
            session: session_name.to_string(),
        })
    }

    fn get_all_windows_with_sessions(&self) -> Result<HashSet<(String, String)>> {
        self.get(MuxRequest::AllWindowsWithSessions)
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        self.get(MuxRequest::AllSessionNames)
    }

    fn wait_until_session_closed(&self, full_session_name: &str) -> Result<()> {
        println!("Waiting for session '{full_session_name}' to close...");
        while self.session_exists(full_session_name)? {
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.act(MuxRequest::SelectPane {
            pane_id: pane_id.to_string(),
        })
    }

    fn zoom_pane(&self, pane_id: &str) -> Result<()> {
        self.act(MuxRequest::ZoomPane {
            pane_id: pane_id.to_string(),
        })
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        self.act(MuxRequest::SwitchToPane {
            pane_id: pane_id.to_string(),
        })
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.kill(MuxRequest::KillPane {
            pane_id: pane_id.to_string(),
        })
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        self.attach_new_pane(cwd, cmd, |pane| MuxRequest::RespawnPane {
            pane_id: pane_id.to_string(),
            pane,
        })
    }

    fn set_pane_name(&self, pane_id: &str, name: &str) -> Result<()> {
        self.act(MuxRequest::SetPaneName {
            pane_id: pane_id.to_string(),
            name: name.to_string(),
        })
    }

    fn capture_pane(&self, _pane_id: &str, _lines: u16) -> Option<String> {
        None
    }

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        self.fresh.lock().unwrap().remove(pane_id);
        self.act(MuxRequest::SendText {
            pane_id: pane_id.to_string(),
            text: text.to_string(),
        })
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.act(MuxRequest::SendEnter {
            pane_id: pane_id.to_string(),
        })
    }

    /// A command for a pane this command made, still a fresh login shell,
    /// starts in a remote pane of its own (an interactive login shell
    /// running it, then a login shell) that replaces the fresh one. Typed,
    /// it could be lost: to the shell still starting up, or to a
    /// workstation view whose first attach failed.
    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        let fresh = self.fresh.lock().unwrap().remove(pane_id);
        let Some(workdir) = fresh else {
            self.send_text_fragment(pane_id, command)?;
            return self.send_enter(pane_id);
        };
        // The command travels in the environment, so it shows in no command
        // line but its own.
        let shell = crate::shell::shell_quote(&self.get_default_shell()?);
        let script = format!(
            "exec {shell} -l -i -c {}",
            crate::shell::shell_quote(&format!(
                "command=${PANE_COMMAND_ENV}; unset {PANE_COMMAND_ENV}; eval \"$command\"\nexec {shell} -l"
            ))
        );
        self.attach_new_pane_with_env(
            &workdir,
            Some(&script),
            &[(PANE_COMMAND_ENV, command)],
            |pane| MuxRequest::RespawnPane {
                pane_id: pane_id.to_string(),
                pane,
            },
        )
        .map(drop)
    }

    fn create_handshake(&self) -> Result<Box<dyn crate::multiplexer::handshake::PaneHandshake>> {
        Ok(Box::new(AwaitCommand))
    }

    fn send_key(&self, _pane_id: &str, _key: &str) -> Result<()> {
        Err(unsupported("sending a key"))
    }

    fn paste_text(&self, _pane_id: &str, _content: &str) -> Result<()> {
        Err(unsupported("pasting text"))
    }

    fn set_status(&self, pane_id: &str, icon: &str, auto_clear_on_focus: bool) -> Result<()> {
        self.act(MuxRequest::SetStatus {
            pane_id: pane_id.to_string(),
            icon: icon.to_string(),
            auto_clear_on_focus,
        })
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        self.act(MuxRequest::ClearStatus {
            pane_id: pane_id.to_string(),
        })
    }

    fn ensure_status_format(&self, pane_id: &str) -> Result<()> {
        self.act(MuxRequest::EnsureStatusFormat {
            pane_id: pane_id.to_string(),
        })
    }

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        self.attach_new_pane(cwd, command, |pane| MuxRequest::SplitPane {
            target_pane_id: target_pane_id.to_string(),
            direction: direction.clone(),
            size,
            percentage,
            pane,
        })
    }

    /// A worktree whose remote panes survive shows them; otherwise the
    /// configured panes start as they would locally.
    fn setup_panes(
        &self,
        initial_pane_id: &str,
        panes: &[PaneConfig],
        working_dir: &Path,
        options: PaneSetupOptions<'_>,
        config: &Config,
        task_agent: Option<&str>,
    ) -> Result<PaneSetupResult> {
        let worktree = worktree_name(options.worktree_root.unwrap_or(working_dir));
        let surviving: Vec<RemotePaneId> = self
            .surviving_panes()?
            .into_iter()
            .filter(|pane| pane.worktree == worktree)
            .collect();
        if surviving.is_empty() {
            return setup_configured_panes(
                self,
                initial_pane_id,
                panes,
                working_dir,
                options,
                config,
                task_agent,
            );
        }
        self.show_surviving_panes(initial_pane_id, panes, surviving)
    }

    fn current_session(&self) -> Option<String> {
        self.get(MuxRequest::CurrentSession).ok().flatten()
    }

    fn client_session(&self) -> Option<String> {
        self.get(MuxRequest::ClientSession).ok().flatten()
    }

    fn instance_id(&self) -> String {
        self.resolve_instance_id()
            .unwrap_or_else(|_| "remote".to_string())
    }

    fn resolve_instance_id(&self) -> Result<String> {
        self.get(MuxRequest::InstanceId)
    }

    fn get_live_pane_info(&self, _pane_id: &str) -> Result<Option<LivePaneInfo>> {
        Err(unsupported("live pane info"))
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        Err(unsupported("live pane info"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, TransportKind};
    use crate::multiplexer::InvocationContext;
    use crate::remote::channels::{ChannelPolicy, Channels};
    use crate::remote::forwarded::{REMOTE_PANE_TAG, WINDOW_TAG};
    use crate::remote::pane::PaneState;
    use crate::remote::session::{CommandRequest, ProjectSite, Stream};
    use crate::remote::testing::ServeTransport;
    use crate::remote::transport::Transport;
    use crate::test_support::PrivateTmuxServer;
    use std::sync::Arc;
    use std::sync::mpsc;

    fn remote() -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Loopback,
            target: "/remote".to_string(),
            builtin_shims: None,
            host_commands: vec![],
            openshell: None,
        }
    }

    /// Stands in for workstation workmux in panes: stays attached.
    fn fake_workmux(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("workmux");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A command running in a command session on `transport`'s remote,
    /// invoked from `invocation`, whose remote panes `holder` keeps. Yields
    /// the forwarding multiplexer it was given, then holds the session open
    /// until `release` exists.
    fn command_session(
        transport: Arc<ServeTransport>,
        invocation: InvocationContext,
        project: &std::path::Path,
        release: &std::path::Path,
        holder: SessionHolder,
    ) -> (ForwardingMux, std::thread::JoinHandle<Result<i32>>) {
        let channels = Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            ChannelPolicy {
                workmux: fake_workmux(release.parent().unwrap()),
                ..ChannelPolicy::default()
            },
        );
        let request = CommandRequest {
            project: ProjectSite {
                remote: "devbox".to_string(),
                name: "api".to_string(),
                path: project.display().to_string(),
                git_url: None,
                session: None,
            },
            argv: [
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "echo \"${SOCKET_ENV} ${SESSION_ENV}\"; \
                     i=0; while [ ! -e '{}' ] && [ $i -lt 1000 ]; do sleep 0.01; i=$((i+1)); done",
                    release.display()
                ),
            ]
            .to_vec(),
            invoking_window_token: None,
            prompt_file: None,
        };
        let (env, received) = mpsc::channel();
        let session = std::thread::spawn(move || {
            channels.run_command(
                crate::remote::Remote::new("devbox", &remote()),
                &request,
                &invocation,
                &mut |stream, data: &[u8]| {
                    if stream == Stream::Stdout {
                        let _ = env.send(String::from_utf8_lossy(data).into_owned());
                    }
                },
                false,
            )
        });
        let line = received
            .recv_timeout(Duration::from_secs(20))
            .expect("the command printed its forwarding environment");
        let (socket, session_id) = line.trim().split_once(' ').unwrap();
        (
            ForwardingMux::new(
                PathBuf::from(socket),
                session_id.parse().unwrap(),
                "api".to_string(),
                holder,
            ),
            session,
        )
    }

    fn start_command(server: &PrivateTmuxServer, pane: &str) -> String {
        server.query(&["display-message", "-p", "-t", pane, "#{pane_start_command}"])
    }

    #[test]
    fn a_command_typed_into_a_new_pane_reaches_its_remote_pane_even_when_no_view_attaches() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let Some((panes, _holder)) = crate::remote::pane::tests::holder() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let worktree = panes.path().join("feature");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let invoking = server.new_session("mine", temp.path());
        let release = temp.path().join("release");
        let invocation = InvocationContext {
            pane_id: Some(invoking),
            tmux: Some(format!("{},1,0", server.socket)),
            cwd: None,
        };
        // The workstation pane's view never attaches (it only sleeps), as
        // when the first attach fails and the view waits to reconnect.
        let (mux, session) = command_session(
            Arc::new(ServeTransport::default()),
            invocation,
            temp.path(),
            &release,
            SessionHolder::new(panes.path().join("panes")),
        );
        let pane = mux
            .create_session(CreateSessionParams {
                prefix: "",
                name: "api",
                cwd: &worktree,
                initial_window_name: Some("wm-feature"),
            })
            .unwrap();
        let started = temp.path().join("agent-started");

        mux.send_keys(&pane, &format!("echo yes > '{}'", started.display()))
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !started.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the agent command never ran"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        mux.kill_session("api").unwrap();
        std::fs::write(&release, "").unwrap();
        session.join().unwrap().unwrap();
    }

    #[test]
    fn a_command_on_the_remote_creates_a_project_window_attached_to_a_remote_pane() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let Some((panes, holder)) = crate::remote::pane::tests::holder() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let worktree = panes.path().join("feature");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let invoking = server.new_session("mine", temp.path());
        let release = temp.path().join("release");
        let invocation = InvocationContext {
            pane_id: Some(invoking.clone()),
            tmux: Some(format!("{},1,0", server.socket)),
            cwd: None,
        };
        let (mux, session) = command_session(
            Arc::new(ServeTransport::default()),
            invocation,
            temp.path(),
            &release,
            SessionHolder::new(panes.path().join("panes")),
        );

        assert_eq!(mux.current_pane_id(), Some(invoking));
        assert!(!mux.session_exists("api").unwrap());
        let pane = mux
            .create_session(CreateSessionParams {
                prefix: "",
                name: "api",
                cwd: &worktree,
                initial_window_name: Some("wm-feature"),
            })
            .unwrap();

        assert_eq!(
            server.query(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{session_name}:#{window_name}"
            ]),
            "api:wm-feature"
        );
        assert_eq!(
            server.query(&["show-options", "-wqv", "-t", &pane, WINDOW_TAG]),
            "devbox:api"
        );
        let shell: RemotePaneId = "api/feature/0".parse().unwrap();
        assert!(
            start_command(&server, &pane).contains("remote attach devbox api/feature/0"),
            "{}",
            start_command(&server, &pane)
        );
        assert_eq!(holder.state(&shell).unwrap(), PaneState::Running);

        // Respawned with a command, the pane shows a new remote pane and
        // the login shell it replaced ends.
        mux.respawn_pane(&pane, &worktree, Some("exec sleep 30"))
            .unwrap();
        assert!(
            start_command(&server, &pane).contains("remote attach devbox api/feature/1"),
            "{}",
            start_command(&server, &pane)
        );
        assert_eq!(holder.state(&shell).unwrap(), PaneState::Ended);
        let agent: RemotePaneId = "api/feature/1".parse().unwrap();
        assert_eq!(holder.state(&agent).unwrap(), PaneState::Running);

        // Killing the window's session ends the remote panes it showed.
        mux.kill_session("api").unwrap();
        assert_eq!(holder.state(&agent).unwrap(), PaneState::Ended);
        std::fs::write(&release, "").unwrap();
        assert_eq!(session.join().unwrap().unwrap(), 0);
    }

    /// The workstation's channels to `transport`'s remote, answering from
    /// `server`'s tmux, with every project known on the remote.
    fn shell_channels(
        server: &PrivateTmuxServer,
        transport: Arc<ServeTransport>,
        dir: &Path,
        linger: Duration,
    ) -> Channels {
        Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            ChannelPolicy {
                workmux: fake_workmux(dir),
                linger,
                tmux_socket: Some(server.socket.clone()),
                registered: |_, project| project == "api",
                ..ChannelPolicy::default()
            },
        )
    }

    /// A workstation window of project api on devbox whose pane shows
    /// remote pane `pane`.
    fn viewing_pane(server: &PrivateTmuxServer, session: &str, dir: &Path, pane: &str) -> String {
        let viewing = server.new_session(session, dir);
        server.query(&["set-option", "-w", "-t", &viewing, WINDOW_TAG, "devbox:api"]);
        server.query(&["set-option", "-p", "-t", &viewing, REMOTE_PANE_TAG, pane]);
        viewing
    }

    #[test]
    fn a_command_in_a_remote_shell_reaches_workstation_tmux_through_the_shell_socket() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let Some((panes, holder)) = crate::remote::pane::tests::holder() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let worktree = panes.path().join("feature");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let transport = Arc::new(ServeTransport::default());
        let channels = shell_channels(
            &server,
            transport.clone(),
            temp.path(),
            Duration::from_secs(60),
        );
        channels
            .channel(crate::remote::Remote::new("devbox", &remote()))
            .unwrap();
        let viewing = viewing_pane(&server, "api", temp.path(), "api/feature/0");
        let shell: RemotePaneId = "api/feature/0".parse().unwrap();
        let mux = ForwardingMux::for_shell(transport.shell_socket(), shell.clone(), holder);

        // The shell's context is the workstation pane showing it.
        assert_eq!(mux.current_pane_id(), Some(viewing.clone()));
        assert_eq!(mux.current_session(), Some("api".to_string()));
        let pane = mux
            .create_window(CreateWindowParams {
                prefix: "wm-",
                name: "other",
                cwd: &worktree,
                after_window: None,
            })
            .unwrap();
        assert_eq!(
            server.query(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{session_name}:#{window_name}"
            ]),
            "api:wm-other"
        );
        assert_eq!(
            server.query(&["show-options", "-wqv", "-t", &pane, WINDOW_TAG]),
            "devbox:api"
        );
        // Like any forwarded request, it cannot reach the user's own panes.
        let mine = server.new_session("mine", temp.path());
        let error = mux.select_pane(&mine).unwrap_err().to_string();
        assert!(error.contains("Refused"), "{error}");
        mux.kill_window("wm-other").unwrap();
    }

    #[test]
    fn a_remote_shell_of_a_project_not_on_the_remote_is_refused() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let transport = Arc::new(ServeTransport::default());
        let channels = shell_channels(
            &server,
            transport.clone(),
            temp.path(),
            Duration::from_secs(60),
        );
        channels
            .channel(crate::remote::Remote::new("devbox", &remote()))
            .unwrap();
        let mux = ForwardingMux::for_shell(
            transport.shell_socket(),
            "web/feature/0".parse().unwrap(),
            SessionHolder::new(temp.path().join("panes")),
        );

        let error = mux.is_running().unwrap_err().to_string();

        assert!(
            error.contains("Refused: project web is not on remote devbox"),
            "{error}"
        );
    }

    #[test]
    fn once_the_control_channel_closes_a_remote_shell_is_not_connected() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let transport = Arc::new(ServeTransport::default());
        let channels = shell_channels(&server, transport.clone(), temp.path(), Duration::ZERO);
        channels
            .channel(crate::remote::Remote::new("devbox", &remote()))
            .unwrap();
        let mux = ForwardingMux::for_shell(
            transport.shell_socket(),
            "api/feature/0".parse().unwrap(),
            SessionHolder::new(temp.path().join("panes")),
        );
        mux.is_running().unwrap();

        channels.close_idle();

        let started = std::time::Instant::now();
        let error = loop {
            match ForwardingMux::for_shell(
                transport.shell_socket(),
                "api/feature/0".parse().unwrap(),
                SessionHolder::new(temp.path().join("panes")),
            )
            .is_running()
            {
                Err(error) => break error.to_string(),
                Ok(_) if started.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Ok(_) => panic!("the shell socket outlived its channel"),
            }
        };
        assert!(error.starts_with("Workstation not connected"), "{error}");
    }

    #[test]
    fn a_new_window_shows_the_worktrees_surviving_remote_panes_instead_of_starting_its_commands() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let Some((panes, holder)) = crate::remote::pane::tests::holder() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let worktree = panes.path().join("feature");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let invoking = server.new_session("mine", temp.path());
        let release = temp.path().join("release");
        let (mux, session) = command_session(
            Arc::new(ServeTransport::default()),
            InvocationContext {
                pane_id: Some(invoking),
                tmux: Some(format!("{},1,0", server.socket)),
                cwd: None,
            },
            temp.path(),
            &release,
            SessionHolder::new(panes.path().join("panes")),
        );
        // One remote pane is shown in another window; two survive a window
        // that is gone: the agent, then the shell split beside it.
        let shown_window = mux
            .create_session(CreateSessionParams {
                prefix: "",
                name: "api",
                cwd: &worktree,
                initial_window_name: Some("wm-other"),
            })
            .unwrap();
        let shown: RemotePaneId = "api/feature/0".parse().unwrap();
        let sleep = ["sleep", "30"].map(String::from).to_vec();
        let agent = holder.create("api", &worktree, &sleep).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let shell = holder.create("api", &worktree, &sleep).unwrap();
        let window = mux
            .create_window_in_session(CreateWindowInSessionParams {
                session_name: "api",
                name: Some("wm-feature"),
                cwd: &worktree,
            })
            .unwrap();
        let login_shell: RemotePaneId = "api/feature/3".parse().unwrap();
        assert_eq!(holder.state(&login_shell).unwrap(), PaneState::Running);
        let layout = vec![
            crate::config::PaneConfig {
                command: Some("touch started-an-agent".to_string()),
                ..Default::default()
            },
            crate::config::PaneConfig {
                command: Some("touch started-a-shell".to_string()),
                split: Some(SplitDirection::Horizontal),
                ..Default::default()
            },
        ];

        let result = mux
            .setup_panes(
                &window,
                &layout,
                &worktree,
                crate::multiplexer::types::PaneSetupOptions {
                    run_commands: true,
                    prompt_file_path: None,
                    worktree_root: Some(&worktree),
                    lima_vm_name: None,
                    resume_mode: Default::default(),
                },
                &crate::config::Config::default(),
                None,
            )
            .unwrap();

        let window_panes =
            server.query(&["list-panes", "-t", &window, "-F", "#{@workmux_remote_pane}"]);
        assert_eq!(
            window_panes.lines().collect::<Vec<_>>(),
            vec![agent.to_string(), shell.to_string()],
            "{window_panes}"
        );
        assert_eq!(
            server.query(&[
                "display-message",
                "-p",
                "-t",
                &result.focus_pane_id,
                "#{@workmux_remote_pane}"
            ]),
            agent.to_string()
        );
        assert!(!worktree.join("started-an-agent").exists());
        assert!(!worktree.join("started-a-shell").exists());
        assert_eq!(holder.state(&login_shell).unwrap(), PaneState::Ended);
        assert_eq!(
            holder.running("api").unwrap(),
            vec![shown.clone(), agent.clone(), shell.clone()]
        );
        assert_eq!(
            server.query(&[
                "display-message",
                "-p",
                "-t",
                &shown_window,
                "#{@workmux_remote_pane}"
            ]),
            shown.to_string()
        );

        mux.kill_session("api").unwrap();
        assert_eq!(holder.running("api").unwrap(), vec![]);
        std::fs::write(&release, "").unwrap();
        assert_eq!(session.join().unwrap().unwrap(), 0);
    }

    #[test]
    fn requests_for_a_session_that_is_not_open_are_refused() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let release = temp.path().join("release");
        let (mux, session) = command_session(
            Arc::new(ServeTransport::default()),
            InvocationContext {
                tmux: Some(format!("{},1,0", server.socket)),
                ..InvocationContext::default()
            },
            temp.path(),
            &release,
            SessionHolder::new(temp.path().join("panes")),
        );
        let forged = ForwardingMux::new(
            mux.socket.clone(),
            match mux.caller {
                Caller::Command(session) => session + 100,
                Caller::Shell(_) => unreachable!(),
            },
            "api".to_string(),
            SessionHolder::new(temp.path().join("panes")),
        );

        let error = forged.session_exists("api").unwrap_err().to_string();

        assert!(error.contains("Refused: no command session"), "{error}");
        std::fs::write(&release, "").unwrap();
        session.join().unwrap().unwrap();
    }

    #[test]
    fn without_the_control_channel_requests_fail_as_not_connected() {
        let temp = tempfile::tempdir().unwrap();
        let mux = ForwardingMux::new(
            temp.path().join("gone.sock"),
            1,
            "api".to_string(),
            SessionHolder::new(temp.path().join("panes")),
        );

        let error = mux.is_running().unwrap_err().to_string();

        assert!(error.starts_with("Workstation not connected"), "{error}");
    }

    #[test]
    fn workflow_only_operations_are_unsupported_on_remote() {
        let temp = tempfile::tempdir().unwrap();
        let mux = ForwardingMux::new(
            temp.path().join("gone.sock"),
            1,
            "api".to_string(),
            SessionHolder::new(temp.path().join("panes")),
        );

        let error = mux
            .schedule_window_close("wm-x", Duration::from_secs(1))
            .unwrap_err()
            .to_string();

        assert_eq!(
            error,
            "scheduling a window close is not supported on a remote"
        );
    }
}
