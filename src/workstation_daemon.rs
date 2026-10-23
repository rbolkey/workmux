//! Workstation daemon: the long-lived workstation process for one tmux server.
//!
//! Clients reach it over a per-instance Unix socket. The sidebar subscribes to
//! snapshot pushes; CLI commands make request/response calls in
//! newline-delimited JSON with a JSON-RPC-shaped envelope (ADR 0001).

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::multiplexer::{create_backend, detect_backend};

/// Version of the request/response protocol on the daemon socket.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on one request or response line.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// How long a client waits for the daemon to answer one call.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// `None` when the request could not be parsed far enough to read its id.
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseError {
    pub message: String,
}

/// Answer to a `ping` request: who is serving this socket.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ping {
    pub version: String,
    pub protocol: u32,
    pub pid: u32,
}

impl Response {
    fn ok(id: u64, result: Value) -> Self {
        Self {
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<u64>, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(ResponseError {
                message: message.into(),
            }),
        }
    }
}

fn dispatch(request: &Request) -> Response {
    match request.method.as_str() {
        "ping" => Response::ok(
            request.id,
            serde_json::to_value(Ping {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol: PROTOCOL_VERSION,
                pid: std::process::id(),
            })
            .expect("ping serializes"),
        ),
        "remote.status" => {
            let name = request.params.get("name").and_then(Value::as_str);
            let statuses = crate::remote::registry::remotes().and_then(|remotes| {
                crate::remote::statuses(crate::remote::channels(), &remotes, name)
            });
            match statuses.and_then(|statuses| Ok(serde_json::to_value(statuses)?)) {
                Ok(result) => Response::ok(request.id, result),
                Err(error) => Response::err(Some(request.id), format!("{error:#}")),
            }
        }
        method => Response::err(Some(request.id), format!("unknown method: {method}")),
    }
}

fn answer(line: &[u8]) -> Response {
    match serde_json::from_slice::<Request>(line) {
        Ok(request) => dispatch(&request),
        Err(error) => Response::err(
            serde_json::from_slice::<Value>(line)
                .ok()
                .and_then(|value| value.get("id")?.as_u64()),
            format!("invalid request: {error}"),
        ),
    }
}

/// Read one newline-terminated line of at most `MAX_LINE_BYTES`.
/// Returns `Ok(None)` at a clean end of stream.
pub(crate) fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read =
        std::io::Read::take(reader, MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(anyhow!(
            "workstation daemon line exceeds {MAX_LINE_BYTES} bytes"
        ));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(Some(line))
}

fn write_message(writer: &mut impl Write, message: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

/// Serve request/response calls on one client connection until it closes.
///
/// `first_line` is a request already read off the stream while the daemon
/// was deciding what kind of client connected.
pub(crate) fn serve_requests(stream: UnixStream, first_line: Option<Vec<u8>>) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    if let Some(line) = first_line {
        write_message(&mut writer, &answer(&line))?;
    }
    while let Some(line) = read_line(&mut reader)? {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        write_message(&mut writer, &answer(&line))?;
    }
    Ok(())
}

/// Make one request/response call to the workstation daemon at `socket`.
pub fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    call_with_timeout(socket, method, params, CALL_TIMEOUT)
}

