//! Command sessions: one command relayed to a remote, with its streamed
//! output, confirmation prompts, cancellation and exit code.
//!
//! The workstation sends `command.run` over the control channel. Remote
//! workmux clones and provisions the project on first use, runs the command in the
//! project's clone and streams its stdout and stderr back as
//! `command.output` notifications before answering with the exit code.
//! When the command reads an answer to a prompt, it sends `command.prompt`
//! through the forwarding socket and the workstation reads the line from
//! the invoking terminal. `command.cancel` interrupts the command.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::remote::git_url::GitUrl;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::channel::Peer;
use super::forwarded::SessionScope;
use super::{forwarding, provision};
use crate::multiplexer::InvocationContext;

/// Request method that starts a command session on the remote.
pub const RUN_METHOD: &str = "command.run";

/// Notification carrying a chunk of a command session's output.
pub const OUTPUT_METHOD: &str = "command.output";

/// Request from a command on the remote for one line the user types in
/// the invoking terminal: `{session}`, answered with `{line}`, where a
/// `null` line is the end of input.
pub const PROMPT_METHOD: &str = "command.prompt";

/// Notification asking remote workmux to interrupt a running command.
pub const CANCEL_METHOD: &str = "command.cancel";

/// How long a cancelled command has to exit after its interrupt before it
/// is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// How often a running session checks whether it was cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// How long a command session may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Environment remote workmux sets for a command it runs for a remote
/// project: the remote's name and the project's name.
pub const REMOTE_ENV: &str = "WORKMUX_REMOTE_NAME";
pub const PROJECT_ENV: &str = "WORKMUX_PROJECT_NAME";
/// The workstation session for the project's windows, when it has one.
pub const PROJECT_SESSION_ENV: &str = "WORKMUX_PROJECT_SESSION";

/// A remote project as remote workmux needs it to run a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSite {
    pub remote: String,
    pub name: String,
    /// Path of the clone on the remote; `~` and relative paths are taken
    /// from the remote's home directory.
    pub path: String,
    #[serde(flatten)]
    pub git_url: Option<GitUrl>,
    /// Workstation session for the project's windows in window mode; `None`
    /// places them in the invoking session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl ProjectSite {
    pub fn id(&self) -> crate::project::RemoteProjectId {
        crate::project::RemoteProjectId::new(&self.remote, &self.name)
    }
}

/// One command to run in a remote project. In `argv`, `workmux` means
/// remote workmux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub project: ProjectSite,
    pub argv: Vec<String>,
    /// Ownership token of the workstation window the command was typed in,
    /// when that window belongs to the project: the command then runs in
    /// that window's worktree, as it would locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoking_window_token: Option<String>,
    /// Contents of the prompt file the command was given on the workstation,
    /// whose `--prompt-file` option `argv` no longer has: remote workmux gets
    /// them in a file of its own, as the command's prompt file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionParams {
    pub session: u64,
}

/// A line read in answer to a prompt; `None` at the end of input.
#[derive(Debug, Serialize, Deserialize)]
pub struct Answer {
    pub line: Option<String>,
}

/// Where a command session's output goes, where its prompts are answered,
/// and whether the user has cancelled it.
pub trait Terminal {
    fn output(&mut self, stream: Stream, data: &[u8]);

    /// One line the user types in answer to a prompt; `None` at the end of
    /// input.
    fn read_line(&mut self) -> Result<Option<String>>;

    fn cancelled(&self) -> bool {
        false
    }
}

