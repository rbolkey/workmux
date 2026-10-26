//! Command router: resolves the project a command targets and relays the
//! command to remote workmux when the project lives on a remote.
//!
//! Target resolution: the project tag on the window (or session) the
//! command was run from, then the config resolved in the working
//! directory's repository (ADR 0004), which puts the project on the
//! workstation or in a sandbox such as OpenShell's. There is no flag naming
//! another project: a command targets the project it runs in.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::forwarded;
use super::session::ServedProject;
use super::session::{Answer, CommandRequest, OutputParams, ProjectSite, Stream, Terminal};
use super::transport::RemoteState;
use crate::config::{Config, Placement, RemoteConfig};
use crate::multiplexer::{InvocationContext, TmuxBackend, create_backend, detect_backend};
use crate::project::{Project, ProjectTag, RemoteProjectId};

/// A project on a remote, resolved from its repository's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProject {
    /// The workstation clone whose config puts the project on its remote.
    pub root: PathBuf,
    /// The project as remote workmux needs it, naming its remote.
    pub site: ProjectSite,
    /// How the workstation reaches that remote.
    pub remote_config: RemoteConfig,
}

impl RemoteProject {
    pub fn project(&self) -> Project {
        Project::remote(&self.site.remote, &self.site.name)
    }

    pub fn id(&self) -> RemoteProjectId {
        self.site.id()
    }

    /// The tag on the project's workstation sessions and windows.
    #[cfg(test)]
    pub fn tag(&self) -> ProjectTag {
        ProjectTag::new(self.id(), &self.root)
    }

    /// The remote the project is on.
    pub fn host(&self) -> super::Remote<'_> {
        super::Remote::new(&self.site.remote, &self.remote_config)
    }

    /// Where the project runs, as a command targeting it says once.
    pub fn target(&self) -> String {
        match self.remote_config.transport {
            crate::config::TransportKind::Openshell => format!(
                "OpenShell sandbox {} (project {})",
                self.remote_config.target, self.site.name
            ),
            _ => format!("remote {} (project {})", self.site.remote, self.site.name),
        }
    }

    /// Why a command that only reports finds nothing to look at, for a
    /// remote in `state`: it doesn't exist yet, or it is stopped. `None`
    /// when it is running.
    pub fn not_running(&self, state: RemoteState) -> Option<String> {
        match state {
            RemoteState::Running => None,
            RemoteState::Missing => Some(match self.remote_config.transport {
                crate::config::TransportKind::Openshell => format!(
                    "No OpenShell sandbox for {} yet; `workmux add <branch>` creates it",
                    self.site.name
                ),
                _ => format!(
                    "No remote {} for project {} yet; `workmux add <branch>` creates it",
                    self.site.remote, self.site.name
                ),
            }),
            RemoteState::Stopped => {
                let target = self.target();
                let mut first = target.chars();
                let target = first
                    .next()
                    .map(|c| c.to_uppercase().chain(first).collect::<String>())
                    .unwrap_or_default();
                Some(format!(
                    "{target} is stopped; `workmux remote start` starts it"
                ))
            }
        }
    }
}

/// Refuse `what`, which would open an editor, for `project`: the command
/// runs on the remote with no terminal to open one in, and would edit a
/// file there.
pub fn refuse_editor(project: &RemoteProject, what: &str, instead: &str) -> anyhow::Error {
    anyhow!(
        "{what} cannot open an editor for remote project '{}' on {}. {instead}",
        project.site.name,
        project.site.remote
    )
}

/// The remote project `name` on `remote`, resolved from the clone its
/// open workstation windows are tagged with.
pub fn remote_project_on(remote: &str, name: &str) -> Result<RemoteProject> {
    super::resolve::open_project(remote, name)
}

/// Every remote project with open workstation windows.
pub fn remote_projects() -> Vec<RemoteProject> {
    super::resolve::open_projects()
}

