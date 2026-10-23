//! Remote shells: commands run in a remote pane's shell, rather than in a
//! command session, reach the workstation through the shell socket.
//!
//! While its control channel is connected, remote workmux listens on a Unix
//! socket at a fixed path in its state directory. Remote panes carry that
//! path in their environment, so workmux run in them forwards its
//! multiplexer requests to the workstation's tmux, never to a tmux on the
//! remote (or, on a loopback remote, the user's own). Each connection to the
//! socket opens a shell session: the workstation scopes it to the remote
//! pane's project and answers context questions from the workstation pane
//! showing that remote pane.

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::pane::{PANE_ENV, RemotePaneId};
use super::shims::Exited;
use crate::multiplexer::Multiplexer;
use crate::multiplexer::{AgentPane, TmuxBackend};

/// Name of the shell socket in remote workmux's state directory.
pub const SOCKET_NAME: &str = "shell.sock";

/// Environment of a remote pane: the shell socket.
pub const SOCKET_ENV: &str = "WORKMUX_SHELL_SOCKET";

/// Request opening a shell session for a remote pane: `{pane}`, answered
/// with `{session}`.
pub const OPEN_METHOD: &str = "shell.open";

/// Notification that a shell session's connection has closed: `{session}`.
pub const CLOSE_METHOD: &str = "shell.close";

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenParams {
    pub pane: RemotePaneId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Opened {
    pub session: u64,
}

/// Request from a remote shell to run an agent command on the workstation,
/// which holds the agents' state: `{argv, stdin}`, answered with its exit
/// code and output once it exits.
pub const RELAY_METHOD: &str = "command.relay";

/// Environment of an agent command the workstation runs for a remote: the
/// remote, whose agents alone it may act on.
pub const RELAYED_ENV: &str = "WORKMUX_RELAYED_REMOTE";

/// The agent commands a remote shell runs on the workstation.
pub const RELAYED_COMMANDS: &[&str] = &["send", "capture", "wait"];

#[derive(Debug, Serialize, Deserialize)]
pub struct Relay {
    pub argv: Vec<String>,
    /// Base64 of the input piped to the command, if any.
    pub stdin: Option<String>,
}

/// Where this remote's shell socket lives.
pub fn socket_path() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join(SOCKET_NAME))
}

/// Where this remote's shims live: commands a remote pane's shell runs on
/// the workstation, put first on its PATH.
pub fn shims_dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("shims/bin"))
}

/// The remote pane this process runs in and the shell socket it reaches
/// the workstation through, when it runs in a remote pane's shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub socket: PathBuf,
    pub pane: RemotePaneId,
}

impl Shell {
    /// Fail with "workstation not connected" unless the workstation answers
    /// this shell.
    pub fn ensure_connected(&self) -> Result<()> {
        let holder = super::pane::SessionHolder::new(std::env::temp_dir());
        super::forwarding::ForwardingMux::for_shell(self.socket.clone(), self.pane.clone(), holder)
            .is_running()
            .map(drop)
    }

    pub fn current() -> Option<Self> {
        let socket = std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty())?;
        let pane = std::env::var(PANE_ENV).ok()?.parse().ok()?;
        Some(Self {
            socket: PathBuf::from(socket),
            pane,
        })
    }
}

/// Run agent command `argv` (without `workmux`) on the workstation from a
/// remote shell, through the shell socket at `socket`, with `stdin` piped
/// to it.
pub fn relay(socket: &Path, argv: &[String], stdin: Option<Vec<u8>>) -> Result<Exited> {
    Ok(serde_json::from_value(super::forwarding::request_once(
        socket,
        RELAY_METHOD,
        serde_json::to_value(Relay {
            argv: argv.to_vec(),
            stdin: stdin.map(|stdin| base64::engine::general_purpose::STANDARD.encode(stdin)),
        })?,
    )?)?)
}

/// Run this agent command on the workstation when it runs in a remote
/// shell: `argv` (without `workmux`), with `stdin` piped to it. Writes its
/// output here and returns its exit code; `None` outside a remote shell.
pub fn run_relayed(argv: Vec<String>, stdin: Option<Vec<u8>>) -> Result<Option<i32>> {
    let Some(shell) = Shell::current() else {
        return Ok(None);
    };
    let exited = relay(&shell.socket, &argv, stdin)?;
    std::io::stdout().write_all(&exited.stdout()?)?;
    std::io::stderr().write_all(&exited.stderr()?)?;
    Ok(Some(exited.exit_code))
}

/// This command's own arguments, to run it again elsewhere.
pub fn own_argv() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// The workstation's answer to `command.relay` from remote `name`: run the
/// agent command with workstation workmux `workmux` against tmux server
/// `tmux_socket`, limited to the remote's agents.
pub fn answer_relay(
    name: &str,
    workmux: &Path,
    tmux_socket: Option<&str>,
    params: Value,
) -> Result<Value> {
    let Relay { argv, stdin } = serde_json::from_value(params)?;
    let command = argv.first().map(String::as_str).unwrap_or_default();
    if !RELAYED_COMMANDS.contains(&command) {
        bail!("refused: workmux {command} does not run on the workstation for a remote");
    }
    let stdin = stdin
        .map(|stdin| base64::engine::general_purpose::STANDARD.decode(stdin))
        .transpose()?;
    let mut child = Command::new(workmux);
    child
        .args(&argv)
        .env(RELAYED_ENV, name)
        .env_remove("TMUX_PANE")
        .current_dir(home::home_dir().unwrap_or_else(std::env::temp_dir))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match tmux_socket {
        Some(socket) => child.env("TMUX", format!("{socket},0,0")),
        None => child.env_remove("TMUX"),
    };
    let mut child = child
        .spawn()
        .with_context(|| format!("failed to run {}", workmux.display()))?;
    if let Some(stdin) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            let _ = pipe.write_all(&stdin);
        });
    }
    let output = child.wait_with_output()?;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(serde_json::to_value(Exited {
        exit_code: output.status.code().unwrap_or(1),
        stdout: encode(&output.stdout),
        stderr: encode(&output.stderr),
    })?)
}

