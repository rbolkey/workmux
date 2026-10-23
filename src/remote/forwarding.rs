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
use crate::config::SplitDirection;
use crate::multiplexer::types::{
    CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams, LivePaneInfo,
    WindowTarget,
};
use crate::multiplexer::{Multiplexer, util};

/// Control channel method carrying one forwarded multiplexer request.
pub const FORWARD_METHOD: &str = "mux.call";

/// A terminal for a workstation pane: what runs on the remote, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTty {
    pub argv: Vec<String>,
    /// Working directory on the remote.
    pub workdir: String,
}

/// One multiplexer operation forwarded from a remote. Windows are named by
/// their full (prefixed) names.
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
        tty: RemoteTty,
    },
    CreateSession {
        name: String,
        initial_window_name: Option<String>,
        tty: RemoteTty,
    },
    CreateWindowInSession {
        session: String,
        name: Option<String>,
        tty: RemoteTty,
    },
    RespawnPane {
        pane_id: String,
        tty: RemoteTty,
    },
    SplitPane {
        target_pane_id: String,
        direction: SplitDirection,
        size: Option<u16>,
        percentage: Option<u8>,
        tty: RemoteTty,
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
    KillSession {
        name: String,
    },
    KillPane {
        pane_id: String,
    },
}

/// Environment remote workmux sets on a command it runs in a command
/// session: the socket that reaches the control channel, and the session.
pub const SOCKET_ENV: &str = "WORKMUX_FORWARDING_SOCKET";
pub const SESSION_ENV: &str = "WORKMUX_COMMAND_SESSION";

/// How long one forwarded request may take.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(120);

/// One line a command sends to `remote serve`'s forwarding socket.
#[derive(Debug, Serialize, Deserialize)]
pub struct Forwarded {
    pub session: u64,
    pub request: MuxRequest,
}

/// `remote serve`'s answer to a [`Forwarded`] line.
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
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(format!("serve-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen on {}", path.display()))?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let peer = peer.clone();
            std::thread::spawn(move || relay(stream, &peer));
        }
    });
    Ok(SocketGuard { path })
}

/// Removes the forwarding socket on drop.
pub struct SocketGuard {
    pub path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn relay(stream: UnixStream, peer: &Peer) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let reply = serde_json::from_str::<Value>(&line)
            .map_err(anyhow::Error::from)
            .and_then(|params| peer.request(FORWARD_METHOD, params, FORWARD_TIMEOUT));
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
}

struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// The multiplexer of a command run in a command session on a remote: it
/// forwards to the workstation's tmux over the control channel.
pub struct ForwardingMux {
    socket: PathBuf,
    session: u64,
    connection: Mutex<Option<Connection>>,
}

impl ForwardingMux {
    pub fn new(socket: PathBuf, session: u64) -> Self {
        Self {
            socket,
            session,
            connection: Mutex::new(None),
        }
    }

    /// The forwarding multiplexer of this process, when remote workmux runs
    /// it in a command session.
    pub fn from_env() -> Option<Self> {
        let socket = std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty())?;
        let session = std::env::var(SESSION_ENV).ok()?.parse().ok()?;
        Some(Self::new(PathBuf::from(socket), session))
    }

    fn call(&self, request: MuxRequest) -> Result<Value> {
        let mut connection = self.connection.lock().unwrap();
        if connection.is_none() {
            let stream = UnixStream::connect(&self.socket)
                .context("workstation not connected: remote workmux's control channel is gone")?;
            *connection = Some(Connection {
                reader: BufReader::new(stream.try_clone()?),
                writer: stream,
            });
        }
        let open = connection.as_mut().expect("connected");
        let mut line = serde_json::to_string(&Forwarded {
            session: self.session,
            request,
        })?;
        line.push('\n');
        let mut reply = String::new();
        let sent = open
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| open.reader.read_line(&mut reply));
        if !matches!(sent, Ok(read) if read > 0) {
            *connection = None;
            bail!("workstation not connected: the control channel closed");
        }
        match serde_json::from_str(&reply)? {
            Reply::Result(value) => Ok(value),
            Reply::Error(error) => Err(anyhow!("workstation: {error}")),
        }
    }

    fn get<T: DeserializeOwned>(&self, request: MuxRequest) -> Result<T> {
        Ok(serde_json::from_value(self.call(request)?)?)
    }

    fn act(&self, request: MuxRequest) -> Result<()> {
        self.call(request).map(drop)
    }

    fn login_shell(&self, workdir: &Path) -> Result<RemoteTty> {
        Ok(RemoteTty {
            argv: vec![self.get_default_shell()?, "-l".to_string()],
            workdir: workdir.display().to_string(),
        })
    }

    fn tty(&self, workdir: &Path, command: Option<&str>) -> Result<RemoteTty> {
        match command {
            Some(script) => Ok(RemoteTty {
                argv: ["sh", "-c", script].map(String::from).to_vec(),
                workdir: workdir.display().to_string(),
            }),
            None => self.login_shell(workdir),
        }
    }
}