/// The remote project a command targets, or `None` for a local project:
/// the project the invoking tmux window (or its session) is tagged with,
/// else the one the working directory's repository config puts in a
/// sandbox. Commands remote workmux runs are never routed again.
pub fn resolve() -> Result<Option<RemoteProject>> {
    if on_a_remote() {
        return Ok(None);
    }
    if let Some(tag) = invoking_project_tag()
        && let Some(project) = super::resolve::tagged(&tag)?
    {
        return Ok(Some(project));
    }
    std::env::current_dir()
        .ok()
        .map(|dir| super::resolve::at(&dir))
        .transpose()
        .map(Option::flatten)
}

/// Whether this process runs on a remote (for a command session, in a
/// remote pane's shell, or in an OpenShell sandbox), where the project's
/// config describes the sandbox it is already in.
pub fn on_a_remote() -> bool {
    ServedProject::current().is_some()
        || super::shell::Shell::current().is_some()
        || std::env::var_os(super::openshell::SANDBOX_ENV).is_some()
}

/// The project tag of the tmux window this command was run from, else of
/// its session: a window or session workmux opened for a project. Commands
/// remote workmux runs are never routed again.
fn invoking_project_tag() -> Option<ProjectTag> {
    if ServedProject::current().is_some() {
        return None;
    }
    let invocation = InvocationContext::current();
    let pane = invocation.pane_id.as_deref()?;
    let tmux = TmuxBackend::for_socket(invocation.tmux_socket_path()?);
    let tags = tmux
        .query(&[
            "display-message",
            "-p",
            "-t",
            pane,
            &format!(
                "#{{{}}}\t#{{{}}}",
                forwarded::WINDOW_TAG,
                forwarded::SESSION_TAG
            ),
        ])
        .ok()?;
    project_tag_of(&tags)
}

/// The project tag in a `<window tag>\t<session tag>` line: the window's,
/// else the session's.
fn project_tag_of(tags: &str) -> Option<ProjectTag> {
    // The query may come back trimmed, losing the tab before a session
    // tag when the window has none.
    tags.split('\t')
        .map(str::trim)
        .find(|tag| !tag.is_empty())
        .and_then(ProjectTag::parse)
}

/// The ownership token of the window this command was typed in, when that
/// window belongs to `project`, so the remote runs the command in its
/// worktree.
fn invoking_window_token(project: &RemoteProject) -> Option<String> {
    if ServedProject::current().is_some() {
        return None;
    }
    let invocation = InvocationContext::current();
    let pane = invocation.pane_id.as_deref()?;
    let tmux = TmuxBackend::for_socket(invocation.tmux_socket_path()?);
    let window = tmux
        .query(&[
            "display-message",
            "-p",
            "-t",
            pane,
            &format!("#{{{}}}\t#{{@workmux_token}}", forwarded::WINDOW_TAG),
        ])
        .ok()?;
    window_token_for(project, &window)
}

/// The token in a `<window tag>\t<token>` line, when the tag is `project`'s.
fn window_token_for(project: &RemoteProject, window: &str) -> Option<String> {
    let (tag, token) = window.trim_end_matches('\n').split_once('\t')?;
    (RemoteProjectId::from_tag(tag) == Some(project.id()) && !token.trim().is_empty())
        .then(|| token.trim().to_string())
}

/// The workstation daemon's socket, when a tmux server can host it.
pub fn daemon_socket() -> Option<PathBuf> {
    let mux = create_backend(detect_backend());
    if mux.name() != "tmux" || !mux.is_running().unwrap_or(false) {
        return None;
    }
    match crate::workstation_daemon::ensure_current() {
        Ok(socket) => Some(socket),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "workstation daemon unavailable; opening control channels directly");
            None
        }
    }
}

/// How long the dashboard reuses the workstation daemon's socket before
/// checking it again: about one refresh.
const DAEMON_REUSE: Duration = Duration::from_secs(2);

static RECENT_DAEMON: Mutex<Option<(Instant, Option<PathBuf>)>> = Mutex::new(None);

