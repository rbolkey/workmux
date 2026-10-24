//! Command router: resolves the project a command targets and relays the
//! command to remote workmux when the project lives on a remote.
//!
//! Target resolution: an explicit `--project`, then the project tag on the
//! current tmux session, then today's cwd-based resolution for local
//! projects.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::forwarded;
use super::session::ServedProject;
use super::session::{Answer, CommandRequest, OutputParams, ProjectSite, Stream, Terminal};
use crate::config::{Config, ProjectPlacement, RemoteConfig};
use crate::multiplexer::{InvocationContext, TmuxBackend, create_backend, detect_backend};
use crate::project::{Project, RemoteProjectId};

/// A project registered on a remote, resolved from global config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProject {
    pub site: ProjectSite,
    pub remote: RemoteConfig,
}

impl RemoteProject {
    pub fn project(&self) -> Project {
        Project::remote(&self.site.remote, &self.site.name)
    }

    pub fn id(&self) -> RemoteProjectId {
        self.site.id()
    }

    /// The remote the project is on.
    pub fn host(&self) -> super::Remote<'_> {
        super::Remote::new(&self.site.remote, &self.remote)
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

/// The remote project `name` in `config`.
pub fn remote_project(config: &Config, name: &str) -> Result<RemoteProject> {
    let entry = config
        .projects
        .get(name)
        .ok_or_else(|| anyhow!("no project named {name} in your global config"))?;
    let remote_name = entry
        .remote
        .as_deref()
        .ok_or_else(|| anyhow!("project {name} is not on a remote"))?;
    let remote = config.remotes.get(remote_name).ok_or_else(|| {
        anyhow!("project {name} is on remote {remote_name}, which is not registered")
    })?;
    Ok(RemoteProject {
        site: ProjectSite {
            remote: remote_name.to_string(),
            name: name.to_string(),
            path: entry.path.clone().unwrap_or_else(|| format!("~/{name}")),
            git_url: entry.git_url.clone(),
            session: match entry.placement.unwrap_or_default() {
                ProjectPlacement::ProjectSession => {
                    Some(entry.session.clone().unwrap_or_else(|| name.to_string()))
                }
                ProjectPlacement::CurrentSession => None,
            },
        },
        remote: remote.clone(),
    })
}

/// What a file last held, read again only once the file changes (its
/// modification time or length), for long-lived processes such as the
/// dashboard that would otherwise read it for every operation.
pub struct FileMemo<T> {
    held: Mutex<Option<(FileVersion, Arc<T>)>>,
}

/// A file's modification time and length, or `None` when there is none.
type FileVersion = Option<(SystemTime, u64)>;

impl<T> Default for FileMemo<T> {
    fn default() -> Self {
        Self {
            held: Mutex::new(None),
        }
    }
}

impl<T> FileMemo<T> {
    /// What `read` makes of the file at `path` (`None`: no file), read
    /// again only once the file changes.
    pub fn get(&self, path: Option<&Path>, read: impl FnOnce() -> Result<T>) -> Result<Arc<T>> {
        let version = path
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|meta| {
                (
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                )
            });
        let mut held = self.held.lock().unwrap();
        if let Some((seen, value)) = held.as_ref()
            && *seen == version
        {
            return Ok(value.clone());
        }
        let value = Arc::new(read()?);
        *held = Some((version, value.clone()));
        Ok(value)
    }
}

/// Global config, read again only once its file changes.
pub fn global_config() -> Result<Arc<Config>> {
    static GLOBAL: OnceLock<FileMemo<Config>> = OnceLock::new();
    GLOBAL.get_or_init(FileMemo::default).get(
        crate::config::global_config_path().as_deref(),
        Config::load_global_only,
    )
}

/// Every project registered on a remote, by name.
pub fn remote_projects() -> Result<Vec<RemoteProject>> {
    let config = Config::load_global_only()?;
    config
        .projects
        .iter()
        .filter(|(_, entry)| entry.remote.is_some())
        .map(|(name, _)| remote_project(&config, name))
        .collect()
}

/// The remote project a command targets, or `None` for a local project:
/// an explicit `--project`, else the project the invoking tmux session is
/// tagged with.
pub fn resolve(explicit: Option<&str>) -> Result<Option<RemoteProject>> {
    if let Some(name) = explicit {
        return Ok(Some(remote_project(&Config::load_global_only()?, name)?));
    }
    match invoking_session_tag() {
        Some(tag) => Ok(project_tagged(&Config::load_global_only()?, &tag)),
        None => Ok(None),
    }
}

