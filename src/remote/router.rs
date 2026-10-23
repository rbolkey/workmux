//! Command router: resolves the project a command targets and relays the
//! command to remote workmux when the project lives on a remote.
//!
//! Target resolution: an explicit `--project`, then the project tag on the
//! current tmux session, then today's cwd-based resolution for local
//! projects.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::PathBuf;

use super::forwarded;
use super::session::ServedProject;
use super::session::{Answer, CommandRequest, OutputParams, ProjectSite, Stream, Terminal};
use crate::config::{Config, ProjectPlacement, RemoteConfig};
use crate::multiplexer::{InvocationContext, TmuxBackend, create_backend, detect_backend};
use crate::project::Project;

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
    let own = forwarded::project_tag(&project.site.remote, &project.site.name);
    (tag.trim() == own && !token.trim().is_empty()).then(|| token.trim().to_string())
}

/// The registered remote project `tag` names, if it still is one.
fn project_tagged(config: &Config, tag: &str) -> Option<RemoteProject> {
    let (remote, name) = forwarded::parse_project_tag(tag)?;
    match remote_project(config, name) {
        Ok(project) if project.site.remote == remote => Some(project),
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

/// Run a command session for `project`, through the workstation daemon,
/// which holds the control channels, or directly when no daemon can run.
fn session(project: &RemoteProject, argv: &[&str], terminal: &mut dyn Terminal) -> Result<i32> {
    let request = CommandRequest {
        project: project.site.clone(),
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
        invoking_window_token: invoking_window_token(project),
    };
    let Some(socket) = daemon_socket() else {
        return super::channels().run_command(
            &project.site.remote,
            &project.remote,
            &request,
            InvocationContext::current(),
            terminal,
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
    session(project, argv, &mut Invoking { output: write_to })
}

/// Run `argv` for `project` on its remote, streaming its stderr to this
/// terminal and returning its exit code and stdout.
pub fn capture(project: &RemoteProject, argv: &[&str]) -> Result<(i32, Vec<u8>)> {
    let mut stdout = Vec::new();
    let exit = session(
        project,
        argv,
        &mut Invoking {
            output: |stream, data: &[u8]| match stream {
                Stream::Stdout => stdout.extend_from_slice(data),
                Stream::Stderr => write_to(stream, data),
            },
        },
    )?;
    Ok((exit, stdout))
}

/// Run `argv` for `project` on its remote, discarding its output. Returns
/// its exit code.
pub fn quietly(project: &RemoteProject, argv: &[&str]) -> Result<i32> {
    session(project, argv, &mut Invoking { output: |_, _: &[u8]| {} })
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

/// Relay this command, as it was typed, to `project`'s remote and exit
/// with the remote command's exit code.
pub fn relay(project: &RemoteProject) -> Result<()> {
    let argv = relayed_argv();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    exit_with(run(project, &argv)?)
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
