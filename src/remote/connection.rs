//! The workstation's end of one control channel, opened through a transport.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::io::BufReader;
use std::time::Duration;

use super::channel::{Handler, Peer};
use super::handshake::{Hello, read_hello};
use super::serve::Ping;
use super::transport::{StdioProcess, Transport};

/// How long a remote has to print its hello once the transport has started.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a health check waits for its answer.
pub const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// An open control channel to remote workmux.
pub struct Connection {
    pub peer: Peer,
    pub hello: Hello,
    /// Set when the remote runs a different workmux release.
    pub warning: Option<String>,
    process: StdioProcess,
}

impl Connection {
    /// Run `workmux remote serve` on `remote` through `transport` and wait for
    /// its hello. Requests from the remote go to `handler`.
    pub fn open(
        remote: &str,
        transport: &dyn Transport,
        handler: impl Handler,
        hello_timeout: Duration,
    ) -> Result<Self> {
        let argv = ["workmux", "remote", "serve"].map(String::from);
        let mut process = transport.exec_stdio(&argv)?;
        let (stdin, stdout) = process.take_stdio();

        // Read the hello on its own thread so a silent remote can time out.
        let (hello_tx, hello_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let hello = read_hello(&mut reader);
            let _ = hello_tx.send(hello.map(|hello| (hello, reader)));
        });
        let (hello, reader) = match hello_rx.recv_timeout(hello_timeout) {
            Ok(Ok(received)) => received,
            Ok(Err(error)) => {
                // The transport's own error explains an early exit best.
                let failure = process.failure(Duration::from_secs(1));
                process.kill();
                return Err(if failure.starts_with("transport process") {
                    anyhow!("{error} ({failure})")
                } else {
                    anyhow!(failure)
                });
            }
            Err(_) => {
                process.kill();
                return Err(anyhow!(
                    "no hello from {remote} within {}s",
                    hello_timeout.as_secs_f32()
                ));
            }
        };
        let warning = match hello.check(remote) {
            Ok(warning) => warning,
            Err(error) => {
                process.kill();
                return Err(error);
            }
        };
        if let Some(warning) = &warning {
            tracing::warn!(remote, "{warning}");
        }

        let peer = Peer::start(reader, stdin, handler);
        Ok(Self {
            peer,
            hello,
            warning,
            process,
        })
    }

    /// Ask remote workmux for its health.
    pub fn ping(&self) -> Result<Ping> {
        let result = self.peer.request("ping", Value::Null, PING_TIMEOUT)?;
        Ok(serde_json::from_value(result)?)
    }

    /// The transport's account of why the channel failed.
    pub fn failure(&mut self) -> String {
        self.process.failure(Duration::from_secs(1))
    }

    pub fn is_closed(&self) -> bool {
        self.peer.is_closed()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::handshake::{PROTOCOL_VERSION, SENTINEL};
    use crate::remote::transport::TtyExec;
    use std::process::{Command, Output};

    /// Runs every command as a shell script on the workstation.
    struct ScriptTransport(String);

    impl Transport for ScriptTransport {
        fn exec_captured(&self, _argv: &[String]) -> Result<Output> {
            unimplemented!()
        }

        fn exec_stdio(&self, _argv: &[String]) -> Result<StdioProcess> {
            let mut command = Command::new("sh");
            command.arg("-c").arg(&self.0);
            StdioProcess::spawn(command)
        }

        fn tty_command(&self, _exec: &TtyExec) -> Result<Command> {
            unimplemented!()
        }
    }

    /// Runs `workmux remote serve` as this test binary, through the isolated
    /// test child below.
    struct TestBinaryTransport;

    impl Transport for TestBinaryTransport {
        fn exec_captured(&self, _argv: &[String]) -> Result<Output> {
            unimplemented!()
        }

        fn exec_stdio(&self, _argv: &[String]) -> Result<StdioProcess> {
            let name = "remote::connection::tests::serve_child";
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([name, "--exact", "--nocapture", "--ignored"])
                .env(crate::test_support::ISOLATED_TEST_ENV, name);
            StdioProcess::spawn(command)
        }

        fn tty_command(&self, _exec: &TtyExec) -> Result<Command> {
            unimplemented!()
        }
    }

    struct NoRequests;

    impl Handler for NoRequests {
        fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
            Err(format!("unknown method: {method}"))
        }
    }

    fn hello_script(noise: &str, workmux: &str, protocol: u32) -> String {
        format!(
            "printf '{noise}'; printf '\\n{SENTINEL} {{\"workmux\":\"{workmux}\",\"protocol\":{protocol}}}\\n'; exec cat >/dev/null"
        )
    }

    fn open(transport: &dyn Transport) -> Result<Connection> {
        Connection::open("devbox", transport, NoRequests, Duration::from_secs(5))
    }

    #[test]
    #[ignore = "runs as a child of serving_a_real_remote_over_stdio"]
    fn serve_child() {
        if crate::test_support::is_isolated_child("remote::connection::tests::serve_child") {
            crate::remote::serve::run().unwrap();
        }
    }

    #[test]
    fn serving_a_real_remote_over_stdio() {
        // The test harness prints its own lines before the hello, which the
        // workstation must discard like shell-startup noise.
        let connection = open(&TestBinaryTransport).unwrap();

        let ping = connection.ping().unwrap();

        assert_eq!(connection.hello, Hello::current());
        assert_eq!(connection.warning, None);
        assert_eq!(ping.workmux, env!("CARGO_PKG_VERSION"));
        assert_eq!(ping.protocol, PROTOCOL_VERSION);
        assert_ne!(ping.pid, std::process::id());
    }

    #[test]
    fn noise_before_the_hello_is_discarded() {
        let transport = ScriptTransport(hello_script(
            "Last login: today\\n\\033[1mhi\\033[0m\\npartial",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
        ));

        let connection = open(&transport).unwrap();

        assert_eq!(connection.hello.workmux, env!("CARGO_PKG_VERSION"));
        assert!(!connection.is_closed());
    }

    #[test]
    fn a_different_workmux_release_opens_with_a_warning() {
        let transport = ScriptTransport(hello_script("", "0.0.1", PROTOCOL_VERSION));

        let connection = open(&transport).unwrap();

        assert!(
            connection
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("devbox runs workmux 0.0.1")),
            "{:?}",
            connection.warning
        );
    }

    #[test]
    fn a_protocol_mismatch_refuses_the_connection() {
        let transport = ScriptTransport(hello_script("", "9.9.9", PROTOCOL_VERSION + 1));

        let error = open(&transport).err().unwrap().to_string();

        assert!(
            error.contains(&format!("install workmux {}", env!("CARGO_PKG_VERSION"))),
            "{error}"
        );
    }

    #[test]
    fn a_transport_failure_is_reported_in_the_transports_words() {
        let transport = ScriptTransport(
            "echo 'ssh: Could not resolve hostname devbox: nodename nor servname provided' >&2; exit 255"
                .to_string(),
        );

        let error = open(&transport).err().unwrap().to_string();

        assert_eq!(
            error,
            "ssh: Could not resolve hostname devbox: nodename nor servname provided"
        );
    }

    #[test]
    fn a_remote_that_never_says_hello_times_out() {
        let transport = ScriptTransport("exec sleep 30".to_string());

        let error = Connection::open("devbox", &transport, NoRequests, Duration::from_millis(200))
            .err()
            .unwrap()
            .to_string();

        assert!(error.contains("no hello from devbox"), "{error}");
    }
}
