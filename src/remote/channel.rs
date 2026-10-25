//! Control channel peer: newline-delimited JSON in a JSON-RPC-shaped envelope
//! over any byte stream (ADR 0001).
//!
//! Each side runs a reader thread, a writer thread and a pending-request
//! table, so either side can send requests concurrently and block on its own
//! response. A message without an `id` is a notification.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Upper bound on one control channel line.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Answers requests and notifications from the other side.
///
/// Each request runs on its own thread, so a handler may block, including
/// on a request of its own to the other side.
pub trait Handler: Send + Sync + 'static {
    fn request(&self, method: &str, params: Value) -> Result<Value, String>;

    fn notification(&self, _method: &str, _params: Value) {}
}

/// One control channel message. Requests carry `method` and `id`,
/// notifications `method` only, responses `id` with `result` or `error`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireError {
    message: String,
}

type Answer = Result<Value, String>;

struct Inner {
    outgoing: Mutex<Option<Sender<Vec<u8>>>>,
    pending: Mutex<HashMap<u64, SyncSender<Answer>>>,
    next_id: AtomicU64,
    /// Why the channel closed, once it has.
    closed: Mutex<Option<String>>,
    closed_changed: Condvar,
}

impl Inner {
    fn send(&self, message: &Message) -> Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        let outgoing = self.outgoing.lock().unwrap();
        match outgoing.as_ref() {
            Some(sender) if sender.send(line).is_ok() => Ok(()),
            _ => Err(self.closed_error()),
        }
    }

    fn closed_error(&self) -> anyhow::Error {
        match self.closed.lock().unwrap().as_deref() {
            Some(reason) => anyhow!("Control channel closed: {reason}"),
            None => anyhow!("Control channel closed"),
        }
    }

    /// Mark the channel closed, failing every pending request.
    fn shut(&self, reason: &str) {
        self.outgoing.lock().unwrap().take();
        {
            let mut closed = self.closed.lock().unwrap();
            if closed.is_none() {
                *closed = Some(reason.to_string());
            }
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, waiter) in pending {
            let _ = waiter.try_send(Err(format!("control channel closed: {reason}")));
        }
        self.closed_changed.notify_all();
    }
}

/// One side of a control channel.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl Peer {
    /// Start serving the channel on `reader` and `writer`.
    pub fn start(
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        handler: impl Handler,
    ) -> Self {
        let (outgoing, lines) = mpsc::channel();
        let inner = Arc::new(Inner {
            outgoing: Mutex::new(Some(outgoing)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: Mutex::new(None),
            closed_changed: Condvar::new(),
        });
        let handler = Arc::new(handler);

        let writer_inner = inner.clone();
        std::thread::spawn(move || write_loop(writer, lines, &writer_inner));
        let reader_inner = inner.clone();
        std::thread::spawn(move || read_loop(reader, &reader_inner, &handler));

        Self { inner }
    }

    /// Send a request and wait up to `timeout` for its answer.
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (waiter, answer) = mpsc::sync_channel(1);
        self.inner.pending.lock().unwrap().insert(id, waiter);
        let sent = self.inner.send(&Message {
            id: Some(id),
            method: Some(method.to_string()),
            params: Some(params),
            ..Default::default()
        });
        if let Err(error) = sent {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let answer = answer.recv_timeout(timeout);
        self.inner.pending.lock().unwrap().remove(&id);
        match answer {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
                "{method} timed out after {}s on the control channel",
                timeout.as_secs_f32()
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.inner.closed_error()),
        }
    }

    /// Send a notification, which gets no answer.
    #[allow(dead_code)] // Used once status records and streamed output flow
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.inner.send(&Message {
            method: Some(method.to_string()),
            params: Some(params),
            ..Default::default()
        })
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.lock().unwrap().is_some()
    }

    /// Why the channel closed, if it has.
    pub fn closed_reason(&self) -> Option<String> {
        self.inner.closed.lock().unwrap().clone()
    }

    /// Wait up to `timeout` for the channel to close. Returns whether it did.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let closed = self.inner.closed.lock().unwrap();
        let (closed, _) = self
            .inner
            .closed_changed
            .wait_timeout_while(closed, timeout, |closed| closed.is_none())
            .unwrap();
        closed.is_some()
    }

    /// Stop sending. The writer is dropped once queued lines are written,
    /// which closes a child's stdin.
    pub fn close(&self) {
        self.inner.shut("closed by this side");
    }
}

