//! The workstation's side of the forwarding multiplexer: answers the
//! multiplexer requests a remote forwards for one command session.
//!
//! Requests act on the workstation's tmux. Context questions are answered
//! from the session's invocation context. Panes only ever run
//! `workmux remote attach`, a view onto a remote pane the remote started. Every
//! request that targets a window or pane is refused unless that window
//! belongs to the requesting project on the requesting remote, which workmux
//! records by tagging the windows it creates for remote projects.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::forwarding::MuxRequest;
use super::pane::RemotePaneId;
use crate::multiplexer::types::{
    CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams,
};
use crate::multiplexer::{InvocationContext, Multiplexer, TmuxBackend};
use crate::project::RemoteProjectId;

/// Session option naming the project a workstation session holds.
pub const SESSION_TAG: &str = "@workmux_project";

/// Window option naming the project a workstation window belongs to.
pub const WINDOW_TAG: &str = "@workmux_project_window";

/// Pane option naming the remote pane a workstation pane shows.
pub const REMOTE_PANE_TAG: &str = "@workmux_remote_pane";
const REMOTE_PANE_FORMAT: &str = "#{@workmux_remote_pane}";

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
        self.id().tag()
    }

    /// The project the session runs for.
    pub fn id(&self) -> RemoteProjectId {
        RemoteProjectId::new(&self.remote, &self.project)
    }

    /// The workstation's tmux, answering context questions from the
    /// session's invocation context.
    pub fn tmux(&self) -> TmuxBackend {
        let socket = self.invocation.tmux_socket_path().unwrap_or("default");
        TmuxBackend::for_socket(socket).with_invocation_context(self.invocation.clone())
    }
}

/// Close every window of project `name` on `remote` in `tmux`, returning
/// how many. A session left holding other windows loses the project's tag,
/// so it is no longer taken for the project's.
pub fn close_project_windows(tmux: &TmuxBackend, remote: &str, name: &str) -> Result<usize> {
    let tag = RemoteProjectId::new(remote, name).tag();
    let windows = tmux.query(&[
        "list-windows",
        "-a",
        "-F",
        &format!("#{{window_id}}\t#{{{WINDOW_TAG}}}"),
    ])?;
    let mut closed = 0;
    for (window, _) in windows
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, window_tag)| window_tag.trim() == tag)
    {
        tmux.query(&["kill-window", "-t", window])?;
        closed += 1;
    }
    let sessions = tmux
        .query(&[
            "list-sessions",
            "-F",
            &format!("#{{session_id}}\t#{{{SESSION_TAG}}}"),
        ])
        .unwrap_or_default();
    for (session, _) in sessions
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, session_tag)| session_tag.trim() == tag)
    {
        tmux.query(&["set-option", "-qu", "-t", session, SESSION_TAG])?;
    }
    Ok(closed)
}

/// Answer `request` from `scope`'s remote, against `tmux`. Panes attach to
/// remote panes by running `workmux`'s `remote attach`.
pub fn answer(
    scope: &SessionScope,
    tmux: &TmuxBackend,
    workmux: &Path,
    request: MuxRequest,
) -> Result<Value> {
    let workstation = Workstation {
        scope,
        tmux,
        workmux,
    };
    workstation.answer(request)
}

