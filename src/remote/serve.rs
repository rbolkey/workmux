//! `workmux remote serve`: remote workmux's end of the control channel.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::channel::{Handler, Peer};
use super::forwarding::{self, SocketGuard};
use super::handshake::{Hello, write_hello};
use super::{provision, session, shell, shims, status};

/// Answer to a `ping` health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub workmux: String,
    pub protocol: u32,
    pub pid: u32,
    /// Problems with this remote's setup, such as agents without hooks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Answers the workstation's requests. Holds this side's peer, set once the
/// channel starts, so a command session can stream output back.
#[derive(Clone, Default)]
struct RemoteHandler {
    peer: Arc<OnceLock<Peer>>,
    /// The socket commands forward multiplexer requests through.
    forwarding: Arc<OnceLock<Option<PathBuf>>>,
    running: session::Running,
}

impl Handler for RemoteHandler {
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            session::RUN_METHOD => session::serve_run(
                self.peer.wait(),
                self.forwarding.wait().as_deref(),
                &self.running,
                params,
            )
            .map_err(|error| format!("{error:#}")),
            shims::INSTALL_METHOD => {
                shims::serve_install(params).map_err(|error| format!("{error:#}"))
            }
            "ping" => {
                let hello = Hello::current();
                serde_json::to_value(Ping {
                    workmux: hello.workmux,
                    protocol: hello.protocol,
                    pid: std::process::id(),
                    warnings: provision::current_warnings(),
                })
                .map_err(|e| e.to_string())
            }
            method => Err(format!("unknown method: {method}")),
        }
    }

    fn notification(&self, method: &str, params: Value) {
        match method {
            session::CANCEL_METHOD => {
                match serde_json::from_value::<session::SessionParams>(params) {
                    Ok(params) => self.running.cancel(params.session),
                    Err(error) => tracing::warn!(%error, "ignoring an unreadable cancellation"),
                }
            }
            method => tracing::warn!(method, "ignoring unknown notification"),
        }
    }
}

/// Print the sentinel hello, then serve the control protocol until the
/// workstation closes the stream.
pub fn serve(
    reader: impl BufRead + Send + 'static,
    mut writer: impl Write + Send + 'static,
) -> Result<()> {
    // Bound before the hello, so remote shells are connected as soon as
    // the channel is open.
    let shell_path = shell::socket_path()
        .inspect_err(|error| tracing::warn!(error = %format!("{error:#}"), "no shell socket"))
        .ok();
    let shell_listener = shell_path.as_ref().and_then(|path| bind_shell_socket(path));
    write_hello(&mut writer, &Hello::current())?;
    let handler = RemoteHandler::default();
    let peer = Peer::start(reader, writer, handler.clone());
    let _ = handler.peer.set(peer.clone());
    // Without the socket, commands still run; their multiplexer requests
    // cannot reach the workstation.
    let socket = forwarding::listen(peer.clone())
        .inspect_err(|error| tracing::warn!(error = %format!("{error:#}"), "no forwarding socket"))
        .ok();
    let _ = handler
        .forwarding
        .set(socket.as_ref().map(|socket| socket.path.clone()));
    let shell = Arc::new(Mutex::new(None));
    if let Some((listener, guard)) = shell_listener {
        forwarding::relay_from(listener, peer.clone());
        *shell.lock().unwrap() = Some(guard);
    }
    if let Some(path) = shell_path {
        claim_shell_socket(peer.clone(), path, shell.clone());
    }
    match status::StatusFiles::for_this_remote() {
        Ok(files) => {
            let peer = peer.clone();
            std::thread::spawn(move || status::send_until_closed(&peer, &files));
        }
        Err(error) => tracing::warn!(error = %format!("{error:#}"), "no status files"),
    }
    while !peer.wait_closed(Duration::from_secs(3600)) {}
    tracing::info!(reason = ?peer.closed_reason(), "control channel closed");
    // Remote shells are no longer connected.
    shell.lock().unwrap().take();
    // Nothing can read their output or answer their prompts any more.
    handler.running.shut_down();
    Ok(())
}

/// How often a `remote serve` without the shell socket checks whether the
/// one holding it has gone.
const SHELL_SOCKET_CLAIM: Duration = Duration::from_secs(2);

/// Bind the shell socket at `path`, unless another `remote serve` is
/// listening there.
fn bind_shell_socket(path: &Path) -> Option<(UnixListener, SocketGuard)> {
    if UnixStream::connect(path).is_ok() {
        return None;
    }
    let bound = path
        .parent()
        .map_or(Ok(()), |dir| {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
        })
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            let _ = std::fs::remove_file(path);
            forwarding::bind(path.to_path_buf())
        });
    bound
        .inspect_err(|error| {
            tracing::warn!(error = %format!("{error:#}"), "failed to claim the shell socket")
        })
        .ok()
}

/// While `peer` is open, take over the shell socket at `path` whenever the
/// `remote serve` holding it has gone.
fn claim_shell_socket(peer: Peer, path: PathBuf, held: Arc<Mutex<Option<SocketGuard>>>) {
    std::thread::spawn(move || {
        while !peer.wait_closed(SHELL_SOCKET_CLAIM) {
            let mut guard = held.lock().unwrap();
            if guard.is_none()
                && let Some((listener, socket)) = bind_shell_socket(&path)
            {
                forwarding::relay_from(listener, peer.clone());
                *guard = Some(socket);
            }
        }
    });
}

/// Run `workmux remote serve` on this process's stdin and stdout.
pub fn run() -> Result<()> {
    // A Ctrl-C meant for the workstation command reaches a loopback remote
    // in the same process group. Outlive it, so the commands it runs are
    // cancelled once the workstation goes away.
    signal_hook::flag::register(
        signal_hook::consts::SIGINT,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    serve(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}