/// The agents among `agents` whose panes are in windows of projects on
/// `remote`, the only agents an agent command run for it may act on.
pub fn agents_of_remote(
    remote: &str,
    agents: Vec<AgentPane>,
    tmux: &TmuxBackend,
) -> Vec<AgentPane> {
    agents
        .into_iter()
        .filter(|agent| {
            tmux.query(&[
                "display-message",
                "-p",
                "-t",
                &agent.pane_id,
                &format!("#{{{}}}", super::forwarded::WINDOW_TAG),
            ])
            .ok()
            .as_deref()
            .and_then(super::forwarded::parse_project_tag)
            .is_some_and(|(tagged, _)| tagged == remote)
        })
        .collect()
}

/// `agents`, limited to the relaying remote's when this process runs an
/// agent command for a remote shell.
pub fn scope_relayed(agents: Vec<AgentPane>) -> Vec<AgentPane> {
    match std::env::var(RELAYED_ENV) {
        Ok(remote) if !remote.is_empty() => agents_of_remote(&remote, agents, &TmuxBackend::new()),
        _ => agents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, TransportKind};
    use crate::remote::channels::{ChannelPolicy, Channels};
    use crate::remote::testing::ServeTransport;
    use crate::remote::transport::Transport;
    use std::sync::Arc;

    /// Stands in for workstation workmux: prints what it was run with.
    fn echoing_workmux(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("workmux");
        std::fs::write(
            &path,
            "#!/bin/sh\necho \"$* for $WORKMUX_RELAYED_REMOTE\"\ncat\necho oops >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn shell_on_devbox(dir: &std::path::Path) -> (Arc<ServeTransport>, Channels) {
        let transport = Arc::new(ServeTransport::default());
        let serving = transport.clone();
        let channels = Channels::new(
            Arc::new(move |_, _| Ok(serving.clone() as Arc<dyn Transport>)),
            ChannelPolicy {
                workmux: echoing_workmux(dir),
                ..ChannelPolicy::default()
            },
        );
        channels
            .channel(
                "devbox",
                &RemoteConfig {
                    transport: TransportKind::Loopback,
                    target: "/remote".to_string(),
                    builtin_shims: None,
                    host_commands: vec![],
                },
            )
            .unwrap();
        (transport, channels)
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn an_agent_command_in_a_remote_shell_runs_on_the_workstation_for_that_remote() {
        let temp = tempfile::tempdir().unwrap();
        let (transport, _channels) = shell_on_devbox(temp.path());

        let exited = relay(
            &transport.shell_socket(),
            &argv(&["send", "feature", "hello"]),
            Some(b"piped\n".to_vec()),
        )
        .unwrap();

        assert_eq!(exited.exit_code, 3);
        assert_eq!(
            String::from_utf8(exited.stdout().unwrap()).unwrap(),
            "send feature hello for devbox\npiped\n"
        );
        assert_eq!(exited.stderr().unwrap(), b"oops\n");
    }

    fn agent(pane_id: &str) -> crate::multiplexer::AgentPane {
        serde_json::from_value(serde_json::json!({
            "session": "s",
            "window_name": "w",
            "pane_id": pane_id,
            "path": "/x",
        }))
        .unwrap()
    }

    #[test]
    fn an_agent_command_run_for_a_remote_sees_only_that_remotes_agents() {
        let Some(server) = crate::test_support::PrivateTmuxServer::start() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let tagged = |session: &str, tag: Option<&str>| {
            let pane = server.new_session(session, temp.path());
            if let Some(tag) = tag {
                server.query(&[
                    "set-option",
                    "-w",
                    "-t",
                    &pane,
                    crate::remote::forwarded::WINDOW_TAG,
                    tag,
                ]);
            }
            pane
        };
        let ours = tagged("api", Some("devbox:api"));
        let other_remote = tagged("web", Some("buildbox:web"));
        let local = tagged("mine", None);
        let tmux = crate::multiplexer::TmuxBackend::for_socket(&server.socket);

        let seen = agents_of_remote(
            "devbox",
            vec![agent(&ours), agent(&other_remote), agent(&local)],
            &tmux,
        );

        assert_eq!(
            seen.iter().map(|a| a.pane_id.as_str()).collect::<Vec<_>>(),
            [ours.as_str()]
        );
    }

    #[test]
    fn other_commands_are_not_run_on_the_workstation() {
        let temp = tempfile::tempdir().unwrap();
        let (transport, _channels) = shell_on_devbox(temp.path());

        let error = relay(
            &transport.shell_socket(),
            &argv(&["remove", "feature"]),
            None,
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("refused: workmux remove does not run on the workstation for a remote"),
            "{error}"
        );
    }
}
