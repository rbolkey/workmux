//! Command sessions: one command relayed to a remote, with its streamed
//! output and exit code.
//!
//! The workstation sends `command.run` over the control channel. Remote
//! workmux clones the project on first use, runs the command in the
//! project's clone and streams its stdout and stderr back as
//! `command.output` notifications before answering with the exit code.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

use super::channel::Peer;

/// Request method that starts a command session on the remote.
pub const RUN_METHOD: &str = "command.run";

/// Notification carrying a chunk of a command session's output.
pub const OUTPUT_METHOD: &str = "command.output";

/// How long a command session may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Environment remote workmux sets for a command it runs for a remote
/// project: the remote's name and the project's registered name.
pub const REMOTE_ENV: &str = "WORKMUX_REMOTE_NAME";
pub const PROJECT_ENV: &str = "WORKMUX_PROJECT_NAME";

/// A remote project as remote workmux needs it to run a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSite {
    pub remote: String,
    pub name: String,
    /// Path of the clone on the remote; `~` and relative paths are taken
    /// from the remote's home directory.
    pub path: String,
    pub git_url: Option<String>,
}

/// One command to run in a remote project. In `argv`, `workmux` means
/// remote workmux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub project: ProjectSite,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunParams {
    session: u64,
    request: CommandRequest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OutputParams {
    pub session: u64,
    pub stream: Stream,
    /// Base64, since output need not be UTF-8 or split on characters.
    pub data: String,
}

impl OutputParams {
    pub fn new(session: u64, stream: Stream, data: &[u8]) -> Self {
        Self {
            session,
            stream,
            data: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.data)?)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Exit {
    pub exit_code: i32,
}

/// What a running session hands back to the workstation caller.
enum Event {
    Output(Stream, Vec<u8>),
    Exit(Result<Value>),
}

/// The workstation's open command sessions, by session id.
#[derive(Default)]
pub struct Sessions {
    next_id: AtomicU64,
    open: Mutex<HashMap<u64, Sender<Event>>>,
}

impl Sessions {
    fn open(&self) -> (u64, Sender<Event>, Receiver<Event>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = mpsc::channel();
        self.open.lock().unwrap().insert(id, sender.clone());
        (id, sender, receiver)
    }

    fn close(&self, id: u64) {
        self.open.lock().unwrap().remove(&id);
    }

    /// Deliver a `command.output` notification to its session.
    pub fn output(&self, params: Value) {
        let delivered = serde_json::from_value::<OutputParams>(params)
            .map_err(anyhow::Error::from)
            .and_then(|params| {
                let data = params.bytes()?;
                let open = self.open.lock().unwrap();
                let session = open
                    .get(&params.session)
                    .ok_or_else(|| anyhow!("no command session {}", params.session))?;
                let _ = session.send(Event::Output(params.stream, data));
                Ok(())
            });
        if let Err(error) = delivered {
            tracing::warn!(error = %format!("{error:#}"), "dropping command output");
        }
    }

    /// Run `request` on the remote at the other end of `peer`, passing its
    /// output to `sink` as it arrives. Returns the command's exit code.
    pub fn run(
        &self,
        peer: &Peer,
        request: &CommandRequest,
        sink: &mut dyn FnMut(Stream, &[u8]),
    ) -> Result<i32> {
        let (session, sender, events) = self.open();
        let params = serde_json::to_value(RunParams {
            session,
            request: request.clone(),
        })?;
        let peer = peer.clone();
        // The answer follows every output notification on the channel, so
        // the exit arrives after the last chunk.
        std::thread::spawn(move || {
            let answer = peer.request(RUN_METHOD, params, COMMAND_TIMEOUT);
            let _ = sender.send(Event::Exit(answer));
        });
        let answer = loop {
            match events.recv() {
                Ok(Event::Output(stream, data)) => sink(stream, &data),
                Ok(Event::Exit(answer)) => break answer,
                Err(_) => break Err(anyhow!("command session ended without an exit code")),
            }
        };
        self.close(session);
        let exit: Exit = serde_json::from_value(answer?)?;
        Ok(exit.exit_code)
    }
}

/// Remote workmux's side of `command.run`: run the command, streaming its
/// output over `peer`, and answer with its exit code.
pub fn serve_run(peer: &Peer, params: Value) -> Result<Value> {
    let RunParams { session, request } = serde_json::from_value(params)?;
    let exit_code = run_on_remote(&request, &|stream, data| {
        let _ = peer.notify(
            OUTPUT_METHOD,
            serde_json::to_value(OutputParams::new(session, stream, data)).expect("serializes"),
        );
    })?;
    Ok(serde_json::to_value(Exit { exit_code })?)
}

/// Where `path` from a project registration lives on this machine.
fn clone_path(path: &str) -> Result<PathBuf> {
    let home = home::home_dir().ok_or_else(|| anyhow!("remote has no home directory"))?;
    let expanded = crate::util::expand_tilde(path);
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        home.join(expanded)
    })
}

fn run_streamed(
    mut command: Command,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<std::process::ExitStatus> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    std::thread::scope(|scope| {
        scope.spawn(|| relay(stdout, Stream::Stdout, emit));
        scope.spawn(|| relay(stderr, Stream::Stderr, emit));
    });
    Ok(child.wait()?)
}