fn write_loop(mut writer: impl Write, lines: Receiver<Vec<u8>>, inner: &Inner) {
    for line in lines {
        if let Err(error) = writer.write_all(&line).and_then(|()| writer.flush()) {
            inner.shut(&format!("write failed: {error}"));
            return;
        }
    }
}

fn read_loop(mut reader: impl BufRead, inner: &Arc<Inner>, handler: &Arc<impl Handler>) {
    let reason = loop {
        let line = match read_line(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => break "the other side ended the stream".to_string(),
            Err(error) => break format!("read failed: {error}"),
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message: Message = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "control channel: ignoring unreadable message");
                continue;
            }
        };
        dispatch(message, inner, handler);
    };
    inner.shut(&reason);
}

fn dispatch(message: Message, inner: &Arc<Inner>, handler: &Arc<impl Handler>) {
    match (message.id, message.method) {
        (Some(id), Some(method)) => {
            let inner = inner.clone();
            let handler = handler.clone();
            std::thread::spawn(move || {
                let answer = handler.request(&method, message.params.unwrap_or(Value::Null));
                let response = match answer {
                    Ok(result) => Message {
                        id: Some(id),
                        result: Some(result),
                        ..Default::default()
                    },
                    Err(message) => Message {
                        id: Some(id),
                        error: Some(WireError { message }),
                        ..Default::default()
                    },
                };
                let _ = inner.send(&response);
            });
        }
        (None, Some(method)) => {
            handler.notification(&method, message.params.unwrap_or(Value::Null));
        }
        (Some(id), None) => {
            let answer = match message.error {
                Some(error) => Err(error.message),
                None => Ok(message.result.unwrap_or(Value::Null)),
            };
            if let Some(waiter) = inner.pending.lock().unwrap().remove(&id) {
                let _ = waiter.try_send(answer);
            }
        }
        (None, None) => tracing::warn!("control channel: ignoring message with no id or method"),
    }
}

