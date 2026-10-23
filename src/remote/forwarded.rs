//! The workstation's side of the forwarding multiplexer: answers the
//! multiplexer requests a remote forwards for one command session.
//!
//! Requests act on the workstation's tmux. Context questions are answered
//! from the session's invocation context. Panes run the remote's terminal
//! through the transport's TTY exec, never a workstation command. Every
//! request that targets a window or pane is refused unless that window
//! belongs to a project on the requesting remote, which workmux records by
//! tagging the windows it creates for remote projects.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::PathBuf;

use super::forwarding::{MuxRequest, RemoteTty};
use super::transport::{Transport, TtyExec};
use crate::multiplexer::types::{
    CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams,
};
use crate::multiplexer::{InvocationContext, Multiplexer, TmuxBackend};

/// Session option naming the project a workstation session holds.
pub const SESSION_TAG: &str = "@workmux_project";

/// Window option naming the project a workstation window belongs to.
pub const WINDOW_TAG: &str = "@workmux_project_window";

/// What the workstation knows about a command session that forwards
/// multiplexer requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScope {
    pub remote: String,
    pub project: String,
    pub invocation: InvocationContext,
}

impl SessionScope {
    /// The tag workmux puts on the project's sessions and windows.
    pub fn tag(&self) -> String {
        project_tag(&self.remote, &self.project)
    }

    /// The workstation's tmux, answering context questions from the
    /// session's invocation context.
    pub fn tmux(&self) -> TmuxBackend {
        let socket = self.invocation.tmux_socket_path().unwrap_or("default");
        TmuxBackend::for_socket(socket).with_invocation_context(self.invocation.clone())
    }
}

/// The tag of project `name` on `remote`.
pub fn project_tag(remote: &str, name: &str) -> String {
    format!("{remote}:{name}")
}

/// The remote and project name in a tag.
pub fn parse_project_tag(tag: &str) -> Option<(&str, &str)> {
    tag.trim()
        .split_once(':')
        .filter(|(remote, name)| !remote.is_empty() && !name.is_empty())
}

/// Answer `request` from `scope`'s remote, against `tmux`, running pane
/// terminals through `transport`.
pub fn answer(
    scope: &SessionScope,
    tmux: &TmuxBackend,
    transport: &dyn Transport,
    request: MuxRequest,
) -> Result<Value> {
    let workstation = Workstation {
        scope,
        tmux,
        transport,
    };
    workstation.answer(request)
}

struct Workstation<'a> {
    scope: &'a SessionScope,
    tmux: &'a TmuxBackend,
    transport: &'a dyn Transport,
}