struct Workstation<'a> {
    scope: &'a SessionScope,
    tmux: &'a TmuxBackend,
    workmux: &'a Path,
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
            // "The current session" is the one the command was typed in, not
            // whichever session tmux picks for the client-less daemon.
            MuxRequest::AllWindowNames => match tmux.current_session_id()? {
                Some(session) => json!(tmux.get_window_names_in_session(&session)?),
                None => json!(tmux.get_all_window_names()?),
            },
            MuxRequest::WindowNamesInSession { session } => {
                json!(tmux.get_window_names_in_session(&session)?)
            }
            MuxRequest::AllWindowsWithSessions => json!(tmux.get_all_windows_with_sessions()?),
            MuxRequest::AllSessionNames => json!(tmux.get_all_session_names()?),
            MuxRequest::CreateWindow {
                name,
                after_window,
                pane,
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
                json!(self.claim_window(&pane_id, &pane)?)
            }
            MuxRequest::CreateSession {
                name,
                initial_window_name,
                pane,
            } => {
                let pane_id = tmux.create_session(CreateSessionParams {
                    prefix: "",
                    name: &name,
                    cwd: &workstation_dir(),
                    initial_window_name: initial_window_name.as_deref(),
                })?;
                self.tag_session(&name)?;
                json!(self.claim_window(&pane_id, &pane)?)
            }
            MuxRequest::CreateWindowInSession {
                session,
                name,
                pane,
            } => {
                self.adopt_session(&session)?;
                let pane_id = tmux.create_window_in_session(CreateWindowInSessionParams {
                    session_name: &session,
                    name: name.as_deref(),
                    cwd: &workstation_dir(),
                })?;
                json!(self.claim_window(&pane_id, &pane)?)
            }
            MuxRequest::RespawnPane { pane_id, pane } => {
                self.check_pane(&pane_id)?;
                json!(self.attach(&pane_id, &pane)?)
            }
            MuxRequest::SplitPane {
                target_pane_id,
                direction,
                size,
                percentage,
                pane,
            } => {
                self.check_pane(&target_pane_id)?;
                let pane_id = tmux.split_pane(
                    &target_pane_id,
                    &direction,
                    &workstation_dir(),
                    size,
                    percentage,
                    Some(&self.attach_line(&pane)),
                )?;
                self.tag_remote_pane(&pane_id, &pane)?;
                json!(pane_id)
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
                self.switch_invoking_clients(&pane_id)?;
                Value::Null
            }
            MuxRequest::SelectWindow { name } => {
                let window = self.window_named(&name)?;
                tmux.query(&["select-window", "-t", &window])?;
                Value::Null
            }
            MuxRequest::KillWindow { name } => {
                let window = self.window_named(&name)?;
                let ended =
                    self.remote_panes(&["list-panes", "-t", &window, "-F", REMOTE_PANE_FORMAT])?;
                tmux.query(&["kill-window", "-t", &window])?;
                json!(ended)
            }
            MuxRequest::OwnedWindowTargets { token } => {
                let owned = tmux.owned_window_targets(&token)?;
                json!(
                    owned
                        .into_iter()
                        .filter(|owned| owned
                            .target
                            .window_id
                            .as_deref()
                            .is_some_and(|id| self.check_window(id).is_ok()))
                        .collect::<Vec<_>>()
                )
            }
            MuxRequest::KillWindowTarget { target } => {
                let window = match &target.window_id {
                    Some(id) => {
                        self.check_window(id)?;
                        id.clone()
                    }
                    None => self.window_named(&target.full_name)?,
                };
                let ended =
                    self.remote_panes(&["list-panes", "-t", &window, "-F", REMOTE_PANE_FORMAT])?;
                tmux.query(&["kill-window", "-t", &window])?;
                json!(ended)
            }
            MuxRequest::WindowTargetExists { target } => json!(match &target.window_id {
                // Another remote's windows are not even reported.
                Some(id) => tmux.window_target_exists(&target)? && self.check_window(id).is_ok(),
                None => self.window_named(&target.full_name).is_ok(),
            }),
            MuxRequest::RenameWindow { old, new } => {
                let window = self.window_named(&old)?;
                tmux.query(&["rename-window", "-t", &window, &new])?;
                Value::Null
            }
            MuxRequest::RenameSession { old, new } => {
                self.check_session(&old)?;
                json!(tmux.rename_session(&old, &new)?)
            }
            MuxRequest::SwitchToSession { name } => {
                self.check_session(&name)?;
                self.switch_invoking_clients(&name)?;
                Value::Null
            }
            MuxRequest::KillSession { name } => {
                self.check_session(&name)?;
                let ended = self.remote_panes(&[
                    "list-panes",
                    "-s",
                    "-t",
                    &name,
                    "-F",
                    REMOTE_PANE_FORMAT,
                ])?;
                tmux.kill_session(&name)?;
                json!(ended)
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
                let ended = self.remote_panes(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane_id,
                    REMOTE_PANE_FORMAT,
                ])?;
                tmux.kill_pane(&pane_id)?;
                json!(ended)
            }
            MuxRequest::ShownRemotePanes => {
                let tag = self.scope.tag();
                let panes = tmux.query(&[
                    "list-panes",
                    "-a",
                    "-F",
                    &format!("#{{{WINDOW_TAG}}}\t{REMOTE_PANE_FORMAT}"),
                ])?;
                json!(
                    panes
                        .lines()
                        .filter_map(|line| line.split_once('\t'))
                        .filter(|(window, _)| *window == tag)
                        .filter_map(|(_, pane)| pane.parse::<RemotePaneId>().ok())
                        .collect::<Vec<_>>()
                )
            }
        })
    }

    /// Switch the tmux clients viewing the invoking session to `target`. The
    /// workstation daemon has no client of its own; with no client viewing
    /// the invoking session there is nothing to switch.
    fn switch_invoking_clients(&self, target: &str) -> Result<()> {
        let Some(session) = self.tmux.current_session_id()? else {
            return Ok(());
        };
        let clients = self
            .tmux
            .query(&["list-clients", "-F", "#{client_name}\t#{session_id}"])?;
        for client in clients.lines().filter_map(|line| {
            let (client, client_session) = line.split_once('\t')?;
            (client_session == session).then_some(client)
        }) {
            self.tmux
                .query(&["switch-client", "-c", client, "-t", target])?;
        }
        Ok(())
    }

    fn refusal(&self, what: &str) -> anyhow::Error {
        anyhow!(
            "refused: {what} does not belong to project {} on remote {}",
            self.scope.project,
            self.scope.remote
        )
    }

    /// Whether `tag` names the session's own project: another project on
    /// the same remote may have windows of the same names.
    fn belongs(&self, tag: &str) -> bool {
        RemoteProjectId::from_tag(tag).is_some_and(|id| id == self.scope.id())
    }

    /// Refuse unless the window holding `target` (a pane or window) was
    /// created for this project.
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
    /// name is adopted and tagged; any other must already be this project's.
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

    /// Tag the new window holding `pane_id` and attach it to remote pane
    /// `pane`.
    fn claim_window(&self, pane_id: &str, pane: &RemotePaneId) -> Result<String> {
        self.tmux.query(&[
            "set-option",
            "-wq",
            "-t",
            pane_id,
            WINDOW_TAG,
            &self.scope.tag(),
        ])?;
        self.attach(pane_id, pane)
    }

    fn attach(&self, pane_id: &str, pane: &RemotePaneId) -> Result<String> {
        let pane_id =
            self.tmux
                .respawn_pane(pane_id, &workstation_dir(), Some(&self.attach_line(pane)))?;
        self.tag_remote_pane(&pane_id, pane)?;
        Ok(pane_id)
    }

    fn tag_remote_pane(&self, pane_id: &str, pane: &RemotePaneId) -> Result<()> {
        self.tmux.query(&[
            "set-option",
            "-pq",
            "-t",
            pane_id,
            REMOTE_PANE_TAG,
            &pane.to_string(),
        ])?;
        Ok(())
    }

    /// The remote panes shown in the panes `query` prints
    /// [`REMOTE_PANE_FORMAT`] for: what a kill leaves for the remote to end.
    fn remote_panes(&self, query: &[&str]) -> Result<Vec<RemotePaneId>> {
        Ok(self
            .tmux
            .query(query)?
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect())
    }

    /// The shell command line a workstation pane runs to show `pane`.
    fn attach_line(&self, pane: &RemotePaneId) -> String {
        use crate::shell::shell_quote;
        format!(
            "exec {} remote attach {} {}",
            shell_quote(&self.workmux.to_string_lossy()),
            shell_quote(&self.scope.remote),
            shell_quote(&pane.to_string())
        )
    }

    /// The window id of this project's window named `name`.
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
}