/// Make one call that may take up to `timeout`, such as one that opens
/// control channels.
pub fn call_with_timeout(
    socket: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to workstation daemon at {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(CALL_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let id = 1;
    write_message(
        &mut writer,
        &Request {
            id,
            method: method.to_string(),
            params,
        },
    )?;
    let line = read_line(&mut reader)?
        .ok_or_else(|| anyhow!("workstation daemon closed the connection without answering"))?;
    let response: Response =
        serde_json::from_slice(&line).context("workstation daemon sent an unreadable response")?;
    if response.id != Some(id) {
        return Err(anyhow!(
            "workstation daemon answered request {:?}, expected {id}",
            response.id
        ));
    }
    match (response.result, response.error) {
        (_, Some(error)) => Err(anyhow!("workstation daemon: {}", error.message)),
        (Some(result), None) => Ok(result),
        (None, None) => Ok(Value::Null),
    }
}

/// Notification a sidebar sends first to receive snapshot pushes.
pub const SUBSCRIBE_METHOD: &str = "subscribe";

/// How long the daemon waits for a new client's first line before treating
/// it as a sidebar from before the workstation daemon, which sends nothing.
const LEGACY_SUBSCRIBER_WAIT: Duration = Duration::from_millis(200);

/// What a newly accepted client wants from the daemon.
pub(crate) enum Connection {
    /// Snapshot pushes, framed as the sidebar has always read them.
    Subscriber,
    /// Request/response calls; carries the first request line.
    Requests(Vec<u8>),
}

/// Decide what a newly accepted client wants by reading its first line.
pub(crate) fn classify(stream: &UnixStream) -> Result<Connection> {
    use std::io::Read;

    stream.set_read_timeout(Some(LEGACY_SUBSCRIBER_WAIT))?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    // Byte at a time so nothing past the first line is consumed.
    loop {
        match (&*stream).read(&mut byte) {
            Ok(0) => return Err(anyhow!("client closed before sending a request")),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() >= MAX_LINE_BYTES => {
                return Err(anyhow!(
                    "workstation daemon line exceeds {MAX_LINE_BYTES} bytes"
                ));
            }
            Ok(_) => line.push(byte[0]),
            Err(error)
                if line.is_empty()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
            {
                stream.set_read_timeout(None)?;
                return Ok(Connection::Subscriber);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    stream.set_read_timeout(None)?;

    let is_subscribe = serde_json::from_slice::<Value>(&line)
        .ok()
        .is_some_and(|value| value.get("method").and_then(Value::as_str) == Some(SUBSCRIBE_METHOD));
    Ok(if is_subscribe {
        Connection::Subscriber
    } else {
        Connection::Requests(line)
    })
}

/// The line a sidebar sends to subscribe to snapshot pushes.
pub fn subscribe_line() -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({ "method": SUBSCRIBE_METHOD }))
        .expect("subscribe serializes");
    line.push(b'\n');
    line
}

/// Compute the workstation daemon socket path for a multiplexer instance.
pub fn socket_path(instance_id: &str) -> PathBuf {
    // FNV-1a provides a stable fixed-width key across clients and the daemon
    // while keeping long tmux socket paths below Unix socket limits. The
    // `sidebar` file name predates the workstation daemon and is kept so a
    // daemon started by an older binary is still found.
    let mut key = 0xcbf29ce484222325u64;
    for byte in instance_id.as_bytes() {
        key ^= u64::from(*byte);
        key = key.wrapping_mul(0x100000001b3);
    }
    std::env::temp_dir().join(format!("workmux-sidebar-{key:016x}.sock"))
}

/// Ensure the workstation daemon for the current tmux server is running,
/// starting it if absent. Returns its socket path.
pub fn ensure_running() -> Result<PathBuf> {
    let mux = create_backend(detect_backend());
    let socket = socket_path(&mux.instance_id());
    ensure_running_at(&socket, spawn_daemon, Duration::from_secs(2))?;
    Ok(socket)
}

/// Like `ensure_running`, but first stops a daemon from another workmux
/// release, which may not understand newer requests.
pub fn ensure_current() -> Result<PathBuf> {
    let mux = create_backend(detect_backend());
    let socket = socket_path(&mux.instance_id());
    ensure_current_at(&socket, stop_daemon, spawn_daemon, Duration::from_secs(2))?;
    Ok(socket)
}

fn ensure_current_at(
    socket: &Path,
    stop: impl FnOnce(Option<u32>) -> Result<()>,
    start: impl FnOnce() -> Result<()>,
    timeout: Duration,
) -> Result<()> {
    if UnixStream::connect(socket).is_ok() {
        let stale_pid = match ping(socket) {
            Ok(ping) if ping.version == env!("CARGO_PKG_VERSION") => return Ok(()),
            Ok(ping) => {
                tracing::info!(
                    version = ping.version,
                    pid = ping.pid,
                    "restarting workstation daemon from another release"
                );
                Some(ping.pid)
            }
            Err(error) => {
                tracing::info!(%error, "restarting workstation daemon that failed its ping");
                None
            }
        };
        stop(stale_pid)?;
        let started = Instant::now();
        while UnixStream::connect(socket).is_ok() {
            if started.elapsed() >= timeout {
                return Err(anyhow!(
                    "workstation daemon from another release did not stop"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    ensure_running_at(socket, start, timeout)
}

/// Stop a running daemon: by the pid it reported, else the pid it recorded
/// in tmux. Waits for it to exit so it cannot remove its successor's socket.
fn stop_daemon(pid: Option<u32>) -> Result<()> {
    let pid = match pid {
        Some(pid) => pid,
        None => {
            let mux = create_backend(detect_backend());
            crate::multiplexer::TmuxBackend::for_socket(&mux.instance_id())
                .global_option("@workmux_sidebar_daemon_pid")
                .ok()
                .flatten()
                .and_then(|pid| pid.trim().parse().ok())
                .ok_or_else(|| {
                    anyhow!("a workstation daemon from another release is running and its pid is unknown")
                })?
        }
    };
    let pid = pid as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn ensure_running_at(
    socket: &Path,
    start: impl FnOnce() -> Result<()>,
    timeout: Duration,
) -> Result<()> {
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }

    // Stale socket from a crashed daemon
    let _ = std::fs::remove_file(socket);
    start()?;
    let started = Instant::now();
    while started.elapsed() < timeout {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!("workstation daemon failed to start"))
}

/// Spawn the workstation daemon as a detached background process.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("_workstation-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Run the workstation daemon (called by the hidden `_workstation-daemon` command).
pub fn run() -> Result<()> {
    crate::command::sidebar::run_daemon()
}

/// Start the workstation daemon if absent, ping it and print the answer as
/// JSON (called by the hidden `_workstation-daemon-ping` command).
pub fn run_ping() -> Result<()> {
    let socket = ensure_running()?;
    println!("{}", serde_json::to_string(&ping(&socket)?)?);
    Ok(())
}

/// Ask the workstation daemon at `socket` for its version and protocol.
pub fn ping(socket: &Path) -> Result<Ping> {
    let result = call(socket, "ping", Value::Null)?;
    serde_json::from_value(result).context("workstation daemon sent an unreadable ping")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn serve_one(listener: UnixListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_requests(stream, None).unwrap();
        })
    }

    #[test]
    fn ping_reports_version_protocol_and_pid() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = serve_one(UnixListener::bind(&socket).unwrap());

        let ping = ping(&socket).unwrap();

        assert_eq!(ping.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(ping.protocol, 1);
        assert_eq!(ping.pid, std::process::id());
        server.join().unwrap();
    }

    #[test]
    fn unknown_method_is_an_error_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = serve_one(UnixListener::bind(&socket).unwrap());

        let error = call(&socket, "frobnicate", Value::Null).unwrap_err();

        assert!(error.to_string().contains("unknown method: frobnicate"));
        server.join().unwrap();
    }

    #[test]
    fn ensure_running_starts_an_absent_daemon_once() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        // A stale socket file left by a crashed daemon.
        drop(UnixListener::bind(&socket).unwrap());
        let starts = std::sync::atomic::AtomicUsize::new(0);

        let start = || {
            starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::fs::remove_file(&socket).ok();
            let listener = UnixListener::bind(&socket)?;
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    serve_requests(stream.unwrap(), None).unwrap();
                }
            });
            Ok(())
        };
        ensure_running_at(&socket, start, Duration::from_secs(2)).unwrap();
        ensure_running_at(&socket, start, Duration::from_secs(2)).unwrap();

        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(ping(&socket).unwrap().protocol, PROTOCOL_VERSION);
    }

    /// Serve `ping` on `socket` as a daemon of release `version` would,
    /// until told to stop.
    fn fake_daemon(socket: &Path, version: &'static str) -> std::sync::mpsc::Sender<()> {
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let socket = socket.to_path_buf();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            while stop_rx.try_recv().is_err() {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                if let Ok(Some(line)) = read_line(&mut reader) {
                    let request: Request = serde_json::from_slice(&line).unwrap();
                    let ping = Ping {
                        version: version.to_string(),
                        protocol: PROTOCOL_VERSION,
                        pid: 4242,
                    };
                    let response = Response::ok(request.id, serde_json::to_value(ping).unwrap());
                    write_message(&mut writer, &response).unwrap();
                }
            }
            std::fs::remove_file(socket).ok();
        });
        stop_tx
    }

    #[test]
    fn a_daemon_from_another_release_is_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let old = fake_daemon(&socket, "0.0.1");
        let stopped = std::sync::Mutex::new(Vec::new());

        ensure_current_at(
            &socket,
            |pid| {
                stopped.lock().unwrap().push(pid);
                old.send(()).unwrap();
                Ok(())
            },
            || {
                // Leak the new daemon's stop handle: it serves until the test ends.
                std::mem::forget(fake_daemon(&socket, env!("CARGO_PKG_VERSION")));
                Ok(())
            },
            Duration::from_secs(2),
        )
        .unwrap();

        assert_eq!(*stopped.lock().unwrap(), vec![Some(4242)]);
        assert_eq!(ping(&socket).unwrap().version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_current_daemon_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let _current = fake_daemon(&socket, env!("CARGO_PKG_VERSION"));

        ensure_current_at(
            &socket,
            |_| panic!("a current daemon must not be stopped"),
            || panic!("a current daemon must not be restarted"),
            Duration::from_secs(2),
        )
        .unwrap();
    }

    #[test]
    fn ensure_running_reports_a_daemon_that_never_listens() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");

        let error = ensure_running_at(&socket, || Ok(()), Duration::from_millis(100)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("workstation daemon failed to start")
        );
    }

    #[test]
    fn socket_path_uses_a_stable_fixed_width_instance_key() {
        let default = socket_path("/private/tmp/tmux-501/default");
        let long = socket_path("/private/tmp/tmux-501/sidebar-repro-socket");
        let unicode = socket_path("/private/tmp/tmux-501/サイドバー");

        assert_eq!(default.parent(), Some(std::env::temp_dir().as_path()));
        assert_eq!(
            default.file_name().unwrap(),
            "workmux-sidebar-3d3d4a9387e30f49.sock"
        );
        assert_eq!(
            long.file_name().unwrap(),
            "workmux-sidebar-51c46e2d6ee045f1.sock"
        );
        assert_eq!(
            unicode.file_name().unwrap(),
            "workmux-sidebar-b596df9918d556c8.sock"
        );
    }

    #[test]
    fn socket_path_for_a_long_instance_can_be_bound() {
        let unique = tempfile::tempdir().unwrap();
        let instance_id = format!(
            "/private/tmp/tmux-501/{}/{}",
            unique.path().display(),
            "long-unicode-name-サイドバー".repeat(8)
        );
        let path = socket_path(&instance_id);

        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        std::fs::remove_file(path).unwrap();
    }
}
