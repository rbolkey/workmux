//! Workstation daemon: the long-lived workstation process for one tmux server.
//!
//! Clients reach it over a per-instance Unix socket. The sidebar subscribes to
//! snapshot pushes; CLI commands make request/response calls in
//! newline-delimited JSON with a JSON-RPC-shaped envelope (ADR 0001).

use anyhow::{Context, Result, anyhow, bail};
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
        crate::remote::dashboard::DAEMON_METHOD => match remote_dashboard(&request.params) {
            Ok(result) => Response::ok(request.id, result),
            Err(error) => Response::err(Some(request.id), format!("{error:#}")),
        },
        method => Response::err(Some(request.id), format!("unknown method: {method}")),
    }
}

#[derive(Deserialize)]
struct DashboardParams {
    remote: String,
    project: String,
    request: crate::remote::dashboard::DashboardRequest,
    #[serde(default)]
    connect: bool,
}

/// Run a dashboard's operation on a remote project over its control channel.
fn remote_dashboard(params: &Value) -> Result<Value> {
    let params: DashboardParams = serde_json::from_value(params.clone())?;
    let project = crate::remote::router::remote_project_on(&params.remote, &params.project)?;
    crate::remote::channels().dashboard(
        project.host(),
        &project.site,
        &params.request,
        params.connect,
    )
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
            "Workstation daemon line exceeds {MAX_LINE_BYTES} bytes"
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
/// was deciding what kind of client connected. A command session takes the
/// rest of the connection.
pub(crate) fn serve_requests(stream: UnixStream, first_line: Option<Vec<u8>>) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    // Pane holds this client took; released when it disconnects.
    let mut holds = Vec::new();
    let mut next = first_line;
    loop {
        let line = match next.take() {
            Some(line) => line,
            None => match read_line(&mut reader)? {
                Some(line) => line,
                None => return Ok(()),
            },
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Request>(&line) {
            Ok(request) if request.method == COMMAND_SESSION_METHOD => {
                let response = command_session(&request, reader, &mut writer);
                return write_message(&mut writer, &response);
            }
            Ok(request) if request.method == PANE_HOLD_METHOD => {
                let response = match pane_hold(&request) {
                    Ok(hold) => {
                        holds.push(hold);
                        Response::ok(request.id, Value::Null)
                    }
                    Err(error) => Response::err(Some(request.id), format!("{error:#}")),
                };
                write_message(&mut writer, &response)?;
            }
            _ => write_message(&mut writer, &answer(&line))?,
        }
    }
}

/// Method a `remote attach` loop calls to keep its remote's control channel
/// open while its connection stays open: `{remote}`.
pub const PANE_HOLD_METHOD: &str = "remote.hold";

fn pane_hold(request: &Request) -> Result<crate::remote::channels::PaneHold> {
    let name = request
        .params
        .get("remote")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("remote.hold needs a remote"))?;
    let remotes = crate::remote::registry::remotes()?;
    let config = remotes
        .get(name)
        .ok_or_else(|| anyhow!("No remote named {name}"))?;
    crate::remote::channels().hold_for_pane(crate::remote::Remote::new(name, config))
}

/// Keep `remote`'s control channel open for an attached pane until the
/// returned connection is dropped.
pub fn hold_for_pane(socket: &Path, remote: &str) -> Result<UnixStream> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "Failed to connect to the workstation daemon at {}",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(CALL_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    write_message(
        &mut writer,
        &Request {
            id: 1,
            method: PANE_HOLD_METHOD.to_string(),
            params: serde_json::json!({ "remote": remote }),
        },
    )?;
    let line = read_line(&mut BufReader::new(&stream))?
        .ok_or_else(|| anyhow!("Workstation daemon closed the connection without answering"))?;
    let response: Response =
        serde_json::from_slice(&line).context("Workstation daemon sent an unreadable response")?;
    if let Some(error) = response.error {
        bail!("Workstation daemon: {}", error.message);
    }
    Ok(stream)
}

/// Method that runs a command session on a remote: `{remote, request}`.
/// The daemon streams the command's output as `command.output`
/// notifications and asks for a prompt's answer with a `command.prompt`
/// notification, which the client answers with a `command.answer`
/// notification `{line}`. The client closing the connection cancels the
/// command.
pub const COMMAND_SESSION_METHOD: &str = "command.session";

/// Notification a command session's client sends with a prompt's answer.
pub const ANSWER_METHOD: &str = "command.answer";

/// A notification the daemon sends a client before its response.
#[derive(Debug, Serialize, Deserialize)]
struct Notification {
    method: String,
    params: Value,
}

#[derive(Deserialize)]
struct CommandSessionParams {
    remote: String,
    request: crate::remote::session::CommandRequest,
    /// Where the command was run from on the workstation.
    #[serde(default)]
    invocation: crate::multiplexer::InvocationContext,
    /// The client has just found the remote running, so its state is not
    /// checked again.
    #[serde(default)]
    running: bool,
}

