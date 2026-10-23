//! `workmux remote serve`: remote workmux's end of the control channel.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::time::Duration;

use super::channel::{Handler, Peer};
use super::handshake::{Hello, write_hello};

/// Answer to a `ping` health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub workmux: String,
    pub protocol: u32,
    pub pid: u32,
}

struct RemoteHandler;

impl Handler for RemoteHandler {
    fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
        match method {
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
}

/// Print the sentinel hello, then serve the control protocol until the
/// workstation closes the stream.
pub fn serve(
    reader: impl BufRead + Send + 'static,
    mut writer: impl Write + Send + 'static,
) -> Result<()> {
    write_hello(&mut writer, &Hello::current())?;
    let peer = Peer::start(reader, writer, RemoteHandler);
    while !peer.wait_closed(Duration::from_secs(3600)) {}
    tracing::info!(reason = ?peer.closed_reason(), "control channel closed");
    Ok(())
}

/// Run `workmux remote serve` on this process's stdin and stdout.
pub fn run() -> Result<()> {
    serve(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}