/// Read one newline-terminated line, without its newline or a trailing
/// `\r`. Returns `Ok(None)` at a clean end of stream.
pub(crate) fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read =
        std::io::Read::take(reader, MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(anyhow!(
            "Control channel line exceeds {MAX_LINE_BYTES} bytes"
        ));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Instant;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// `echo` answers its params, `sleep` answers after `params.ms`, and
    /// `ask_back` asks the other side `echo` before answering.
    #[derive(Default)]
    struct TestHandler {
        peer: Arc<OnceLock<Peer>>,
        notifications: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl Handler for TestHandler {
        fn request(&self, method: &str, params: Value) -> Result<Value, String> {
            match method {
                "echo" => Ok(params),
                "sleep" => {
                    std::thread::sleep(Duration::from_millis(params["ms"].as_u64().unwrap()));
                    Ok(params)
                }
                "ask_back" => {
                    let peer = self.peer.get().unwrap();
                    let answer = peer
                        .request("echo", params, TIMEOUT)
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "asked_back": answer }))
                }
                other => Err(format!("unknown method: {other}")),
            }
        }

        fn notification(&self, method: &str, params: Value) {
            self.notifications
                .lock()
                .unwrap()
                .push((method.to_string(), params));
        }
    }

    struct Side {
        peer: Peer,
        notifications: Arc<Mutex<Vec<(String, Value)>>>,
    }

    fn side(stream: UnixStream) -> Side {
        let handler = TestHandler::default();
        let slot = handler.peer.clone();
        let notifications = handler.notifications.clone();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let peer = Peer::start(reader, stream, handler);
        let _ = slot.set(peer.clone());
        Side {
            peer,
            notifications,
        }
    }

    fn pair() -> (Side, Side) {
        let (a, b) = UnixStream::pair().unwrap();
        (side(a), side(b))
    }

    #[test]
    fn a_request_gets_the_other_sides_answer() {
        let (workstation, _remote) = pair();

        let answer = workstation
            .peer
            .request("echo", json!({ "hi": 1 }), TIMEOUT)
            .unwrap();

        assert_eq!(answer, json!({ "hi": 1 }));
    }

    #[test]
    fn an_error_answer_fails_the_request() {
        let (workstation, _remote) = pair();

        let error = workstation
            .peer
            .request("frobnicate", Value::Null, TIMEOUT)
            .unwrap_err();

        assert!(error.to_string().contains("unknown method: frobnicate"));
    }

    #[test]
    fn concurrent_requests_flow_in_both_directions_over_one_channel() {
        let (workstation, remote) = pair();
        let (done_tx, done_rx) = mpsc::channel();
        let started = Instant::now();

        // Slow and fast requests from both sides at once; each side also
        // asks a request that makes the other side ask back while pending.
        for (name, peer) in [("workstation", &workstation.peer), ("remote", &remote.peer)] {
            for (label, method, params) in [
                ("slow", "sleep", json!({ "ms": 300 })),
                ("fast", "echo", json!({ "n": 1 })),
                ("ask_back", "ask_back", json!({ "n": 2 })),
            ] {
                let peer = peer.clone();
                let done_tx = done_tx.clone();
                std::thread::spawn(move || {
                    let answer = peer.request(method, params, TIMEOUT).unwrap();
                    done_tx.send((name, label, answer)).unwrap();
                });
            }
        }
        drop(done_tx);
        let finished: Vec<_> = done_rx.iter().collect();

        assert_eq!(finished.len(), 6);
        // Fast answers are not queued behind the slow ones.
        let first_slow = finished.iter().position(|f| f.1 == "slow").unwrap();
        assert!(
            first_slow >= 4,
            "slow requests blocked others: {finished:?}"
        );
        for (_, label, answer) in &finished {
            match *label {
                "slow" => assert_eq!(answer, &json!({ "ms": 300 })),
                "fast" => assert_eq!(answer, &json!({ "n": 1 })),
                _ => assert_eq!(answer, &json!({ "asked_back": { "n": 2 } })),
            }
        }
        // The two slow requests ran at the same time.
        assert!(started.elapsed() < Duration::from_millis(550));
    }

    #[test]
    fn a_notification_reaches_the_other_side_without_an_answer() {
        let (workstation, remote) = pair();

        workstation
            .peer
            .notify("status", json!({ "pane": "%1" }))
            .unwrap();
        // A request after the notification proves it was read first.
        workstation
            .peer
            .request("echo", Value::Null, TIMEOUT)
            .unwrap();

        assert_eq!(
            *remote.notifications.lock().unwrap(),
            vec![("status".to_string(), json!({ "pane": "%1" }))]
        );
    }

    #[test]
    fn a_pending_request_fails_when_the_channel_closes() {
        let (stream, other_end) = UnixStream::pair().unwrap();
        let workstation = side(stream);
        let pending = {
            let peer = workstation.peer.clone();
            std::thread::spawn(move || peer.request("echo", Value::Null, TIMEOUT))
        };
        let mut request = String::new();
        BufReader::new(&other_end).read_line(&mut request).unwrap();

        drop(other_end);

        let error = pending.join().unwrap().unwrap_err();
        assert!(
            error.to_string().contains("control channel closed"),
            "{error}"
        );
        assert!(workstation.peer.wait_closed(TIMEOUT));
        assert!(workstation.peer.is_closed());
    }

    #[test]
    fn a_request_times_out_when_no_answer_arrives() {
        let (workstation, _remote) = pair();

        let error = workstation
            .peer
            .request("sleep", json!({ "ms": 1000 }), Duration::from_millis(100))
            .unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error}");
    }
}