impl Workstation<'_> {
    fn answer(&self, request: MuxRequest) -> Result<Value> {
        let tmux = self.tmux;
        Ok(match request {
            MuxRequest::IsRunning => json!(tmux.is_running()?),
            MuxRequest::InstanceId => json!(tmux.resolve_instance_id()?),
            MuxRequest::CurrentPaneId => json!(tmux.current_pane_id()),
            MuxRequest::CurrentWindowId => json!(tmux.current_window_id()?),
            MuxRequest::RightmostWindowId => json!(tmux.rightmost_window_id()?),
            MuxRequest::CurrentSessionId => json!(tmux.current_session_id()?),
            MuxRequest::CurrentWindowName => json!(tmux.current_window_name()?),
            MuxRequest::CurrentSession => json!(tmux.current_session()),
            MuxRequest::ClientSession => json!(tmux.client_session()),
            MuxRequest::SessionExists { name } => json!(tmux.session_exists(&name)?),
            MuxRequest::AllWindowNames => json!(tmux.get_all_window_names()?),
            MuxRequest::WindowNamesInSession { session } => {
                json!(tmux.get_window_names_in_session(&session)?)
            }
            MuxRequest::AllWindowsWithSessions => json!(tmux.get_all_windows_with_sessions()?),
            MuxRequest::AllSessionNames => json!(tmux.get_all_session_names()?),
            MuxRequest::CreateWindow {
                name,
                after_window,
                tty,
            } => {
                if let Some(after) = after_window.as_deref()
                    && tmux.current_window_id()?.as_deref() != Some(after)
                {
                    self.check_window(after)?;
                }
                let (prefix, name) = ("", name.as_str());
                let pane_id = tmux.create_window(CreateWindowParams {
                    prefix,
                    name,
                    cwd: &workstation_dir(),
                    after_window: after_window.as_deref(),
                })?;
                json!(self.claim_window(&pane_id, &tty)?)
            }
            MuxRequest::CreateSession {
                name,
                initial_window_name,
                tty,
            } => {
                let pane_id = tmux.create_session(CreateSessionParams {
                    prefix: "",
                    name: &name,
                    cwd: &workstation_dir(),
                    initial_window_name: initial_window_name.as_deref(),
                })?;
                self.tag_session(&name)?;
                json!(self.claim_window(&pane_id, &tty)?)
            }
            MuxRequest::CreateWindowInSession { session, name, tty } => {
                self.adopt_session(&session)?;
                let pane_id = tmux.create_window_in_session(CreateWindowInSessionParams {
                    session_name: &session,
                    name: name.as_deref(),
                    cwd: &workstation_dir(),
                })?;
                json!(self.claim_window(&pane_id, &tty)?)
            }
            MuxRequest::RespawnPane { pane_id, tty } => {
                self.check_pane(&pane_id)?;
                json!(self.run_tty(&pane_id, &tty)?)
            }
            MuxRequest::SplitPane {
                target_pane_id,
                direction,
                size,
                percentage,
                tty,
            } => {
                self.check_pane(&target_pane_id)?;
                let (dir, line) = self.tty_command_line(&tty)?;
                json!(tmux.split_pane(
                    &target_pane_id,
                    &direction,
                    &dir,
                    size,
                    percentage,
                    Some(&line)
                )?)
            }
            MuxRequest::SetWindowOwnership {
                pane_id,
                token,
                is_primary,
            } => {
                self.check_pane(&pane_id)?;
                json!(tmux.set_window_ownership(&pane_id, &token, is_primary)?)
            }
            MuxRequest::SetPaneName { pane_id, name } => {
                self.check_pane(&pane_id)?;
                json!(tmux.set_pane_name(&pane_id, &name)?)
            }
            MuxRequest::SelectPane { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.select_pane(&pane_id)?)
            }
            MuxRequest::ZoomPane { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.zoom_pane(&pane_id)?)
            }
            MuxRequest::SwitchToPane { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.switch_to_pane(&pane_id, None)?)
            }
            MuxRequest::SelectWindow { name } => {
                let window = self.window_named(&name)?;
                tmux.query(&["select-window", "-t", &window])?;
                Value::Null
            }
            MuxRequest::KillWindow { name } => {
                let window = self.window_named(&name)?;
                tmux.query(&["kill-window", "-t", &window])?;
                Value::Null
            }
            MuxRequest::SwitchToSession { name } => {
                self.check_session(&name)?;
                json!(tmux.switch_to_session("", &name)?)
            }
            MuxRequest::KillSession { name } => {
                self.check_session(&name)?;
                json!(tmux.kill_session(&name)?)
            }
            MuxRequest::SendText { pane_id, text } => {
                self.check_pane(&pane_id)?;
                json!(tmux.send_text_fragment(&pane_id, &text)?)
            }
            MuxRequest::SendEnter { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.send_enter(&pane_id)?)
            }
            MuxRequest::SetStatus {
                pane_id,
                icon,
                auto_clear_on_focus,
            } => {
                self.check_pane(&pane_id)?;
                json!(tmux.set_status(&pane_id, &icon, auto_clear_on_focus)?)
            }
            MuxRequest::ClearStatus { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.clear_status(&pane_id)?)
            }
            MuxRequest::EnsureStatusFormat { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.ensure_status_format(&pane_id)?)
            }
            MuxRequest::KillPane { pane_id } => {
                self.check_pane(&pane_id)?;
                json!(tmux.kill_pane(&pane_id)?)
            }
        })
    }

    fn refusal(&self, what: &str) -> anyhow::Error {
        anyhow!(
            "refused: {what} does not belong to a project on remote {}",
            self.scope.remote
        )
    }

    fn belongs(&self, tag: &str) -> bool {
        parse_project_tag(tag).is_some_and(|(remote, _)| remote == self.scope.remote)
    }

    /// Refuse unless the window holding `target` (a pane or window) was
    /// created for a project on this remote.
    fn check_window(&self, target: &str) -> Result<()> {
        let tag = self
            .tmux
            .query(&["show-options", "-wqv", "-t", target, WINDOW_TAG])
            .unwrap_or_default();
        if self.belongs(&tag) {
            Ok(())
        } else {
            Err(self.refusal(&format!("window of {target}")))
        }
    }

    fn check_pane(&self, pane_id: &str) -> Result<()> {
        self.check_window(pane_id)
            .map_err(|_| self.refusal(&format!("pane {pane_id}")))
    }

    fn session_tag(&self, session: &str) -> String {
        self.tmux
            .query(&["show-options", "-qv", "-t", session, SESSION_TAG])
            .unwrap_or_default()
    }

    fn check_session(&self, session: &str) -> Result<()> {
        if self.belongs(&self.session_tag(session)) {
            Ok(())
        } else {
            Err(self.refusal(&format!("session {session}")))
        }
    }

    /// Use `session` for the project: an untagged session of the project's
    /// name is adopted and tagged; any other must already be this remote's.
    fn adopt_session(&self, session: &str) -> Result<()> {
        let tag = self.session_tag(session);
        if tag.trim().is_empty() {
            return self.tag_session(session);
        }
        if self.belongs(&tag) {
            Ok(())
        } else {
            Err(self.refusal(&format!("session {session}")))
        }
    }

    fn tag_session(&self, session: &str) -> Result<()> {
        self.tmux.query(&[
            "set-option",
            "-q",
            "-t",
            session,
            SESSION_TAG,
            &self.scope.tag(),
        ])?;
        Ok(())
    }

    /// Tag the new window holding `pane_id` and start the remote's
    /// terminal in it.
    fn claim_window(&self, pane_id: &str, tty: &RemoteTty) -> Result<String> {
        self.tmux.query(&[
            "set-option",
            "-wq",
            "-t",
            pane_id,
            WINDOW_TAG,
            &self.scope.tag(),
        ])?;
        self.run_tty(pane_id, tty)
    }

    fn run_tty(&self, pane_id: &str, tty: &RemoteTty) -> Result<String> {
        let (dir, line) = self.tty_command_line(tty)?;
        self.tmux.respawn_pane(pane_id, &dir, Some(&line))
    }

    /// The window id of this remote's window named `name`.
    fn window_named(&self, name: &str) -> Result<String> {
        let windows = self.tmux.query(&[
            "list-windows",
            "-a",
            "-F",
            &format!("#{{window_id}}\t#{{window_name}}\t#{{{WINDOW_TAG}}}"),
        ])?;
        windows
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, '\t');
                Some((fields.next()?, fields.next()?, fields.next()?))
            })
            .find(|(_, window_name, tag)| *window_name == name && self.belongs(tag))
            .map(|(id, _, _)| id.to_string())
            .ok_or_else(|| self.refusal(&format!("window {name}")))
    }

    /// The workstation directory and shell command line that run `tty` on
    /// the remote.
    fn tty_command_line(&self, tty: &RemoteTty) -> Result<(PathBuf, String)> {
        if tty.argv.is_empty() {
            bail!("no command for the remote terminal");
        }
        let command = self.transport.tty_command(&TtyExec {
            argv: tty.argv.clone(),
            workdir: Some(tty.workdir.clone()),
            env: Vec::new(),
        })?;
        Ok((
            command
                .get_current_dir()
                .map(PathBuf::from)
                .unwrap_or_else(workstation_dir),
            command_line(&command),
        ))
    }
}