/// An output-only terminal: prompts read the end of input.
impl<F: FnMut(Stream, &[u8])> Terminal for F {
    fn output(&mut self, stream: Stream, data: &[u8]) {
        self(stream, data)
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// What a running session hands back to the workstation caller.
enum Event {
    Output(Stream, Vec<u8>),
    Prompt(mpsc::SyncSender<Option<String>>),
    Exit(Result<Value>),
}

struct OpenSession {
    events: Sender<Event>,
    scope: SessionScope,
}

/// The workstation's open command sessions, by session id.
#[derive(Default)]
pub struct Sessions {
    next_id: AtomicU64,
    open: Mutex<HashMap<u64, OpenSession>>,
}

impl Sessions {
    fn open(&self, scope: SessionScope) -> (u64, Sender<Event>, Receiver<Event>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = mpsc::channel();
        self.open.lock().unwrap().insert(
            id,
            OpenSession {
                events: sender.clone(),
                scope,
            },
        );
        (id, sender, receiver)
    }

    /// The scope of open session `id`, for answering the requests its
    /// command forwards.
    pub fn scope(&self, id: u64) -> Option<SessionScope> {
        self.open
            .lock()
            .unwrap()
            .get(&id)
            .map(|session| session.scope.clone())
    }

    fn close(&self, id: u64) {
        self.open.lock().unwrap().remove(&id);
    }

    /// Open a session for the commands run in a remote pane's shell: their
    /// forwarded requests are scoped and answered like a command session's,
    /// but their output and prompts stay in the remote pane.
    pub fn open_shell(&self, scope: SessionScope) -> u64 {
        let (id, _, _) = self.open(scope);
        id
    }

    /// Close shell session `id` of `remote`, once its connection closes.
    pub fn close_shell(&self, id: u64, remote: &str) {
        let mut open = self.open.lock().unwrap();
        if open
            .get(&id)
            .is_some_and(|session| session.scope.remote == remote)
        {
            open.remove(&id);
        }
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
                    .ok_or_else(|| anyhow!("No command session {}", params.session))?;
                let _ = session.events.send(Event::Output(params.stream, data));
                Ok(())
            });
        if let Err(error) = delivered {
            tracing::warn!(error = %format!("{error:#}"), "dropping command output");
        }
    }

    /// Read one line from the terminal of open session `id`, for a prompt
    /// of the command it runs.
    pub fn prompt(&self, id: u64) -> Result<Option<String>> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.open
            .lock()
            .unwrap()
            .get(&id)
            .ok_or_else(|| anyhow!("No command session {id}"))?
            .events
            .send(Event::Prompt(reply))
            .map_err(|_| anyhow!("Command session {id} has ended"))?;
        Ok(answer.recv().unwrap_or(None))
    }

    /// Run `request` on the remote at the other end of `peer`, passing its
    /// output to `terminal` as it arrives and reading its prompts' answers
    /// there. Requests the command forwards are answered from
    /// `invocation`, and the windows they open tagged with the workstation
    /// clone `root`. Returns the command's exit code.
    pub fn run(
        &self,
        peer: &Peer,
        root: &Path,
        request: &CommandRequest,
        invocation: &InvocationContext,
        terminal: &mut dyn Terminal,
    ) -> Result<i32> {
        let (session, sender, events) = self.open(SessionScope {
            remote: request.project.remote.clone(),
            project: request.project.name.clone(),
            root: root.to_path_buf(),
            invocation: invocation.clone(),
        });
        let params = serde_json::to_value(RunParams {
            session,
            request: request.clone(),
        })?;
        let requester = peer.clone();
        // The answer follows every output notification on the channel, so
        // the exit arrives after the last chunk.
        std::thread::spawn(move || {
            let answer = requester.request(RUN_METHOD, params, COMMAND_TIMEOUT);
            let _ = sender.send(Event::Exit(answer));
        });
        let mut cancel_sent = false;
        let answer = loop {
            if !cancel_sent && terminal.cancelled() {
                cancel_sent = true;
                let _ = peer.notify(
                    CANCEL_METHOD,
                    serde_json::to_value(SessionParams { session }).expect("serializes"),
                );
            }
            match events.recv_timeout(CANCEL_POLL) {
                Ok(Event::Output(stream, data)) => terminal.output(stream, &data),
                Ok(Event::Prompt(reply)) => {
                    let line = terminal.read_line().unwrap_or_else(|error| {
                        tracing::warn!(error = %format!("{error:#}"), "reading a prompt answer");
                        None
                    });
                    let _ = reply.send(line);
                }
                Ok(Event::Exit(answer)) => break answer,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(anyhow!("Command session ended without an exit code"));
                }
            }
        };
        self.close(session);
        let exit: Exit = serde_json::from_value(answer?)?;
        Ok(exit.exit_code)
    }
}

/// The commands remote workmux is running, by command session, so they
/// can be cancelled.
#[derive(Clone, Default)]
pub struct Running {
    groups: Arc<Mutex<HashMap<u64, i32>>>,
}

impl Running {
    /// Interrupt the command of `session` and everything it started, and
    /// kill them if they are still running after a grace period.
    pub fn cancel(&self, session: u64) {
        let Some(group) = self.groups.lock().unwrap().get(&session).copied() else {
            return;
        };
        tracing::info!(session, "cancelling command session");
        signal_group(group, libc::SIGINT);
        let groups = self.groups.clone();
        std::thread::spawn(move || {
            std::thread::sleep(CANCEL_GRACE);
            if groups.lock().unwrap().get(&session) == Some(&group) {
                signal_group(group, libc::SIGKILL);
            }
        });
    }