/// [`daemon_socket`], reused for [`DAEMON_REUSE`]: for a long-lived
/// process making many requests, such as the dashboard.
pub fn recent_daemon_socket() -> Option<PathBuf> {
    let mut recent = RECENT_DAEMON.lock().unwrap();
    if let Some((at, socket)) = recent.as_ref()
        && at.elapsed() < DAEMON_REUSE
    {
        return socket.clone();
    }
    let socket = daemon_socket();
    *recent = Some((Instant::now(), socket.clone()));
    socket
}

/// Forget the socket [`recent_daemon_socket`] reuses, after a call to it
/// failed.
pub fn forget_recent_daemon_socket() {
    *RECENT_DAEMON.lock().unwrap() = None;
}

/// What a command session does about its remote's state before it runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Readiness {
    /// Make the remote ready with [`ready`], starting it.
    Start,
    /// The caller has just found the remote running with [`ready`].
    Running,
    /// Leave the remote as it is; the channel's opener checks its state.
    AsItIs,
}

/// Run a command session for `project`, through the workstation daemon,
/// which holds the control channels, or directly when no daemon can run.
/// A remote found running is not checked again on the way.
fn session(
    project: &RemoteProject,
    argv: &[&str],
    prompt_file: Option<String>,
    terminal: &mut dyn Terminal,
    readiness: Readiness,
) -> Result<i32> {
    let running = match readiness {
        Readiness::Start => ready(project, true)?,
        Readiness::Running => true,
        Readiness::AsItIs => false,
    };
    session_once(project, argv, prompt_file, terminal, running)
}

fn session_once(
    project: &RemoteProject,
    argv: &[&str],
    prompt_file: Option<String>,
    terminal: &mut dyn Terminal,
    running: bool,
) -> Result<i32> {
    let request = CommandRequest {
        project: project.site.clone(),
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
        invoking_window_token: invoking_window_token(project),
        prompt_file,
        placement: Placement::of(&Config::load_global_only()?),
    };
    let Some(socket) = daemon_socket() else {
        return super::channels().run_command(
            project,
            &request,
            InvocationContext::current(),
            terminal,
            running,
        );
    };
    let mut undelivered = None;
    let result = crate::workstation_daemon::call_streaming(
        &socket,
        crate::workstation_daemon::COMMAND_SESSION_METHOD,
        serde_json::json!({
            "root": project.root,
            "request": request,
            "invocation": InvocationContext::current(),
            "running": running,
        }),
        None,
        &mut |method, params| match method {
            super::session::OUTPUT_METHOD => {
                match serde_json::from_value::<OutputParams>(params)
                    .map_err(anyhow::Error::from)
                    .and_then(|output| Ok((output.stream, output.bytes()?)))
                {
                    Ok((stream, data)) => terminal.output(stream, &data),
                    Err(error) => undelivered = Some(error),
                }
                None
            }
            super::session::PROMPT_METHOD => {
                let line = terminal.read_line().unwrap_or_else(|error| {
                    tracing::warn!(error = %format!("{error:#}"), "reading a prompt answer");
                    None
                });
                Some(serde_json::to_value(Answer { line }).expect("serializes"))
            }
            _ => None,
        },
    )?;
    if let Some(error) = undelivered {
        return Err(error.context("Workstation daemon sent unreadable command output"));
    }
    let exit: super::session::Exit =
        serde_json::from_value(result).context("Workstation daemon sent no exit code")?;
    Ok(exit.exit_code)
}

/// This process's terminal: output to stdout and stderr, prompts answered
/// from stdin.
struct Invoking<F> {
    output: F,
}

impl<F: FnMut(Stream, &[u8])> Terminal for Invoking<F> {
    fn output(&mut self, stream: Stream, data: &[u8]) {
        (self.output)(stream, data)
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        Ok(match std::io::stdin().read_line(&mut line)? {
            0 => None,
            _ => Some(line),
        })
    }
}