fn relay(mut from: impl Read, stream: Stream, emit: &(dyn Fn(Stream, &[u8]) + Sync)) {
    let mut buf = [0u8; 8192];
    while let Ok(read) = from.read(&mut buf) {
        if read == 0 {
            break;
        }
        emit(stream, &buf[..read]);
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Clone the project to its registered path if it is missing.
fn ensure_cloned(
    project: &ProjectSite,
    path: &Path,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let Some(git_url) = &project.git_url else {
        bail!(
            "project {} has no repository URL and {} does not exist on {}",
            project.name,
            path.display(),
            project.remote
        );
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    emit(
        Stream::Stderr,
        format!(
            "Cloning {git_url} into {} on {}\n",
            path.display(),
            project.remote
        )
        .as_bytes(),
    );
    let mut clone = Command::new("git");
    clone.arg("clone").arg("--quiet").arg(git_url).arg(path);
    let status = run_streamed(clone, emit)?;
    if !status.success() {
        bail!("git clone of {git_url} into {} failed", path.display());
    }
    Ok(())
}

/// Run `request` on this machine, the remote: clone the project on first
/// use, then run the command in its clone.
pub fn run_on_remote(
    request: &CommandRequest,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<i32> {
    let path = clone_path(&request.project.path)?;
    ensure_cloned(&request.project, &path, emit)?;
    let (program, args) = request
        .argv
        .split_first()
        .ok_or_else(|| anyhow!("no command to run"))?;
    let mut command = if program == "workmux" {
        Command::new(std::env::current_exe().context("failed to locate the workmux binary")?)
    } else {
        Command::new(program)
    };
    command
        .args(args)
        .current_dir(&path)
        .env(REMOTE_ENV, &request.project.remote)
        .env(PROJECT_ENV, &request.project.name);
    Ok(exit_code(run_streamed(command, emit)?))
}

/// The remote project a command runs for, when remote workmux runs it in
/// a command session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedProject {
    pub remote: String,
    pub name: String,
}

impl ServedProject {
    pub fn current() -> Option<Self> {
        Some(Self {
            remote: std::env::var(REMOTE_ENV).ok().filter(|v| !v.is_empty())?,
            name: std::env::var(PROJECT_ENV).ok().filter(|v| !v.is_empty())?,
        })
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

    fn channels() -> Channels {
        let transport = Arc::new(ServeTransport::default());
        Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            ChannelPolicy::default(),
        )
    }

    fn remote() -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Loopback,
            target: "/remote".to_string(),
            builtin_shims: None,
            host_commands: vec![],
        }
    }

    fn request(path: &Path, git_url: Option<&Path>, script: &str) -> CommandRequest {
        CommandRequest {
            project: ProjectSite {
                remote: "devbox".to_string(),
                name: "api".to_string(),
                path: path.display().to_string(),
                git_url: git_url.map(|url| url.display().to_string()),
            },
            argv: ["sh", "-c", script].map(String::from).to_vec(),
        }
    }

    #[derive(Default)]
    struct Captured {
        stdout: String,
        stderr: String,
    }

    fn run(
        request: &CommandRequest,
        mut on_stdout: impl FnMut(&str),
    ) -> (Result<i32>, Captured) {
        let mut captured = Captured::default();
        let exit = channels().run_command("devbox", &remote(), request, &mut |stream, data| {
            let text = String::from_utf8_lossy(data);
            match stream {
                Stream::Stdout => {
                    captured.stdout.push_str(&text);
                    on_stdout(&captured.stdout);
                }
                Stream::Stderr => captured.stderr.push_str(&text),
            }
        });
        (exit, captured)
    }

    #[test]
    fn output_streams_while_the_command_runs_and_the_exit_code_is_kept() {
        let project = tempfile::tempdir().unwrap();
        let gate = project.path().join("gate");
        // The command waits for the workstation to see its first line, so it
        // only finishes if output arrives while it runs.
        let script = "echo started; i=0; \
            while [ ! -e gate ] && [ $i -lt 500 ]; do sleep 0.01; i=$((i+1)); done; \
            [ -e gate ] || echo 'not streamed'; echo failed >&2; exit 3";

        let (exit, captured) = run(&request(project.path(), None, script), |stdout| {
            if stdout.contains("started") {
                let _ = std::fs::write(&gate, "");
            }
        });

        assert_eq!(exit.unwrap(), 3);
        assert_eq!(captured.stdout, "started\n");
        assert_eq!(captured.stderr, "failed\n");
    }

    #[test]
    fn the_command_runs_in_the_clone_with_the_project_identity() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().canonicalize().unwrap();

        let (exit, captured) = run(
            &request(&path, None, "pwd; echo $WORKMUX_REMOTE_NAME $WORKMUX_PROJECT_NAME"),
            |_| {},
        );

        assert_eq!(exit.unwrap(), 0);
        assert_eq!(captured.stdout, format!("{}\ndevbox api\n", path.display()));
    }

    #[test]
    fn first_use_clones_the_repository_to_the_registered_path() {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        crate::test_support::init_repo(&origin);
        let path = temp.path().join("code").join("api");

        let (exit, captured) = run(
            &request(&path, Some(&origin), "git rev-parse --abbrev-ref HEAD"),
            |_| {},
        );

        assert_eq!(exit.unwrap(), 0);
        assert!(path.join(".git").is_dir());
        assert_eq!(captured.stdout, "main\n");
        assert!(
            captured.stderr.contains(&format!(
                "Cloning {} into {} on devbox",
                origin.display(),
                path.display()
            )),
            "{}",
            captured.stderr
        );
    }

    #[test]
    fn a_missing_clone_without_a_repository_url_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("api");

        let (exit, _) = run(&request(&path, None, "true"), |_| {});

        assert_eq!(
            exit.unwrap_err().to_string(),
            format!(
                "project api has no repository URL and {} does not exist on devbox",
                path.display()
            )
        );
    }
}