    /// Interrupt every running command, as when the control channel
    /// closes, and kill what is left after the grace period.
    pub fn shut_down(&self) {
        let groups: Vec<i32> = self.groups.lock().unwrap().values().copied().collect();
        if groups.is_empty() {
            return;
        }
        tracing::info!(count = groups.len(), "cancelling running command sessions");
        for &group in &groups {
            signal_group(group, libc::SIGINT);
        }
        let deadline = std::time::Instant::now() + CANCEL_GRACE;
        while std::time::Instant::now() < deadline && !self.groups.lock().unwrap().is_empty() {
            std::thread::sleep(CANCEL_POLL);
        }
        for group in self.groups.lock().unwrap().values() {
            signal_group(*group, libc::SIGKILL);
        }
    }

    fn start(&self, session: u64, group: i32) {
        self.groups.lock().unwrap().insert(session, group);
    }

    fn end(&self, session: u64) {
        self.groups.lock().unwrap().remove(&session);
    }
}

fn signal_group(group: i32, signal: i32) {
    unsafe {
        libc::killpg(group, signal);
    }
}

/// Remote workmux's side of `command.run`: run the command, streaming its
/// output over `peer`, and answer with its exit code.
pub fn serve_run(
    peer: &Peer,
    forwarding_socket: Option<&Path>,
    running: &Running,
    params: Value,
) -> Result<Value> {
    let RunParams { session, request } = serde_json::from_value(params)?;
    let forwarding = forwarding_socket.map(|socket| Forwarding { socket, session });
    let tracked = Tracked { running, session };
    let exit_code = run_on_remote(&request, forwarding, Some(tracked), &|stream, data| {
        let _ = peer.notify(
            OUTPUT_METHOD,
            serde_json::to_value(OutputParams::new(session, stream, data)).expect("serializes"),
        );
    })?;
    Ok(serde_json::to_value(Exit { exit_code })?)
}

/// Where `path` from a project registration lives on this machine.
pub(crate) fn clone_path(path: &str) -> Result<PathBuf> {
    let home = home::home_dir().ok_or_else(|| anyhow!("Remote has no home directory"))?;
    let expanded = crate::util::expand_tilde(path);
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        home.join(expanded)
    })
}

/// A command session whose command can be cancelled while it runs.
#[derive(Clone, Copy)]
pub struct Tracked<'a> {
    pub running: &'a Running,
    pub session: u64,
}