fn write_to(stream: Stream, data: &[u8]) {
    let _ = match stream {
        Stream::Stdout => {
            let mut out = std::io::stdout().lock();
            out.write_all(data).and_then(|()| out.flush())
        }
        Stream::Stderr => {
            let mut err = std::io::stderr().lock();
            err.write_all(data).and_then(|()| err.flush())
        }
    };
}

/// Run `argv` for `project` on its remote, streaming its output to this
/// terminal. Returns the command's exit code.
pub fn run(project: &RemoteProject, argv: &[&str]) -> Result<i32> {
    session(
        project,
        argv,
        None,
        &mut Invoking { output: write_to },
        Readiness::Start,
    )
}

/// Make `project`'s remote ready in this process, so creating or starting
/// it reports to this terminal: with `start`, a missing remote is created
/// and a stopped one started. Returns whether it is running.
pub fn ready(project: &RemoteProject, start: bool) -> Result<bool> {
    Ok(state(project, start)? == RemoteState::Running)
}

/// Like [`ready`], returning the remote's state: running, else as it was
/// found.
fn state(project: &RemoteProject, start: bool) -> Result<RemoteState> {
    let transport = super::transport::build(&project.remote_config)?;
    super::transport::ensure_running(
        &project.site.remote,
        transport.as_ref(),
        start,
        &mut |notice| eprintln!("{notice}"),
    )
}

/// For a command that only reports: why `project`'s remote has nothing to
/// look at, as [`RemoteProject::not_running`] says, or `None` when it is
/// running. A missing remote is never created to look, nor a stopped one
/// started (ADR 0004: the sandbox is created on the first `workmux add`).
pub fn not_running(project: &RemoteProject) -> Result<Option<String>> {
    Ok(project.not_running(state(project, false)?))
}

/// Run `argv` for `project` on its remote like [`run`], on a remote the
/// caller has just found running with [`not_running`].
pub fn run_running(project: &RemoteProject, argv: &[&str]) -> Result<i32> {
    session(
        project,
        argv,
        None,
        &mut Invoking { output: write_to },
        Readiness::Running,
    )
}

/// Run `argv` for `project` on its remote, streaming its stderr to this
/// terminal and returning its exit code and stdout. The caller has just
/// found the remote running with [`ready`]; it is not checked again.
pub fn capture_running(project: &RemoteProject, argv: &[&str]) -> Result<(i32, Vec<u8>)> {
    let mut stdout = Vec::new();
    let exit = session(
        project,
        argv,
        None,
        &mut Invoking {
            output: |stream, data: &[u8]| match stream {
                Stream::Stdout => stdout.extend_from_slice(data),
                Stream::Stderr => write_to(stream, data),
            },
        },
        Readiness::Running,
    )?;
    Ok((exit, stdout))
}

/// Run `argv` for `project` on its remote, discarding its output, without
/// starting a stopped remote: for polls. Returns its exit code.
pub fn quietly(project: &RemoteProject, argv: &[&str]) -> Result<i32> {
    session(
        project,
        argv,
        None,
        &mut Invoking {
            output: |_, _: &[u8]| {},
        },
        Readiness::AsItIs,
    )
}

