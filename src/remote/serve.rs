//! `workmux remote serve`: remote workmux's end of the control channel.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::channel::{Handler, Peer};
use super::handshake::{Hello, write_hello};
use super::{forwarding, session};

/// Answer to a `ping` health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub workmux: String,
    pub protocol: u32,
    pub pid: u32,
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
            "ping" => {
                let hello = Hello::current();
                serde_json::to_value(Ping {
                    workmux: hello.workmux,
                    protocol: hello.protocol,
                    pid: std::process::id(),
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
    while !peer.wait_closed(Duration::from_secs(3600)) {}
    tracing::info!(reason = ?peer.closed_reason(), "control channel closed");
    // Nothing can read their output or answer their prompts any more.
    handler.running.shut_down();
    Ok(())
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