fn run_streamed(
    mut command: Command,
    tracked: Option<Tracked>,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;
    let program = command.get_program().to_string_lossy().into_owned();
    // Its own process group, so cancelling reaches the hooks it runs too.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("Failed to run {program}"))?;
    if let Some(tracked) = tracked {
        tracked.running.start(tracked.session, child.id() as i32);
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    std::thread::scope(|scope| {
        scope.spawn(|| relay(stdout, Stream::Stdout, emit));
        scope.spawn(|| relay(stderr, Stream::Stderr, emit));
    });
    let status = child.wait();
    if let Some(tracked) = tracked {
        tracked.running.end(tracked.session);
    }
    Ok(status?)
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

/// Clone the project to its path if it is missing. Returns
/// whether it cloned.
fn ensure_cloned(
    project: &ProjectSite,
    path: &Path,
    tracked: Option<Tracked>,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let Some(git_url) = &project.git_url else {
        bail!(
            "Project {} has no repository URL and {} does not exist on {}",
            project.name,
            path.display(),
            project.remote
        );
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    emit(
        Stream::Stderr,
        format!(
            "Cloning {} into {} on {}\n",
            git_url.describe(),
            path.display(),
            project.remote
        )
        .as_bytes(),
    );
    let mut clone = crate::git::unattended_git(None)?;
    clone
        .arg("clone")
        .arg("--quiet")
        .arg(&git_url.url)
        .arg(path);
    let status = run_streamed(clone, tracked, emit)?;
    if !status.success() {
        bail!(
            "git clone of {} into {} failed",
            git_url.url,
            path.display()
        );
    }
    Ok(true)
}

/// Provision this remote for the project just cloned at `path`. A failed
/// step is a warning: the command it came before still runs.
fn provision(
    project: &ProjectSite,
    path: &Path,
    tracked: Option<Tracked>,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<()> {
    let mode = crate::config::Config::load_with_location_from(path, None)
        .map(|(config, _)| config.sandbox.toolchain())
        .unwrap_or_default();
    let toolchain = crate::sandbox::toolchain::resolve_toolchain(&mode, path);
    let plan = provision::plan(&toolchain, provision::has_command);
    provision::record_devshell_warning_here(&toolchain);
    let remote = &project.remote;
    for warning in &plan.warnings {
        emit(
            Stream::Stderr,
            format!("warning: {}\n", warning.replace("this remote", remote)).as_bytes(),
        );
    }
    for step in &plan.steps {
        emit(
            Stream::Stderr,
            format!("Provisioning {remote}: {}\n", step.what).as_bytes(),
        );
        let mut command = remote_command(&step.argv)?;
        command.current_dir(path);
        // Its output is not the command's: keep the command's stdout clean.
        let status = run_streamed(command, tracked, &|_, data| emit(Stream::Stderr, data))?;
        if !status.success() {
            emit(
                Stream::Stderr,
                format!(
                    "warning: {} on {remote} failed ({status}); do it there yourself\n",
                    step.what
                )
                .as_bytes(),
            );
        }
    }
    Ok(())
}

/// The command for `argv` on this machine, the remote, where `workmux`
/// means remote workmux.
fn remote_command(argv: &[String]) -> Result<Command> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("No command to run"))?;
    let mut command = if program == "workmux" {
        Command::new(std::env::current_exe().context("Failed to locate the workmux binary")?)
    } else {
        Command::new(program)
    };
    command.args(args);
    Ok(command)
}

/// Where a command run in a command session forwards its multiplexer
/// requests.
#[derive(Debug, Clone, Copy)]
pub struct Forwarding<'a> {
    pub socket: &'a Path,
    pub session: u64,
}

/// Run `request` on this machine, the remote: clone and provision the
/// project on first use, then run the command in its clone.
pub fn run_on_remote(
    request: &CommandRequest,
    forwarding: Option<Forwarding>,
    tracked: Option<Tracked>,
    emit: &(dyn Fn(Stream, &[u8]) + Sync),
) -> Result<i32> {
    let path = clone_path(&request.project.path)?;
    if ensure_cloned(&request.project, &path, tracked, emit)? {
        provision(&request.project, &path, tracked, emit)?;
    }
    let prompt_file = request
        .prompt_file
        .as_deref()
        .map(write_prompt_file)
        .transpose()?;
    let mut argv = request.argv.clone();
    if let Some(file) = &prompt_file {
        // Options go before any `--`, after which everything is positional.
        let at = argv
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(argv.len());
        argv.splice(
            at..at,
            [
                "--prompt-file".to_string(),
                file.path().display().to_string(),
            ],
        );
    }
    let mut command = remote_command(&argv)?;
    let workdir = request
        .invoking_window_token
        .as_deref()
        .and_then(|token| worktree_with_window_token(&path, token))
        .unwrap_or(path);
    command
        .current_dir(&workdir)
        .env(REMOTE_ENV, &request.project.remote)
        .env(PROJECT_ENV, &request.project.name);
    match &request.project.session {
        Some(session) => command.env(PROJECT_SESSION_ENV, session),
        None => command.env_remove(PROJECT_SESSION_ENV),
    };
    if let Some(forwarding) = forwarding {
        command
            .env(forwarding::SOCKET_ENV, forwarding.socket)
            .env(forwarding::SESSION_ENV, forwarding.session.to_string());
    }
    Ok(exit_code(run_streamed(command, tracked, emit)?))
}

/// A file holding `contents`, a prompt file from the workstation, removed
/// when dropped.
fn write_prompt_file(contents: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("workmux-prompt-")
        .suffix(".md")
        .tempfile()
        .context("Failed to create a file for the workstation's prompt file")?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.flush())
        .context("Failed to write the workstation's prompt file")?;
    Ok(file)
}

/// The worktree of the clone at `clone` whose window carries `token`.
fn worktree_with_window_token(clone: &Path, token: &str) -> Option<PathBuf> {
    crate::git::list_worktrees_in(Some(clone))
        .ok()?
        .into_iter()
        .map(|(path, _)| path)
        .find(|path| {
            path.file_name()
                .and_then(|handle| handle.to_str())
                .and_then(|handle| crate::git::get_worktree_window_token_in(handle, Some(clone)))
                .is_some_and(|candidate| candidate == token)
        })
}