/// What a command run with [`captured`] printed.
#[derive(Debug, Default)]
pub struct Captured {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A terminal that keeps everything and answers every prompt with end of
/// input, as a closed stdin would: for callers that own the real terminal,
/// such as the dashboard.
#[derive(Default)]
struct Collecting {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Terminal for Collecting {
    fn output(&mut self, stream: Stream, data: &[u8]) {
        match stream {
            Stream::Stdout => self.stdout.extend_from_slice(data),
            Stream::Stderr => self.stderr.extend_from_slice(data),
        }
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Run `argv` for `project` on its remote without touching this process's
/// terminal: output is kept and prompts read end of input.
pub fn captured(project: &RemoteProject, argv: &[&str]) -> Result<Captured> {
    let mut terminal = Collecting::default();
    let exit_code = session(project, argv, None, &mut terminal, Readiness::AsItIs)?;
    Ok(Captured {
        exit_code,
        stdout: terminal.stdout,
        stderr: terminal.stderr,
    })
}

/// This command's own arguments, for relaying to remote workmux: `workmux`
/// and the arguments it was run with.
pub fn relayed_argv() -> Vec<String> {
    std::iter::once("workmux".to_string())
        .chain(std::env::args().skip(1))
        .collect()
}

/// `argv`, a relayed `workmux <subcommand> ...` defined by `command`, less
/// its `--prompt-file`/`-P` option, however it was spelled.
fn without_prompt_file(argv: Vec<String>, command: &clap::Command) -> Vec<String> {
    rewrite_option(argv, command, "prompt_file", |_| Ok(Rewrite::Drop))
        .expect("dropping an option cannot fail")
}

/// `argv`, a relayed `workmux <subcommand> ...` defined by `command`, with
/// each `--agent`/`-a` that names a workstation alias from `config`'s
/// `agents:` replaced by the alias's agent type, which the remote can run
/// where the alias means nothing. Any other agent is relayed as typed.
fn with_agent_types(
    argv: Vec<String>,
    command: &clap::Command,
    config: &Config,
) -> Result<Vec<String>> {
    rewrite_option(argv, command, "agent", |agent| {
        if !config.agents.contains_key(agent) {
            return Ok(Rewrite::Keep);
        }
        config.agent_type_of(agent).map(Rewrite::Replace).ok_or_else(|| {
            anyhow!(
                "Agent '{agent}' is a workstation alias with no agent type a remote project can run. \
                 Set its type: in agents: (claude, codex, ...) or pass the agent's plain CLI."
            )
        })
    })
}

/// What [`rewrite_option`] does with one of an option's values.
enum Rewrite {
    Keep,
    Replace(String),
    Drop,
}

/// `argv`, a relayed `workmux <subcommand> ...` defined by `command`, with
/// each value of its option `id`, however it was spelled, kept, replaced or
/// dropped (with the option) as `rewrite` says. A replaced value is spelled
/// `--long=value`.
fn rewrite_option(
    argv: Vec<String>,
    command: &clap::Command,
    id: &str,
    mut rewrite: impl FnMut(&str) -> Result<Rewrite>,
) -> Result<Vec<String>> {
    let option = command
        .get_arguments()
        .find(|a| a.get_id() == id)
        .expect("the relayed command has the option");
    let (short, long) = (option.get_short(), option.get_long());
    let takes_value =
        |found: Option<&clap::Arg>| found.is_some_and(|arg| arg.get_action().takes_values());
    // The option as typed, `typed`, for its value `value`, after any
    // flags clustered `before` it, respelled as `rewrite` says.
    let mut respelled = |before: Option<String>, typed: Vec<String>, value: &str| {
        let option = match rewrite(value)? {
            Rewrite::Keep => return Ok::<_, anyhow::Error>(typed),
            Rewrite::Replace(value) => Some(format!("--{}={value}", long.unwrap_or(id))),
            Rewrite::Drop => None,
        };
        Ok(before.into_iter().chain(option).collect())
    };
    let mut args = argv.into_iter();
    let mut kept: Vec<String> = args.by_ref().take(2).collect();
    while let Some(arg) = args.next() {
        if arg == "--" {
            kept.push(arg);
            kept.extend(args.by_ref());
            break;
        }
        if let Some(long_arg) = arg.strip_prefix("--") {
            let (name, inline) = match long_arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long_arg, None),
            };
            if long == Some(name) {
                let (typed, value) = typed_value(&arg, inline, &mut args);
                kept.extend(respelled(None, typed, &value)?);
                continue;
            }
            let wants_next = inline.is_none()
                && takes_value(command.get_arguments().find(|a| a.get_long() == Some(name)));
            kept.push(arg);
            if wants_next {
                kept.extend(args.next());
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
            kept.push(arg);
            continue;
        };
        // A cluster of short flags: the first one that takes a value takes
        // the rest of the cluster, or the next argument when none is left.
        let mut wants_next = false;
        let mut kept_cluster = vec![arg.clone()];
        for (at, flag) in cluster.char_indices() {
            if !takes_value(
                command
                    .get_arguments()
                    .find(|a| a.get_short() == Some(flag)),
            ) {
                continue;
            }
            let inline = &cluster[at + flag.len_utf8()..];
            wants_next = inline.is_empty();
            if Some(flag) == short {
                let (typed, value) =
                    typed_value(&arg, (!wants_next).then(|| inline.to_string()), &mut args);
                wants_next = false;
                let before = (at > 0).then(|| format!("-{}", &cluster[..at]));
                kept_cluster = respelled(before, typed, &value)?;
            }
            break;
        }
        kept.extend(kept_cluster);
        if wants_next {
            kept.extend(args.next());
        }
    }
    Ok(kept)
}

/// An option as typed, `arg`, with its value: `inline`, else the next of
/// `args`, which is then typed too.
fn typed_value(
    arg: &str,
    inline: Option<String>,
    args: &mut impl Iterator<Item = String>,
) -> (Vec<String>, String) {
    let mut typed = vec![arg.to_string()];
    let value = inline.unwrap_or_else(|| {
        let value = args.next().unwrap_or_default();
        typed.push(value.clone());
        value
    });
    (typed, value)
}

/// Relay this command, as it was typed, to `project`'s remote and exit
/// with the remote command's exit code.
pub fn relay(project: &RemoteProject) -> Result<()> {
    announce(project);
    relay_argv(project, &relayed_argv())
}

/// Say once which remote a relayed command runs on, so the target is
/// always visible.
fn announce(project: &RemoteProject) {
    eprintln!("Running in {}", project.target());
}

/// Run `argv` on `project`'s remote and exit with its exit code.
fn relay_argv(project: &RemoteProject, argv: &[String]) -> Result<()> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    exit_with(run(project, &argv)?)
}

/// The largest prompt file relayed to a remote: well within the control
/// channel's line limit.
const MAX_PROMPT_FILE_BYTES: u64 = 1024 * 1024;

/// Relay this command, `command` as it was typed, to `project`'s remote like
/// [`relay`], with the arguments that mean something only on the workstation
/// resolved here: its prompt file, `prompt_file`, read where the user means
/// it rather than on the remote, and any workstation agent alias it names
/// translated to its agent type.
pub fn relay_with_workstation_args(
    project: &RemoteProject,
    prompt_file: Option<&Path>,
    command: &clap::Command,
) -> Result<()> {
    announce(project);
    let mut argv = relayed_argv();
    if command.get_arguments().any(|a| a.get_id() == "agent") {
        argv = with_agent_types(argv, command, &Config::load_global_only()?)?;
    }
    let Some(path) = prompt_file else {
        return relay_argv(project, &argv);
    };
    let contents = read_prompt_file(path)?;
    let argv = without_prompt_file(argv, command);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    exit_with(session(
        project,
        &argv,
        Some(contents),
        &mut Invoking { output: write_to },
        Readiness::Start,
    )?)
}

fn read_prompt_file(path: &Path) -> Result<String> {
    let failed = || format!("Failed to read prompt file: {}", path.display());
    let size = std::fs::metadata(path).with_context(failed)?.len();
    if size > MAX_PROMPT_FILE_BYTES {
        return Err(anyhow!(
            "Prompt file {} is {size} bytes, more than the {MAX_PROMPT_FILE_BYTES} bytes a remote project accepts",
            path.display()
        ));
    }
    std::fs::read_to_string(path).with_context(failed)
}

/// Relay this command, as it was typed, to every remote project with an
/// open workstation window and to the one this command runs in, printing
/// each one's output under its name; a project with nothing to say prints
/// nothing. Returns how many projects failed or could not be reached.
pub fn relay_to_every_project() -> Result<usize> {
    let argv = relayed_argv();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut projects = remote_projects();
    if let Some(here) = resolve()?
        && !projects.iter().any(|project| project.id() == here.id())
    {
        projects.insert(0, here);
    }
    let mut failures = 0;
    for project in projects {
        let (name, remote) = (&project.site.name, &project.site.remote);
        // A stopped sandbox is never started just to visit it, nor a
        // missing one created.
        match not_running(&project) {
            Ok(None) => {}
            Ok(Some(why)) => {
                eprintln!(
                    "Skipping remote project '{name}': {why} (run it in a clone of its repository)"
                );
                continue;
            }
            Err(error) => {
                eprintln!("Failed to reach remote project '{name}' on {remote}: {error:#}");
                failures += 1;
                continue;
            }
        }
        match capture_running(&project, &argv) {
            Ok((code, stdout)) => {
                if !stdout.is_empty() {
                    println!("\n{name} (on {remote}):");
                    print!("{}", String::from_utf8_lossy(&stdout));
                }
                if code != 0 {
                    failures += 1;
                }
            }
            Err(error) => {
                eprintln!("Failed to reach remote project '{name}' on {remote}: {error:#}");
                failures += 1;
            }
        }
    }
    Ok(failures)
}

/// Finish a command relayed to a remote with the remote command's exit code.
pub fn exit_with(code: i32) -> Result<()> {
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn api() -> RemoteProject {
        crate::remote::testing::devbox_api()
    }

    #[test]
    fn a_relayed_command_loses_its_prompt_file_however_it_was_spelled() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let add = crate::cli::subcommand("add").unwrap();
        let relayed = |list: &[&str]| without_prompt_file(args(list), &add);

        for spelled in [
            &["-P", "p.md"][..],
            &["-Pp.md"],
            &["--prompt-file", "p.md"],
            &["--prompt-file=p.md"],
        ] {
            let typed = [&["workmux", "add", "feature"][..], spelled, &["-w"]].concat();
            assert_eq!(relayed(&typed), args(&["workmux", "add", "feature", "-w"]));
        }
        // Flags clustered with -P stay; -P inside another flag's value is
        // that value, as is anything after `--`.
        assert_eq!(
            relayed(&["workmux", "add", "feature", "-HP", "p.md"]),
            args(&["workmux", "add", "feature", "-H"])
        );
        assert_eq!(
            relayed(&["workmux", "add", "-HPp.md", "feature"]),
            args(&["workmux", "add", "-H", "feature"])
        );
        let untouched = ["workmux", "add", "feature", "-bpPlease"];
        assert_eq!(relayed(&untouched), args(&untouched));
        let untouched = ["workmux", "add", "feature", "--", "-P", "p.md"];
        assert_eq!(relayed(&untouched), args(&untouched));
    }