/// The project tag of the tmux session this command was run from. Commands
/// remote workmux runs are never routed again.
fn invoking_session_tag() -> Option<String> {
    if ServedProject::current().is_some() {
        return None;
    }
    let invocation = InvocationContext::current();
    let pane = invocation.pane_id.as_deref()?;
    let tmux = TmuxBackend::for_socket(invocation.tmux_socket_path()?);
    let tag = tmux
        .query(&["show-options", "-qv", "-t", pane, forwarded::SESSION_TAG])
        .ok()?;
    Some(tag.trim().to_string()).filter(|tag| !tag.is_empty())
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
    let own = project.id().tag();
    (tag.trim() == own && !token.trim().is_empty()).then(|| token.trim().to_string())
}

/// The registered remote project `tag` names, if it still is one.
fn project_tagged(config: &Config, tag: &str) -> Option<RemoteProject> {
    let id = RemoteProjectId::from_tag(tag)?;
    match remote_project(config, &id.name) {
        Ok(project) if project.site.remote == id.remote => Some(project),
        _ => {
            tracing::warn!(tag, "session is tagged with a project no longer registered");
            None
        }
    }
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
    let request = CommandRequest {
        project: project.site.clone(),
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
        invoking_window_token: invoking_window_token(project),
        prompt_file,
    };
    let Some(socket) = daemon_socket() else {
        return super::channels().run_command(
            project.host(),
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
            "remote": project.site.remote,
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
        return Err(error.context("workstation daemon sent unreadable command output"));
    }
    let exit: super::session::Exit =
        serde_json::from_value(result).context("workstation daemon sent no exit code")?;
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
    let transport = super::registry::transport(&project.site.remote, &project.remote)?;
    super::transport::ensure_running(
        &project.site.remote,
        transport.as_ref(),
        start,
        &mut |notice| eprintln!("{notice}"),
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
/// and the arguments it was run with, less `--project`.
pub fn relayed_argv() -> Vec<String> {
    without_project(std::env::args().skip(1))
}

fn without_project(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut argv = vec!["workmux".to_string()];
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            argv.push(arg);
            argv.extend(args.by_ref());
            break;
        }
        if arg == "--project" {
            args.next();
        } else if !arg.starts_with("--project=") {
            argv.push(arg);
        }
    }
    argv
}

/// `argv`, a relayed `workmux <subcommand> ...` defined by `command`, less
/// its `--prompt-file`/`-P` option, however it was spelled.
fn without_prompt_file(argv: Vec<String>, command: &clap::Command) -> Vec<String> {
    let takes_value =
        |found: Option<&clap::Arg>| found.is_some_and(|arg| arg.get_action().takes_values());
    let mut args = argv.into_iter();
    let mut kept: Vec<String> = args.by_ref().take(2).collect();
    while let Some(arg) = args.next() {
        if arg == "--" {
            kept.push(arg);
            kept.extend(args.by_ref());
            break;
        }
        if arg == "--prompt-file" {
            args.next();
            continue;
        }
        if arg.starts_with("--prompt-file=") {
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let wants_next = !long.contains('=')
                && takes_value(command.get_arguments().find(|a| a.get_long() == Some(long)));
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
        let mut kept_cluster = arg.clone();
        for (at, flag) in cluster.char_indices() {
            if !takes_value(
                command
                    .get_arguments()
                    .find(|a| a.get_short() == Some(flag)),
            ) {
                continue;
            }
            let value = &cluster[at + flag.len_utf8()..];
            wants_next = value.is_empty();
            if flag == 'P' {
                kept_cluster = format!("-{}", &cluster[..at]);
                if wants_next {
                    args.next();
                    wants_next = false;
                }
            }
            break;
        }
        if kept_cluster != "-" {
            kept.push(kept_cluster);
        }
        if wants_next {
            kept.extend(args.next());
        }
    }
    kept
}

/// Relay this command, as it was typed, to `project`'s remote and exit
/// with the remote command's exit code.
pub fn relay(project: &RemoteProject) -> Result<()> {
    let argv = relayed_argv();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    exit_with(run(project, &argv)?)
}

/// The largest prompt file relayed to a remote: well within the control
/// channel's line limit.
const MAX_PROMPT_FILE_BYTES: u64 = 1024 * 1024;

/// Relay this command, `command` as it was typed, to `project`'s remote like
/// [`relay`], with its prompt file, `prompt_file`, read here on the
/// workstation, where the user means it, rather than on the remote.
pub fn relay_with_prompt_file(
    project: &RemoteProject,
    prompt_file: Option<&Path>,
    command: &clap::Command,
) -> Result<()> {
    let Some(path) = prompt_file else {
        return relay(project);
    };
    let contents = read_prompt_file(path)?;
    let argv = without_prompt_file(relayed_argv(), command);
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

/// Relay this command, as it was typed, to every remote project, printing
/// each one's output under its name; a project with nothing to say prints
/// nothing. Returns how many projects failed or could not be reached.
pub fn relay_to_every_project() -> Result<usize> {
    let argv = relayed_argv();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut failures = 0;
    for project in remote_projects()? {
        let (name, remote) = (&project.site.name, &project.site.remote);
        // A stopped sandbox is never started just to visit it.
        match ready(&project, false) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "Skipping remote project '{name}': remote {remote} is stopped (start it with: workmux remote start {remote})"
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

    const REGISTERED: &str = "\
remotes:
  devbox:
    transport: ssh
    target: me@devbox
projects:
  api:
    remote: devbox
    git_url: git@github.com:me/api.git
    path: ~/code/api
  web:
    remote: devbox
    git_url: git@github.com:me/web.git
    session: frontend
  docs:
    remote: devbox
    placement: current-session
  stray:
    remote: gone
";

    #[test]
    fn a_remote_project_is_identified_by_its_registered_name() {
        let api = remote_project(&config(REGISTERED), "api").unwrap();

        assert_eq!(api.project(), Project::remote("devbox", "api"));
        assert_eq!(api.site.path, "~/code/api");
        assert_eq!(
            api.site.git_url.as_deref(),
            Some("git@github.com:me/api.git")
        );
        assert_eq!(api.remote.target, "me@devbox");
    }

    #[test]
    fn a_projects_windows_go_to_its_session_its_override_or_the_invoking_session() {
        let config = config(REGISTERED);
        let session = |name| remote_project(&config, name).unwrap().site.session;

        assert_eq!(session("api").as_deref(), Some("api"));
        assert_eq!(session("web").as_deref(), Some("frontend"));
        assert_eq!(session("docs"), None);
    }

    #[test]
    fn a_session_tag_names_a_project_only_while_it_is_registered_on_that_remote() {
        let config = config(REGISTERED);

        assert_eq!(
            project_tagged(&config, "devbox:api").map(|p| p.project()),
            Some(Project::remote("devbox", "api"))
        );
        assert_eq!(project_tagged(&config, "other:api"), None);
        assert_eq!(project_tagged(&config, "devbox:gone"), None);
        assert_eq!(project_tagged(&config, "garbage"), None);
    }

    #[test]
    fn a_relayed_command_keeps_its_arguments_but_not_its_project() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();

        assert_eq!(
            without_project(args(&["add", "--project", "api", "feature", "-p", "x"])),
            args(&["workmux", "add", "feature", "-p", "x"])
        );
        assert_eq!(
            without_project(args(&[
                "add",
                "--project=api",
                "feature",
                "--",
                "--project"
            ])),
            args(&["workmux", "add", "feature", "--", "--project"])
        );
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
    fn only_a_window_of_the_project_tells_the_remote_its_worktree() {
        let api = remote_project(&config(REGISTERED), "api").unwrap();

        assert_eq!(
            window_token_for(&api, "devbox:api\tf00d\n").as_deref(),
            Some("f00d")
        );
        assert_eq!(window_token_for(&api, "devbox:web\tf00d\n"), None);
        assert_eq!(window_token_for(&api, "\tf00d\n"), None);
        assert_eq!(window_token_for(&api, "devbox:api\t\n"), None);
    }

    #[test]
    fn a_file_is_read_again_only_once_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "one").unwrap();
        let reads = std::cell::Cell::new(0);
        let memo = FileMemo::default();
        let read = || {
            memo.get(Some(&path), || {
                reads.set(reads.get() + 1);
                Ok(std::fs::read_to_string(&path)?)
            })
            .unwrap()
        };

        assert_eq!(*read(), "one");
        assert_eq!(*read(), "one");
        assert_eq!(reads.get(), 1);

        std::fs::write(&path, "two!").unwrap();
        assert_eq!(*read(), "two!");
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn a_project_without_a_path_lives_in_the_remotes_home() {
        let web = remote_project(&config(REGISTERED), "web").unwrap();

        assert_eq!(web.site.path, "~/web");
    }

    #[test]
    fn unknown_projects_and_remotes_are_errors() {
        let config = config(REGISTERED);

        assert_eq!(
            remote_project(&config, "nope").unwrap_err().to_string(),
            "no project named nope in your global config"
        );
        assert_eq!(
            remote_project(&config, "stray").unwrap_err().to_string(),
            "project stray is on remote gone, which is not registered"
        );
    }
}