/// The terminal of a command session run for a client of the daemon.
struct ClientTerminal<'a, W: Write> {
    writer: &'a mut W,
    answers: std::sync::mpsc::Receiver<Option<String>>,
    /// Set once the client has gone away.
    gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<W: Write> ClientTerminal<'_, W> {
    fn notify(&mut self, method: &str, params: Value) {
        let notification = Notification {
            method: method.to_string(),
            params,
        };
        if write_message(self.writer, &notification).is_err() {
            self.gone.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl<W: Write> crate::remote::session::Terminal for ClientTerminal<'_, W> {
    fn output(&mut self, stream: crate::remote::session::Stream, data: &[u8]) {
        if self.cancelled() {
            return;
        }
        self.notify(
            crate::remote::session::OUTPUT_METHOD,
            serde_json::to_value(crate::remote::session::OutputParams::new(0, stream, data))
                .expect("serializes"),
        );
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        if self.cancelled() {
            return Ok(None);
        }
        self.notify(crate::remote::session::PROMPT_METHOD, Value::Null);
        Ok(self.answers.recv().unwrap_or(None))
    }

    fn cancelled(&self) -> bool {
        self.gone.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Read a command session client's answers until it closes the
/// connection, which marks it `gone`.
fn read_answers(
    mut reader: BufReader<UnixStream>,
    answers: std::sync::mpsc::Sender<Option<String>>,
    gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    while let Ok(Some(line)) = read_line(&mut reader) {
        match serde_json::from_slice::<Notification>(&line) {
            Ok(notification) if notification.method == ANSWER_METHOD => {
                let answer =
                    serde_json::from_value::<crate::remote::session::Answer>(notification.params)
                        .map(|answer| answer.line)
                        .unwrap_or(None);
                let _ = answers.send(answer);
            }
            _ => tracing::warn!("ignoring an unexpected line from a command session client"),
        }
    }
    gone.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn command_session(
    request: &Request,
    reader: BufReader<UnixStream>,
    writer: &mut impl Write,
) -> Response {
    let run = || -> Result<Value> {
        let params: CommandSessionParams = serde_json::from_value(request.params.clone())?;
        let project =
            crate::remote::router::remote_project_on(&params.remote, &params.request.project.name)?;
        if params.request.project.remote != params.remote {
            bail!(
                "Project {} is not on remote {}",
                params.request.project.name,
                params.remote
            );
        }
        let gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (answered, answers) = std::sync::mpsc::channel();
        {
            let gone = gone.clone();
            std::thread::spawn(move || read_answers(reader, answered, gone));
        }
        // A client that has gone away cancels the command.
        let exit_code = crate::remote::channels().run_command(
            project.host(),
            &params.request,
            &params.invocation,
            &mut ClientTerminal {
                writer: &mut *writer,
                answers,
                gone,
            },
            params.running,
        )?;
        Ok(serde_json::json!({ "exit_code": exit_code }))
    };
    match run() {
        Ok(result) => Response::ok(request.id, result),
        Err(error) => Response::err(Some(request.id), format!("{error:#}")),
    }
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
    call_streaming(socket, method, params, Some(timeout), &mut |_, _| None)
}

/// Make one call, passing each notification the daemon sends before its
/// response to `on_notification`; what it returns is sent back as an
/// `ANSWER_METHOD` notification. `timeout` bounds each wait for a line;
/// `None` waits as long as the call runs.
pub fn call_streaming(
    socket: &Path,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
    on_notification: &mut dyn FnMut(&str, Value) -> Option<Value>,
) -> Result<Value> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "Failed to connect to the workstation daemon at {}",
            socket.display()
        )
    })?;
    stream.set_read_timeout(timeout)?;
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
    let response = loop {
        let line = read_line(&mut reader)?
            .ok_or_else(|| anyhow!("Workstation daemon closed the connection without answering"))?;
        if let Ok(notification) = serde_json::from_slice::<Notification>(&line) {
            if let Some(answer) = on_notification(&notification.method, notification.params) {
                write_message(
                    &mut writer,
                    &Notification {
                        method: ANSWER_METHOD.to_string(),
                        params: answer,
                    },
                )?;
            }
            continue;
        }
        break serde_json::from_slice::<Response>(&line)
            .context("Workstation daemon sent an unreadable response")?;
    };
    if response.id != Some(id) {
        return Err(anyhow!(
            "Workstation daemon answered request {:?}, expected {id}",
            response.id
        ));
    }
    match (response.result, response.error) {
        (_, Some(error)) => Err(anyhow!("Workstation daemon: {}", error.message)),
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
            Ok(0) => return Err(anyhow!("Client closed before sending a request")),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() >= MAX_LINE_BYTES => {
                return Err(anyhow!(
                    "Workstation daemon line exceeds {MAX_LINE_BYTES} bytes"
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
                    "Workstation daemon from another release did not stop"
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
                    anyhow!("A workstation daemon from another release is running and its pid is unknown")
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
    Err(anyhow!("Workstation daemon failed to start"))
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
    serde_json::from_value(result).context("Workstation daemon sent an unreadable ping")
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
        // A stale file left by a crashed daemon. A plain file stands in for
        // the dead socket: binding and dropping a real listener races with
        // other tests' child processes, which can briefly inherit its fd
        // between fork and exec and keep it accepting connections.
        std::fs::write(&socket, b"").unwrap();
        let starts = std::sync::atomic::AtomicUsize::new(0);

        let start = || {
            starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::fs::remove_file(&socket).ok();
            let listener = UnixListener::bind(&socket)?;
            std::thread::spawn(move || {
                // Like the real daemon, one client's failure (a probe that
                // hung up mid-read) must not take the listener down with it.
                for stream in listener.incoming().flatten() {
                    let _ = serve_requests(stream, None);
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
                .contains("Workstation daemon failed to start")
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