    #[test]
    fn a_relayed_agent_alias_becomes_its_agent_type() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let add = crate::cli::subcommand("add").unwrap();
        let config = config(
            "\
agents:
  cc: /opt/wrappers/claude --dangerously-skip-permissions
  cx:
    command: /opt/wrappers/run-agent
    type: codex
",
        );
        let relayed = |list: &[&str]| with_agent_types(args(list), &add, &config).unwrap();

        for (spelled, typed) in [
            (&["-a", "cc"][..], &["--agent=claude"][..]),
            (&["-acc"], &["--agent=claude"]),
            (&["--agent", "cx"], &["--agent=codex"]),
            (&["--agent=cx"], &["--agent=codex"]),
            (&["-ba", "cc"], &["-b", "--agent=claude"]),
            (&["-bacc"], &["-b", "--agent=claude"]),
        ] {
            let typed_argv = [&["workmux", "add", "feature"][..], typed, &["-w"]].concat();
            let spelled_argv = [&["workmux", "add", "feature"][..], spelled, &["-w"]].concat();
            assert_eq!(relayed(&spelled_argv), args(&typed_argv), "{spelled:?}");
        }
        // Several agents each translate; a plain CLI is relayed as typed, as
        // is an alias's name inside another flag's value or after `--`.
        assert_eq!(
            relayed(&[
                "workmux", "add", "f", "-a", "cc", "-a", "gemini", "-a", "cx"
            ]),
            args(&[
                "workmux",
                "add",
                "f",
                "--agent=claude",
                "-a",
                "gemini",
                "--agent=codex"
            ])
        );
        let untouched = ["workmux", "add", "f", "-p", "-a cc", "--", "-a", "cc"];
        assert_eq!(relayed(&untouched), args(&untouched));
    }