/// Where workstation panes for remote terminals start: they never use the
/// remote's paths, which need not exist here.
fn workstation_dir() -> PathBuf {
    home::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::types::{OwnedWindowTarget, WindowTarget};
    use crate::test_support::PrivateTmuxServer;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    struct Workstation {
        server: PrivateTmuxServer,
        remote_root: tempfile::TempDir,
        /// Stands in for workstation workmux: records its arguments in
        /// `attached`, then stays attached.
        workmux: PathBuf,
    }

    impl Workstation {
        fn start() -> Option<Self> {
            let server = PrivateTmuxServer::start()?;
            let remote_root = tempfile::tempdir().unwrap();
            let workmux = remote_root.path().join("workmux");
            std::fs::write(
                &workmux,
                format!(
                    "#!/bin/sh\necho \"$@\" > '{}'\nexec sleep 30\n",
                    remote_root.path().join("attached").display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&workmux, std::fs::Permissions::from_mode(0o755)).unwrap();
            Some(Self {
                server,
                remote_root,
                workmux,
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
            answer(scope, &scope.tmux(), &self.workmux, request)
        }

        fn tmux(&self, args: &[&str]) -> String {
            self.server.query(args)
        }

        fn server_tmux(&self) -> TmuxBackend {
            TmuxBackend::for_socket(&self.server.socket)
        }
    }

    fn remote_pane() -> RemotePaneId {
        "api/feature/0".parse().unwrap()
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
            assert!(
                Instant::now() < deadline,
                "{} never written",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_new_project_session_is_tagged_and_its_window_attaches_to_the_remote_pane() {
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
                    pane: remote_pane(),
                },
            )
            .unwrap(),
        );

        assert_eq!(
            ws.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane_id,
                "#{session_name}:#{window_name}"
            ]),
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
        let attached = wait_for_file(&ws.remote_root.path().join("attached"));
        assert_eq!(attached.trim(), "remote attach devbox api/feature/0");
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
                    pane: remote_pane(),
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
                    pane: remote_pane(),
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
                pane: remote_pane(),
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
                pane: remote_pane(),
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
                error.contains("does not belong to project api on remote devbox"),
                "{request:?}: {error}"
            );
        }
        assert_eq!(
            ws.tmux(&["display-message", "-p", "-t", &others_pane, "#{pane_id}"]),
            others_pane
        );
    }

    #[test]
    fn a_project_acts_only_on_its_own_windows_when_another_on_its_remote_shares_a_name() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        let window_of = |project: &str| {
            let scope = ws.scope("devbox", project, &invoking);
            let pane_id = pane(
                ws.ask(
                    &scope,
                    MuxRequest::CreateSession {
                        name: project.to_string(),
                        initial_window_name: Some("wm-feature".to_string()),
                        pane: remote_pane(),
                    },
                )
                .unwrap(),
            );
            let window = ws.tmux(&["display-message", "-p", "-t", &pane_id, "#{window_id}"]);
            (pane_id, window)
        };
        // The other project's window comes first in tmux's listing.
        let (web_pane, web_window) = window_of("web");
        let (_, api_window) = window_of("api");
        let api = ws.scope("devbox", "api", &invoking);

        ws.ask(
            &api,
            MuxRequest::RenameWindow {
                old: "wm-feature".to_string(),
                new: "wm-renamed".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            ws.tmux(&["display-message", "-p", "-t", &web_window, "#{window_name}"]),
            "wm-feature"
        );
        ws.ask(
            &api,
            MuxRequest::KillWindow {
                name: "wm-renamed".to_string(),
            },
        )
        .unwrap();
        let error = ws
            .ask(
                &api,
                MuxRequest::KillWindow {
                    name: "wm-feature".to_string(),
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("refused"), "{error}");
        for request in [
            MuxRequest::KillPane {
                pane_id: web_pane.clone(),
            },
            MuxRequest::KillSession {
                name: "web".to_string(),
            },
        ] {
            let error = ws.ask(&api, request.clone()).unwrap_err().to_string();
            assert!(
                error.contains("does not belong to project api on remote devbox"),
                "{request:?}: {error}"
            );
        }
        let windows = ws.tmux(&["list-windows", "-a", "-F", "#{window_id}"]);
        assert!(!windows.lines().any(|id| id == api_window), "{windows}");
        assert!(windows.lines().any(|id| id == web_window), "{windows}");
    }

    #[test]
    fn closing_a_projects_windows_leaves_every_other_window() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        let create = |project: &str, session: &str| {
            pane(
                ws.ask(
                    &ws.scope("devbox", project, &invoking),
                    MuxRequest::CreateSession {
                        name: session.to_string(),
                        initial_window_name: Some("wm-feature".to_string()),
                        pane: remote_pane(),
                    },
                )
                .unwrap(),
            )
        };
        create("api", "api");
        let web = create("web", "web");
        // The user's own window in the project's session.
        let own = ws.server.new_window("api");

        let closed = close_project_windows(&ws.server_tmux(), "devbox", "api").unwrap();

        assert_eq!(closed, 1);
        let windows = ws.tmux(&["list-windows", "-a", "-F", "#{pane_id}"]);
        assert!(windows.lines().any(|pane| pane == web), "{windows}");
        let window_ids = ws.tmux(&["list-windows", "-a", "-F", "#{window_id}"]);
        assert!(window_ids.lines().any(|id| id == own), "{window_ids}");
        assert!(windows.lines().any(|pane| pane == invoking), "{windows}");
        assert_eq!(
            ws.tmux(&["show-options", "-qv", "-t", "api", SESSION_TAG]),
            ""
        );
    }

    #[test]
    fn a_remote_finds_and_kills_only_its_own_owned_windows() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        let owned_window = |remote: &str, session: &str| {
            let scope = ws.scope(remote, session, &invoking);
            let pane_id = pane(
                ws.ask(
                    &scope,
                    MuxRequest::CreateSession {
                        name: session.to_string(),
                        initial_window_name: Some("wm-feature".to_string()),
                        pane: remote_pane(),
                    },
                )
                .unwrap(),
            );
            ws.ask(
                &scope,
                MuxRequest::SetWindowOwnership {
                    pane_id: pane_id.clone(),
                    token: "token".to_string(),
                    is_primary: true,
                },
            )
            .unwrap();
            ws.tmux(&["display-message", "-p", "-t", &pane_id, "#{window_id}"])
        };
        let others = owned_window("other", "web");
        let ours = owned_window("devbox", "api");
        let scope = ws.scope("devbox", "api", &invoking);

        let owned: Vec<OwnedWindowTarget> = serde_json::from_value(
            ws.ask(
                &scope,
                MuxRequest::OwnedWindowTargets {
                    token: "token".to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            owned,
            vec![OwnedWindowTarget {
                target: WindowTarget::with_id(
                    "wm-feature".to_string(),
                    Some("api".to_string()),
                    ours.clone()
                ),
                is_primary: true,
            }]
        );
        let exists = |target: &WindowTarget| {
            ws.ask(
                &scope,
                MuxRequest::WindowTargetExists {
                    target: target.clone(),
                },
            )
            .unwrap()
        };
        let others_target = WindowTarget::with_id(
            "wm-feature".to_string(),
            Some("web".to_string()),
            others.clone(),
        );
        assert_eq!(exists(&owned[0].target), json!(true));
        assert_eq!(exists(&others_target), json!(false));
        let error = ws
            .ask(
                &scope,
                MuxRequest::KillWindowTarget {
                    target: WindowTarget::with_id(
                        "wm-feature".to_string(),
                        Some("web".to_string()),
                        others.clone(),
                    ),
                },
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not belong to project api on remote devbox"),
            "{error}"
        );
        ws.ask(
            &scope,
            MuxRequest::KillWindowTarget {
                target: owned[0].target.clone(),
            },
        )
        .unwrap();
        assert_eq!(exists(&owned[0].target), json!(false));
        let windows = ws.tmux(&["list-windows", "-a", "-F", "#{window_id}"]);
        assert!(!windows.lines().any(|id| id == ours), "{windows}");
        assert!(windows.lines().any(|id| id == others), "{windows}");
    }

    #[test]
    fn a_projects_window_and_session_are_renamed_but_no_one_elses() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("mine", ws.remote_root.path());
        ws.tmux(&["rename-window", "-t", &invoking, "wm-old"]);
        let scope = ws.scope("devbox", "api", &invoking);
        let pane_id = pane(
            ws.ask(
                &scope,
                MuxRequest::CreateSession {
                    name: "wm-old-session".to_string(),
                    initial_window_name: Some("wm-old".to_string()),
                    pane: remote_pane(),
                },
            )
            .unwrap(),
        );
        let other = ws.scope("other", "web", &invoking);

        ws.ask(
            &scope,
            MuxRequest::RenameWindow {
                old: "wm-old".to_string(),
                new: "wm-new".to_string(),
            },
        )
        .unwrap();
        ws.ask(
            &scope,
            MuxRequest::RenameSession {
                old: "wm-old-session".to_string(),
                new: "wm-new-session".to_string(),
            },
        )
        .unwrap();
        let refused = [
            ws.ask(
                &other,
                MuxRequest::RenameWindow {
                    old: "wm-new".to_string(),
                    new: "x".to_string(),
                },
            ),
            ws.ask(
                &other,
                MuxRequest::RenameSession {
                    old: "wm-new-session".to_string(),
                    new: "x".to_string(),
                },
            ),
            ws.ask(
                &scope,
                MuxRequest::RenameSession {
                    old: "mine".to_string(),
                    new: "x".to_string(),
                },
            ),
        ];

        assert_eq!(
            ws.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane_id,
                "#{session_name}:#{window_name}"
            ]),
            "wm-new-session:wm-new"
        );
        // The user's own window of the old name is left alone.
        assert_eq!(
            ws.tmux(&["display-message", "-p", "-t", &invoking, "#{window_name}"]),
            "wm-old"
        );
        for refusal in refused {
            assert!(
                refusal
                    .unwrap_err()
                    .to_string()
                    .contains("does not belong to project"),
            );
        }
    }

    #[test]
    fn window_names_are_those_of_the_session_the_command_was_typed_in() {
        let Some(ws) = Workstation::start() else {
            return;
        };
        let invoking = ws.server.new_session("typed-in", ws.remote_root.path());
        ws.tmux(&["rename-window", "-t", &invoking, "wm-here"]);
        // Made last, this is the session a client-less tmux command picks.
        let elsewhere = ws.server.new_session("elsewhere", ws.remote_root.path());
        ws.tmux(&["rename-window", "-t", &elsewhere, "wm-there"]);
        let scope = ws.scope("devbox", "api", &invoking);

        assert_eq!(
            ws.ask(&scope, MuxRequest::AllWindowNames).unwrap(),
            json!(["wm-here"])
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