/// The remote project a command runs for, when remote workmux runs it in
/// a command session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedProject {
    pub remote: String,
    pub name: String,
    /// Workstation session for the project's windows in window mode.
    pub session: Option<String>,
}

impl ServedProject {
    pub fn current() -> Option<Self> {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        Some(Self {
            remote: var(REMOTE_ENV)?,
            name: var(PROJECT_ENV)?,
            session: var(PROJECT_SESSION_ENV),
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

    /// Project `api` on remote `devbox`, as [`remote`] reaches it.
    fn devbox() -> crate::remote::router::RemoteProject {
        let mut project = crate::remote::testing::devbox_api();
        project.remote_config = remote();
        project
    }

    fn remote() -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Loopback,
            target: "/remote".to_string(),
            builtin_shims: None,
            host_commands: vec![],
            openshell: None,
        }
    }

    fn request(path: &Path, git_url: Option<&Path>, script: &str) -> CommandRequest {
        CommandRequest {
            project: ProjectSite {
                remote: "devbox".to_string(),
                name: "api".to_string(),
                path: path.display().to_string(),
                git_url: git_url.map(|url| GitUrl::new(url.display().to_string())),
                session: None,
            },
            argv: ["sh", "-c", script].map(String::from).to_vec(),
            invoking_window_token: None,
            prompt_file: None,
        }
    }

    #[derive(Default)]
    struct Captured {
        stdout: String,
        stderr: String,
    }

    fn run(request: &CommandRequest, mut on_stdout: impl FnMut(&str)) -> (Result<i32>, Captured) {
        let mut captured = Captured::default();
        let exit = channels().run_command(
            &devbox(),
            request,
            &InvocationContext::default(),
            &mut |stream, data: &[u8]| {
                let text = String::from_utf8_lossy(data);
                match stream {
                    Stream::Stdout => {
                        captured.stdout.push_str(&text);
                        on_stdout(&captured.stdout);
                    }
                    Stream::Stderr => captured.stderr.push_str(&text),
                }
            },
            false,
        );
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
    fn a_prompt_file_from_the_workstation_reaches_the_command_as_its_prompt_file() {
        let project = tempfile::tempdir().unwrap();
        let mut request = request(
            project.path(),
            None,
            r#"echo "$@"; cat "$4"; echo "$4" > seen"#,
        );
        request
            .argv
            .extend(["sh", "add", "feature"].map(String::from));
        request.prompt_file = Some("the workstation's file\n".to_string());

        let (exit, captured) = run(&request, |_| {});

        assert_eq!(exit.unwrap(), 0, "{}", captured.stderr);
        let mut lines = captured.stdout.lines();
        let args = lines.next().unwrap();
        assert!(args.starts_with("add feature --prompt-file /"), "{args}");
        assert_eq!(lines.next(), Some("the workstation's file"));
        // The file lasts only as long as the command.
        let seen = std::fs::read_to_string(project.path().join("seen")).unwrap();
        assert!(!Path::new(seen.trim()).exists());
    }

    /// A terminal whose user presses Ctrl-C once the command has printed
    /// `started`.
    #[derive(Default)]
    struct Interrupting {
        stdout: String,
    }

    impl Terminal for Interrupting {
        fn output(&mut self, stream: Stream, data: &[u8]) {
            if stream == Stream::Stdout {
                self.stdout.push_str(&String::from_utf8_lossy(data));
            }
        }

        fn read_line(&mut self) -> Result<Option<String>> {
            Ok(None)
        }

        fn cancelled(&self) -> bool {
            self.stdout.contains("started")
        }
    }

    fn running(pid: &str) -> bool {
        unsafe { libc::kill(pid.trim().parse().unwrap(), 0) == 0 }
    }

    #[test]
    fn cancelling_interrupts_the_command_and_what_it_started() {
        let project = tempfile::tempdir().unwrap();
        let pid_file = project.path().join("hook.pid");
        // A hook the command started, still running when the user cancels.
        let script = "sleep 30 & echo $! > hook.pid; echo started; wait";
        let started = std::time::Instant::now();

        let exit = channels().run_command(
            &devbox(),
            &request(project.path(), None, script),
            &InvocationContext::default(),
            &mut Interrupting::default(),
            false,
        );

        assert_eq!(exit.unwrap(), 130);
        assert!(started.elapsed() < Duration::from_secs(10));
        let hook = std::fs::read_to_string(&pid_file).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while running(&hook) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running(&hook), "the hook outlived the cancelled command");
    }

    const PROMPT_CHILD: &str = "remote::session::tests::prompt_child";

    #[test]
    #[ignore = "runs as a command in a command session"]
    fn prompt_child() {
        if crate::test_support::is_isolated_child(PROMPT_CHILD) {
            let answer = crate::ui::confirm::read_answer().unwrap();
            println!("answer: {}", answer.trim());
        }
    }

    /// A terminal whose user types `line` at every prompt.
    struct Typing {
        line: &'static str,
        stdout: String,
        prompts: usize,
    }

    impl Terminal for Typing {
        fn output(&mut self, stream: Stream, data: &[u8]) {
            if stream == Stream::Stdout {
                self.stdout.push_str(&String::from_utf8_lossy(data));
            }
        }

        fn read_line(&mut self) -> Result<Option<String>> {
            self.prompts += 1;
            Ok(Some(self.line.to_string()))
        }
    }

    #[test]
    fn a_prompt_of_the_remote_command_is_answered_in_the_invoking_terminal() {
        let project = tempfile::tempdir().unwrap();
        let mut request = request(project.path(), None, "");
        request.argv = vec![
            "env".to_string(),
            format!("{}={PROMPT_CHILD}", crate::test_support::ISOLATED_TEST_ENV),
            std::env::current_exe().unwrap().display().to_string(),
            PROMPT_CHILD.to_string(),
            "--exact".to_string(),
            "--ignored".to_string(),
            "--nocapture".to_string(),
        ];
        let mut terminal = Typing {
            line: "y\n",
            stdout: String::new(),
            prompts: 0,
        };

        let exit = channels().run_command(
            &devbox(),
            &request,
            &InvocationContext::default(),
            &mut terminal,
            false,
        );

        assert_eq!(exit.unwrap(), 0, "{}", terminal.stdout);
        assert_eq!(terminal.prompts, 1);
        assert!(
            terminal.stdout.contains("answer: y\n"),
            "{}",
            terminal.stdout
        );
    }

    #[test]
    fn the_command_runs_in_the_clone_with_the_project_identity() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().canonicalize().unwrap();

        let (exit, captured) = run(
            &request(
                &path,
                None,
                "pwd; echo $WORKMUX_REMOTE_NAME $WORKMUX_PROJECT_NAME",
            ),
            |_| {},
        );

        assert_eq!(exit.unwrap(), 0);
        assert_eq!(captured.stdout, format!("{}\ndevbox api\n", path.display()));
    }