    #[test]
    fn a_relayed_agent_alias_without_a_known_type_is_refused() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let add = crate::cli::subcommand("add").unwrap();
        let config = config(
            "\
agents:
  mine: /opt/wrappers/run-agent
  odd:
    command: /opt/wrappers/run-agent
    type: notanagent
",
        );

        for alias in ["mine", "odd"] {
            let refused =
                with_agent_types(args(&["workmux", "add", "f", "-a", alias]), &add, &config)
                    .unwrap_err()
                    .to_string();
            assert!(refused.contains(&format!("'{alias}'")), "{refused}");
            assert!(refused.contains("type:"), "{refused}");
        }
    }

    #[test]
    fn a_prompt_file_too_large_for_a_remote_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.md");
        std::fs::write(&path, "x".repeat(MAX_PROMPT_FILE_BYTES as usize)).unwrap();
        assert!(read_prompt_file(&path).is_ok());

        std::fs::write(&path, "x".repeat(MAX_PROMPT_FILE_BYTES as usize + 1)).unwrap();
        let refused = read_prompt_file(&path).unwrap_err().to_string();
        assert!(refused.contains("more than the 1048576 bytes"), "{refused}");
    }

    #[test]
    fn a_command_names_the_sandbox_it_targets() {
        let mut api = api();
        assert_eq!(api.target(), "remote devbox (project api)");
        api.remote_config.transport = crate::config::TransportKind::Openshell;
        api.remote_config.target = "wm-api".to_string();
        assert_eq!(api.target(), "OpenShell sandbox wm-api (project api)");
    }

    #[test]
    fn a_command_that_only_reports_says_why_the_sandbox_has_nothing_to_show() {
        let mut api = api();
        api.remote_config.transport = crate::config::TransportKind::Openshell;
        api.remote_config.target = "wm-api".to_string();

        assert_eq!(api.not_running(RemoteState::Running), None);
        assert_eq!(
            api.not_running(RemoteState::Missing).as_deref(),
            Some("No OpenShell sandbox for api yet; `workmux add <branch>` creates it")
        );
        assert_eq!(
            api.not_running(RemoteState::Stopped).as_deref(),
            Some(
                "OpenShell sandbox wm-api (project api) is stopped; `workmux remote start` starts it"
            )
        );
    }

    #[test]
    fn a_project_window_targets_its_project_before_its_session() {
        let tag = |line: &str| project_tag_of(line).map(|tag| tag.to_string());
        assert_eq!(
            tag("devbox:api:/src/api\tdevbox:web:/src/web\n").as_deref(),
            Some("devbox:api:/src/api")
        );
        assert_eq!(
            tag("\tdevbox:web:/src/web\n").as_deref(),
            Some("devbox:web:/src/web")
        );
        assert_eq!(
            tag("devbox:web:/src/web").as_deref(),
            Some("devbox:web:/src/web")
        );
        assert_eq!(tag("\t\n"), None);
    }

    #[test]
    fn only_a_window_of_the_project_tells_the_remote_its_worktree() {
        let api = api();

        assert_eq!(
            window_token_for(&api, "devbox:api:/src/api\tf00d\n").as_deref(),
            Some("f00d")
        );
        assert_eq!(window_token_for(&api, "devbox:web:/src/web\tf00d\n"), None);
        assert_eq!(window_token_for(&api, "\tf00d\n"), None);
        assert_eq!(window_token_for(&api, "devbox:api:/src/api\t\n"), None);
    }
}