/// Where workstation panes for remote terminals start: they never use the
/// remote's paths, which need not exist here.
fn workstation_dir() -> PathBuf {
    home::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `command` as a POSIX shell command line: its environment changes, then
/// its program and arguments.
fn command_line(command: &std::process::Command) -> String {
    use crate::shell::shell_quote;
    let mut unset = Vec::new();
    let mut set = Vec::new();
    for (name, value) in command.get_envs() {
        let name = name.to_string_lossy();
        match value {
            Some(value) => set.push(shell_quote(&format!("{name}={}", value.to_string_lossy()))),
            None => unset.push(format!("-u {}", shell_quote(&name))),
        }
    }
    let mut parts = vec!["exec".to_string(), "env".to_string()];
    parts.extend(unset);
    parts.extend(set);
    parts.push(shell_quote(&command.get_program().to_string_lossy()));
    parts.extend(
        command
            .get_args()
            .map(|arg| shell_quote(&arg.to_string_lossy())),
    );
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::transport::Loopback;
    use crate::test_support::PrivateTmuxServer;
    use std::time::{Duration, Instant};

    struct Workstation {
        server: PrivateTmuxServer,
        remote_root: tempfile::TempDir,
        transport: Loopback,
    }

    impl Workstation {
        fn start() -> Option<Self> {
            let server = PrivateTmuxServer::start()?;
            let remote_root = tempfile::tempdir().unwrap();
            let transport = Loopback::new(remote_root.path().canonicalize().unwrap()).unwrap();
            Some(Self {
                server,
                remote_root,
                transport,
            })
        }

        /// The scope of a command session for project `name` on `remote`,
        /// invoked from `pane`.
        fn scope(&self, remote: &str, name: &str, pane: &str) -> SessionScope {
            SessionScope {
                remote: remote.to_string(),
                project: name.to_string(),
                invocation: InvocationContext {
                    pane_id: Some(pane.to_string()),
                    tmux: Some(format!("{},1,0", self.server.socket)),
                    cwd: None,
                },
            }
        }

        fn ask(&self, scope: &SessionScope, request: MuxRequest) -> Result<Value> {
            answer(scope, &scope.tmux(), &self.transport, request)
        }

        fn tty(&self, script: &str) -> RemoteTty {
            RemoteTty {
                argv: ["sh", "-c", script].map(String::from).to_vec(),
                workdir: self.remote_root.path().display().to_string(),
            }
        }

        fn tmux(&self, args: &[&str]) -> String {
            self.server.query(args)
        }
    }

    fn pane(value: Value) -> String {
        value.as_str().expect("a pane id").to_string()
    }

    fn wait_for_file(path: &std::path::Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            assert!(Instant::now() < deadline, "{} never written", path.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_new_project_session_is_tagged_and_its_window_runs_the_remote_terminal() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        let scope = ws.scope("devbox", "api", &invoking);

        let pane_id = pane(
            ws.ask(
                &scope,
                MuxRequest::CreateSession {
                    name: "api".to_string(),
                    initial_window_name: Some("wm-feature".to_string()),
                    tty: ws.tty("pwd -P > ran; exec sleep 30"),
                },
            )
            .unwrap(),
        );

        assert_eq!(
            ws.tmux(&["display-message", "-p", "-t", &pane_id, "#{session_name}:#{window_name}"]),
            "api:wm-feature"
        );
        assert_eq!(
            ws.tmux(&["show-options", "-qv", "-t", "api", SESSION_TAG]),
            "devbox:api"
        );
        assert_eq!(
            ws.tmux(&["show-options", "-wqv", "-t", &pane_id, WINDOW_TAG]),
            "devbox:api"
        );
        let ran = wait_for_file(&ws.remote_root.path().join("ran"));
        assert_eq!(
            ran.trim(),
            ws.remote_root.path().canonicalize().unwrap().display().to_string()
        );
    }

    #[test]
    fn an_existing_session_is_adopted_and_tagged() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        ws.server.new_session("api", ws.remote_root.path());
        let scope = ws.scope("devbox", "api", &invoking);

        let pane_id = pane(
            ws.ask(
                &scope,
                MuxRequest::CreateWindowInSession {
                    session: "api".to_string(),
                    name: Some("wm-feature".to_string()),
                    tty: ws.tty("exec sleep 30"),
                },
            )
            .unwrap(),
        );

        assert_eq!(
            ws.tmux(&["display-message", "-p", "-t", &pane_id, "#{session_name}"]),
            "api"
        );
        assert_eq!(
            ws.tmux(&["show-options", "-qv", "-t", "api", SESSION_TAG]),
            "devbox:api"
        );
    }

    #[test]
    fn requests_targeting_another_remotes_window_are_refused() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        let other = ws.scope("other", "web", &invoking);
        let others_pane = pane(
            ws.ask(
                &other,
                MuxRequest::CreateSession {
                    name: "web".to_string(),
                    initial_window_name: Some("wm-x".to_string()),
                    tty: ws.tty("exec sleep 30"),
                },
            )
            .unwrap(),
        );
        let scope = ws.scope("devbox", "api", &invoking);

        let refusals = [
            MuxRequest::SendText {
                pane_id: others_pane.clone(),
                text: "rm -rf ~".to_string(),
            },
            MuxRequest::RespawnPane {
                pane_id: others_pane.clone(),
                tty: ws.tty("true"),
            },
            MuxRequest::KillPane {
                pane_id: others_pane.clone(),
            },
            MuxRequest::KillWindow {
                name: "wm-x".to_string(),
            },
            MuxRequest::KillSession {
                name: "web".to_string(),
            },
            MuxRequest::CreateWindowInSession {
                session: "web".to_string(),
                name: None,
                tty: ws.tty("true"),
            },
            // The invoking user's own pane belongs to no project.
            MuxRequest::SendText {
                pane_id: invoking.clone(),
                text: "x".to_string(),
            },
        ];
        for request in refusals {
            let error = ws.ask(&scope, request.clone()).unwrap_err().to_string();
            assert!(
                error.contains("does not belong to a project on remote devbox"),
                "{request:?}: {error}"
            );
        }
        assert_eq!(
            ws.tmux(&["display-message", "-p", "-t", &others_pane, "#{pane_id}"]),
            others_pane
        );
    }

    #[test]
    fn context_questions_are_answered_from_the_invocation_context() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        ws.server.new_session("first", ws.remote_root.path());
        let invoking = ws.server.new_session("second", ws.remote_root.path());
        let scope = ws.scope("devbox", "api", &invoking);

        assert_eq!(
            ws.ask(&scope, MuxRequest::CurrentPaneId).unwrap(),
            json!(invoking)
        );
        assert_eq!(
            ws.ask(&scope, MuxRequest::CurrentSession).unwrap(),
            json!("second")
        );
    }
}
