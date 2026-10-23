//! Dashboard operations on a remote project: the git status, diffs and hunk
//! staging the dashboard shows for remote worktrees, and ending a remote
//! pane. The workstation asks over the control channel; remote workmux
//! answers from its own clone.

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::pane::{RemotePaneId, SessionHolder};
use super::router::RemoteProject;
use super::session::ProjectSite;

/// Method the workstation calls on remote workmux: `{project, request}`.
pub const METHOD: &str = "dashboard.request";

/// Method a dashboard calls on the workstation daemon:
/// `{remote, project, request, connect}`.
pub const DAEMON_METHOD: &str = "remote.dashboard";

/// How long one dashboard operation may take on the remote.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// One dashboard operation on a worktree or pane of a remote project.
/// Worktree paths are paths on the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DashboardRequest {
    /// The worktree's `GitStatus`.
    GitStatus { worktree: PathBuf },
    /// The raw text of `git diff <diff_arg>`, plus untracked files as new.
    Diff {
        worktree: PathBuf,
        diff_arg: String,
        include_untracked: bool,
    },
    /// The files `git diff <diff_arg>` touches, with line counts.
    FileList {
        worktree: PathBuf,
        diff_arg: String,
        include_untracked: bool,
    },
    /// Apply `patch` to the worktree's index; `reverse` unstages it.
    ApplyPatch {
        worktree: PathBuf,
        patch: String,
        reverse: bool,
    },
    /// End a remote pane of the project.
    KillPane { pane: RemotePaneId },
}

#[derive(Debug, Serialize, Deserialize)]
struct Params {
    project: ProjectSite,
    request: DashboardRequest,
}

/// The params of a `METHOD` request for `request` on `project`.
pub fn params(project: &ProjectSite, request: &DashboardRequest) -> Value {
    serde_json::json!({ "project": project, "request": request })
}

/// Remote workmux's side of `METHOD`.
pub fn serve(params: Value) -> Result<Value> {
    let Params { project, request } = serde_json::from_value(params)?;
    let clone = super::session::clone_path(&project.path)?;
    Ok(match request {
        DashboardRequest::GitStatus { worktree } => {
            let worktree = project_worktree(&project, &clone, &worktree)?;
            let main_branch = crate::config::Config::load_with_location_from(&worktree, None)
                .ok()
                .and_then(|(config, _)| config.main_branch);
            serde_json::to_value(crate::git::get_git_status(
                &worktree,
                main_branch.as_deref(),
            ))?
        }
        DashboardRequest::Diff {
            worktree,
            diff_arg,
            include_untracked,
        } => {
            let worktree = project_worktree(&project, &clone, &worktree)?;
            let diff =
                crate::command::dashboard::diff::raw_diff(&worktree, &diff_arg, include_untracked)
                    .map_err(|error| anyhow!(error))?;
            Value::String(diff)
        }
        DashboardRequest::FileList {
            worktree,
            diff_arg,
            include_untracked,
        } => {
            let worktree = project_worktree(&project, &clone, &worktree)?;
            serde_json::to_value(crate::command::dashboard::diff::get_file_list_numstat(
                &worktree,
                &diff_arg,
                include_untracked,
            ))?
        }
        DashboardRequest::ApplyPatch {
            worktree,
            patch,
            reverse,
        } => {
            let worktree = project_worktree(&project, &clone, &worktree)?;
            crate::command::dashboard::diff::apply_to_index(&worktree, &patch, reverse)
                .map_err(|error| anyhow!(error))?;
            Value::Null
        }
        DashboardRequest::KillPane { pane } => {
            if pane.project != project.name {
                bail!("remote pane {pane} is not one of project {}", project.name);
            }
            SessionHolder::for_this_remote()?.kill(&pane)?;
            Value::Null
        }
    })
}

/// `worktree`, when it is a worktree of the project cloned at `clone`.
fn project_worktree(project: &ProjectSite, clone: &Path, worktree: &Path) -> Result<PathBuf> {
    let key = |path: &Path| crate::git::get_repository_key_in(Some(path)).ok();
    match (key(clone), key(worktree)) {
        (Some(ours), Some(theirs)) if ours == theirs => Ok(worktree.to_path_buf()),
        _ => bail!(
            "{} is not a worktree of project {} on {}",
            worktree.display(),
            project.name,
            project.remote
        ),
    }
}