fn unsupported(what: &str) -> anyhow::Error {
    anyhow!("{what} is unsupported on remote")
}

impl Multiplexer for ForwardingMux {
    fn name(&self) -> &'static str {
        "tmux"
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
        self.get(MuxRequest::CreateWindow {
            name: util::prefixed(params.prefix, params.name),
            after_window: params.after_window.map(str::to_string),
            tty: self.login_shell(params.cwd)?,
        })
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        self.get(MuxRequest::CreateSession {
            name: util::prefixed(params.prefix, params.name),
            initial_window_name: params.initial_window_name.map(str::to_string),
            tty: self.login_shell(params.cwd)?,
        })
    }

    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        self.get(MuxRequest::CreateWindowInSession {
            session: params.session_name.to_string(),
            name: params.name.map(str::to_string),
            tty: self.login_shell(params.cwd)?,
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
        self.act(MuxRequest::KillSession {
            name: full_name.to_string(),
        })
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        self.act(MuxRequest::KillWindow {
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

    fn schedule_window_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
        Err(unsupported("scheduling a window close"))
    }

    fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
        Err(unsupported("scheduling a session close"))
    }

    fn run_deferred_script(&self, _script: &str) -> Result<()> {
        Err(unsupported("running a deferred script"))
    }

    fn shell_select_window_cmd(&self, _full_name: &str) -> Result<String> {
        Err(unsupported("a shell command for the workstation's tmux"))
    }

    fn shell_kill_window_cmd(&self, _full_name: &str) -> Result<String> {
        Err(unsupported("a shell command for the workstation's tmux"))
    }

    fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(unsupported("a shell command for the workstation's tmux"))
    }

    fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(unsupported("a shell command for the workstation's tmux"))
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
        self.act(MuxRequest::KillPane {
            pane_id: pane_id.to_string(),
        })
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        self.get(MuxRequest::RespawnPane {
            pane_id: pane_id.to_string(),
            tty: self.tty(cwd, cmd)?,
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
        self.get(MuxRequest::SplitPane {
            target_pane_id: target_pane_id.to_string(),
            direction: direction.clone(),
            size,
            percentage,
            tty: self.tty(cwd, command)?,
        })
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
    use crate::remote::forwarded::WINDOW_TAG;
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
        }
    }

    /// A command running in a command session on `transport`'s remote,
    /// invoked from `invocation`. Yields the forwarding multiplexer it was
    /// given, then holds the session open until `release` exists.
    fn command_session(
        transport: Arc<ServeTransport>,
        invocation: InvocationContext,
        project: &std::path::Path,
        release: &std::path::Path,
    ) -> (ForwardingMux, std::thread::JoinHandle<Result<i32>>) {
        let channels = Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            ChannelPolicy::default(),
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
        };
        let (env, received) = mpsc::channel();
        let session = std::thread::spawn(move || {
            channels.run_command(
                "devbox",
                &remote(),
                &request,
                &invocation,
                &mut |stream, data| {
                    if stream == Stream::Stdout {
                        let _ = env.send(String::from_utf8_lossy(data).into_owned());
                    }
                },
            )
        });
        let line = received
            .recv_timeout(Duration::from_secs(20))
            .expect("the command printed its forwarding environment");
        let (socket, session_id) = line.trim().split_once(' ').unwrap();
        (
            ForwardingMux::new(PathBuf::from(socket), session_id.parse().unwrap()),
            session,
        )
    }

    #[test]
    fn a_command_on_the_remote_creates_a_project_window_in_workstation_tmux() {
        let Some(server) = PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
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
        );

        assert_eq!(mux.current_pane_id(), Some(invoking));
        assert!(!mux.session_exists("api").unwrap());
        let pane = mux
            .create_session(CreateSessionParams {
                prefix: "",
                name: "api",
                cwd: temp.path(),
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
        );
        let forged = ForwardingMux::new(mux.socket.clone(), mux.session + 100);

        let error = forged.session_exists("api").unwrap_err().to_string();

        assert!(error.contains("refused: no command session"), "{error}");
        std::fs::write(&release, "").unwrap();
        session.join().unwrap().unwrap();
    }

    #[test]
    fn without_the_control_channel_requests_fail_as_not_connected() {
        let temp = tempfile::tempdir().unwrap();
        let mux = ForwardingMux::new(temp.path().join("gone.sock"), 1);

        let error = mux.is_running().unwrap_err().to_string();

        assert!(error.starts_with("workstation not connected"), "{error}");
    }

    #[test]
    fn workflow_only_operations_are_unsupported_on_remote() {
        let temp = tempfile::tempdir().unwrap();
        let mux = ForwardingMux::new(temp.path().join("gone.sock"), 1);

        let error = mux
            .schedule_window_close("wm-x", Duration::from_secs(1))
            .unwrap_err()
            .to_string();

        assert_eq!(error, "scheduling a window close is unsupported on remote");
    }
}
