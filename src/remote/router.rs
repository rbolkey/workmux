//! Command router: resolves the project a command targets and relays the
//! command to remote workmux when the project lives on a remote.
//!
//! Target resolution: an explicit `--project`, then (once sessions are
//! tagged) the current tmux session's project, then today's cwd-based
//! resolution for local projects.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::PathBuf;

use super::session::{CommandRequest, OutputParams, ProjectSite, Stream};
use crate::config::{Config, RemoteConfig};
use crate::multiplexer::{InvocationContext, create_backend, detect_backend};
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

/// The remote project a command targets, or `None` for a local project.
pub fn resolve(explicit: Option<&str>) -> Result<Option<RemoteProject>> {
    match explicit {
        Some(name) => Ok(Some(remote_project(&Config::load_global_only()?, name)?)),
        None => Ok(None),
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
fn session(
    project: &RemoteProject,
    argv: &[&str],
    sink: &mut dyn FnMut(Stream, &[u8]),
) -> Result<i32> {
    let request = CommandRequest {
        project: project.site.clone(),
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
    };
    let Some(socket) = daemon_socket() else {
        return super::channels().run_command(
            &project.site.remote,
            &project.remote,
            &request,
            InvocationContext::current(),
            sink,
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
        &mut |method, params| {
            if method != super::session::OUTPUT_METHOD {
                return;
            }
            match serde_json::from_value::<OutputParams>(params)
                .map_err(anyhow::Error::from)
                .and_then(|output| Ok((output.stream, output.bytes()?)))
            {
                Ok((stream, data)) => sink(stream, &data),
                Err(error) => undelivered = Some(error),
            }
        },
    )?;
    if let Some(error) = undelivered {
        return Err(error.context("workstation daemon sent unreadable command output"));
    }
    let exit: super::session::Exit =
        serde_json::from_value(result).context("workstation daemon sent no exit code")?;
    Ok(exit.exit_code)
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
    session(project, argv, &mut write_to)
}

/// Run `argv` for `project` on its remote, streaming its stderr to this
/// terminal and returning its exit code and stdout.
pub fn capture(project: &RemoteProject, argv: &[&str]) -> Result<(i32, Vec<u8>)> {
    let mut stdout = Vec::new();
    let exit = session(project, argv, &mut |stream, data| match stream {
        Stream::Stdout => stdout.extend_from_slice(data),
        Stream::Stderr => write_to(stream, data),
    })?;
    Ok((exit, stdout))
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