/// Run `request` on `project`'s remote and decode its answer: through the
/// workstation daemon, which holds the control channels, or directly when
/// no daemon can run. Without `connect`, only an open channel is used, so a
/// background refresh never dials a disconnected or stopped remote.
pub fn request<T: DeserializeOwned>(
    project: &RemoteProject,
    request: &DashboardRequest,
    connect: bool,
) -> Result<T> {
    let answer = match super::router::daemon_socket() {
        Some(socket) => crate::workstation_daemon::call_with_timeout(
            &socket,
            DAEMON_METHOD,
            serde_json::json!({
                "remote": project.site.remote,
                "project": project.site.name,
                "request": request,
                "connect": connect,
            }),
            TIMEOUT + Duration::from_secs(5),
        )?,
        None => super::channels().dashboard(
            &project.site.remote,
            &project.remote,
            &project.site,
            request,
            connect,
        )?,
    };
    Ok(serde_json::from_value(answer)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, TransportKind};
    use crate::remote::channels::{ChannelPolicy, Channels};
    use crate::remote::testing::ServeTransport;
    use crate::remote::transport::Transport;
    use crate::test_support::{init_repo, run_git, run_git_output};
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

    fn site(clone: &Path) -> ProjectSite {
        ProjectSite {
            remote: "devbox".to_string(),
            name: "api".to_string(),
            path: clone.display().to_string(),
            git_url: None,
            session: None,
        }
    }

    /// A clone with a linked worktree `feature` whose README has two
    /// unstaged changes far apart.
    fn project_with_feature() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let clone = temp.path().join("api");
        std::fs::create_dir(&clone).unwrap();
        init_repo(&clone);
        let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        std::fs::write(clone.join("README.md"), &lines).unwrap();
        run_git(&clone, &["commit", "-qam", "twenty lines"]);
        let worktree = temp.path().join("api__worktrees").join("feature");
        run_git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let changed = lines
            .replace("line 2\n", "line two\n")
            .replace("line 19\n", "line nineteen\n");
        std::fs::write(worktree.join("README.md"), changed).unwrap();
        (temp, clone, worktree)
    }

    fn ask<T: DeserializeOwned>(
        channels: &Channels,
        clone: &Path,
        request: DashboardRequest,
    ) -> Result<T> {
        let answer = channels.dashboard("devbox", &remote(), &site(clone), &request, true)?;
        Ok(serde_json::from_value(answer)?)
    }

    #[test]
    fn a_remote_worktrees_branch_state_and_diff_come_from_the_remote() {
        let (_temp, clone, worktree) = project_with_feature();
        let channels = channels();

        let status: crate::git::GitStatus = ask(
            &channels,
            &clone,
            DashboardRequest::GitStatus {
                worktree: worktree.clone(),
            },
        )
        .unwrap();
        let diff: String = ask(
            &channels,
            &clone,
            DashboardRequest::Diff {
                worktree: worktree.clone(),
                diff_arg: "HEAD".to_string(),
                include_untracked: true,
            },
        )
        .unwrap();

        assert_eq!(status.branch.as_deref(), Some("feature"));
        assert!(status.is_dirty);
        assert_eq!(status.uncommitted_added, 2);
        assert!(diff.contains("+line two\n"), "{diff}");
        assert!(diff.contains("+line nineteen\n"), "{diff}");
    }

    #[test]
    fn staging_a_hunk_changes_the_remote_index_and_undo_restores_it() {
        let (_temp, clone, worktree) = project_with_feature();
        let channels = channels();
        let diff: String = ask(
            &channels,
            &clone,
            DashboardRequest::Diff {
                worktree: worktree.clone(),
                diff_arg: String::new(),
                include_untracked: true,
            },
        )
        .unwrap();
        let hunks = crate::command::dashboard::diff::parse_diff_into_hunks(&diff);
        assert_eq!(hunks.len(), 2, "{diff}");
        let patch = |hunk: &crate::command::dashboard::diff::DiffHunk| {
            format!("{}\n{}\n", hunk.file_header, hunk.hunk_body)
        };
        let apply = |reverse| DashboardRequest::ApplyPatch {
            worktree: worktree.clone(),
            patch: patch(&hunks[0]),
            reverse,
        };

        ask::<Value>(&channels, &clone, apply(false)).unwrap();
        let staged = run_git_output(&worktree, &["diff", "--cached"]);
        ask::<Value>(&channels, &clone, apply(true)).unwrap();
        let unstaged = run_git_output(&worktree, &["diff", "--cached"]);

        assert!(staged.contains("+line two"), "{staged}");
        assert!(!staged.contains("nineteen"), "{staged}");
        assert_eq!(unstaged, "");
    }

    #[test]
    fn a_path_outside_the_project_is_refused() {
        let (_temp, clone, _worktree) = project_with_feature();
        let other = tempfile::tempdir().unwrap();
        init_repo(other.path());

        let error = ask::<Value>(
            &channels(),
            &clone,
            DashboardRequest::GitStatus {
                worktree: other.path().to_path_buf(),
            },
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("is not a worktree of project api on devbox"),
            "{error:#}"
        );
    }

    #[test]
    fn without_connect_a_remote_with_no_open_channel_is_not_dialled() {
        let (_temp, clone, worktree) = project_with_feature();
        let transport = Arc::new(ServeTransport::default());
        let channels = {
            let transport = transport.clone();
            Channels::new(
                Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
                ChannelPolicy::default(),
            )
        };
        let request = DashboardRequest::GitStatus { worktree };

        let error = channels
            .dashboard("devbox", &remote(), &site(&clone), &request, false)
            .unwrap_err();

        assert_eq!(error.to_string(), "remote devbox is not connected");
        assert_eq!(transport.exec_count(), 0);
    }
}