    #[test]
    fn a_command_from_a_worktree_window_runs_in_that_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let clone = temp.path().canonicalize().unwrap().join("api");
        std::fs::create_dir(&clone).unwrap();
        crate::test_support::init_repo(&clone);
        let worktree = clone.with_file_name("api__worktrees").join("feature");
        crate::test_support::run_git(
            &clone,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        crate::test_support::run_git(
            &clone,
            &["config", "workmux.worktree.feature.window-token", "f00d"],
        );
        let mut from_window = request(&clone, None, "pwd");
        from_window.invoking_window_token = Some("f00d".to_string());
        let mut from_elsewhere = from_window.clone();
        from_elsewhere.invoking_window_token = Some("beef".to_string());

        let (exit, captured) = run(&from_window, |_| {});
        let (_, elsewhere) = run(&from_elsewhere, |_| {});

        assert_eq!(exit.unwrap(), 0);
        assert_eq!(captured.stdout, format!("{}\n", worktree.display()));
        assert_eq!(elsewhere.stdout, format!("{}\n", clone.display()));
    }

    #[test]
    fn first_use_clones_the_repository_to_the_projects_path() {
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
    fn a_first_use_clone_shows_the_ssh_url_its_https_url_was_rewritten_from() {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        crate::test_support::init_repo(&origin);
        let path = temp.path().join("api");
        let mut request = request(&path, Some(&origin), "true");
        if let Some(git_url) = &mut request.project.git_url {
            git_url.ssh = Some("git@github.com:me/api.git".to_string());
        }

        let (exit, captured) = run(&request, |_| {});

        assert_eq!(exit.unwrap(), 0);
        assert!(
            captured.stderr.contains(&format!(
                "Cloning {} (rewritten from SSH: git@github.com:me/api.git) into {} on devbox",
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
                "Project api has no repository URL and {} does not exist on devbox",
                path.display()
            )
        );
    }
}
