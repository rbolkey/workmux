//! Where the dashboard reads and changes an agent's worktree: on the
//! workstation, or on a remote through remote workmux.
//!
//! A remote agent's path is a path on its remote. It may happen to exist on
//! the workstation too (a loopback remote, a same-named home directory), so
//! a remote worktree is always named by its remote and project as well, and
//! never read from the workstation's filesystem.

use std::path::{Path, PathBuf};

use crate::git::GitStatus;
use crate::multiplexer::AgentPane;
use crate::project::Project;
use crate::remote::dashboard::DashboardRequest;
use crate::remote::router::RemoteProject;

use super::diff::{self, FileEntry};

/// The branch a new worktree of the repository at `repo_path` starts from
/// by default: the configured base, else the current branch, else the
/// default branch.
pub fn default_add_worktree_base(repo_path: &Path) -> String {
    crate::config::Config::load_with_location_from(repo_path, None)
        .ok()
        .and_then(|(config, _)| {
            crate::workflow::resolve_configured_base_branch(&config, repo_path)
                .ok()
                .flatten()
        })
        .or_else(|| {
            crate::git::get_current_branch_in(repo_path)
                .ok()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty())
        })
        .or_else(|| crate::git::get_default_branch_in(Some(repo_path)).ok())
        .unwrap_or_else(|| "main".to_string())
}

/// The worktree's 20 most recent commits, as `%h\t%ar\t%s` lines.
pub fn recent_commits(worktree: &Path) -> anyhow::Result<String> {
    let output = crate::git::unattended_git(Some(worktree))?
        .args(["log", "--format=%h\t%ar\t%s", "-n", "20"])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeSite {
    Workstation(PathBuf),
    Remote {
        remote: String,
        project: String,
        /// The worktree's path on the remote.
        worktree: PathBuf,
    },
}

impl WorktreeSite {
    pub fn of(agent: &AgentPane) -> Self {
        match &agent.remote {
            Some(remote) => Self::Remote {
                remote: remote.remote.clone(),
                project: remote.project.clone(),
                worktree: agent.path.clone(),
            },
            None => Self::Workstation(agent.path.clone()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    /// The worktree's path where it lives.
    pub fn path(&self) -> &Path {
        match self {
            Self::Workstation(path) => path,
            Self::Remote { worktree, .. } => worktree,
        }
    }

    pub fn project(&self) -> Project {
        match self {
            Self::Workstation(path) => Project::of_workstation_path(path),
            Self::Remote {
                remote, project, ..
            } => Project::remote(remote, project),
        }
    }

    /// Key of the worktree's git status: `AgentPane::git_key`.
    pub fn status_key(&self) -> PathBuf {
        match self {
            Self::Workstation(path) => path.clone(),
            Self::Remote {
                remote,
                project,
                worktree,
            } => crate::remote::dashboard::worktree_key(remote, project, worktree),
        }
    }

    /// Key the PR and check caches file the worktree's repository under,
    /// found without git for a remote worktree.
    pub fn repo_key(&self) -> Option<PathBuf> {
        self.project().cache_key(self.path()).ok()
    }

    /// The remote project a remote worktree belongs to, from global config.
    fn remote_project(&self) -> Result<Option<RemoteProject>, String> {
        let Self::Remote {
            remote, project, ..
        } = self
        else {
            return Ok(None);
        };
        let resolved = crate::config::Config::load_global_only()
            .and_then(|config| crate::remote::router::remote_project(&config, project))
            .map_err(|error| format!("{error:#}"))?;
        if resolved.site.remote != *remote {
            return Err(format!("project {project} is no longer on remote {remote}"));
        }
        Ok(Some(resolved))
    }

    fn ask<T: serde::de::DeserializeOwned>(
        project: &RemoteProject,
        request: DashboardRequest,
        connect: bool,
    ) -> Result<T, String> {
        crate::remote::dashboard::request(project, &request, connect)
            .map_err(|error| format!("{error:#}"))
    }

    /// The worktree's git status. For a remote worktree only an open
    /// control channel is used; `None` keeps the last known status.
    pub fn git_status(&self, main_branch: Option<&str>) -> Option<GitStatus> {
        match self.remote_project() {
            Ok(None) => Some(crate::git::get_git_status(self.path(), main_branch)),
            Ok(Some(project)) => Self::ask(
                &project,
                DashboardRequest::GitStatus {
                    worktree: self.path().to_path_buf(),
                },
                false,
            )
            .inspect_err(|error| tracing::debug!(error, "remote git status unavailable"))
            .ok(),
            Err(error) => {
                tracing::debug!(error, "remote git status unavailable");
                None
            }
        }
    }

    /// The raw text of `git diff <diff_arg>`, with untracked files as new.
    pub fn raw_diff(&self, diff_arg: &str, include_untracked: bool) -> Result<String, String> {
        match self.remote_project()? {
            None => diff::raw_diff(self.path(), diff_arg, include_untracked),
            Some(project) => Self::ask(
                &project,
                DashboardRequest::Diff {
                    worktree: self.path().to_path_buf(),
                    diff_arg: diff_arg.to_string(),
                    include_untracked,
                },
                true,
            ),
        }
    }

    /// The diff colored for display: (content, lines_added, lines_removed, hunks).
    pub fn diff_content(
        &self,
        diff_arg: &str,
        include_untracked: bool,
        parse_hunks: bool,
    ) -> Result<(String, usize, usize, Vec<diff::DiffHunk>), String> {
        diff::render_diff(self.raw_diff(diff_arg, include_untracked)?, parse_hunks)
    }

    pub fn file_list(&self, diff_arg: &str, include_untracked: bool) -> Vec<FileEntry> {
        match self.remote_project() {
            Ok(None) => diff::get_file_list_numstat(self.path(), diff_arg, include_untracked),
            Ok(Some(project)) => Self::ask(
                &project,
                DashboardRequest::FileList {
                    worktree: self.path().to_path_buf(),
                    diff_arg: diff_arg.to_string(),
                    include_untracked,
                },
                true,
            )
            .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// End `pane`, a remote pane of this remote worktree's project.
    pub fn kill_remote_pane(&self, pane: crate::remote::pane::RemotePaneId) -> Result<(), String> {
        match self.remote_project()? {
            None => Ok(()),
            Some(project) => {
                Self::ask::<serde_json::Value>(&project, DashboardRequest::KillPane { pane }, true)
                    .map(drop)
            }
        }
    }

    /// Stage `patch` in the worktree's index; `reverse` unstages it.
    pub fn apply_to_index(&self, patch: &str, reverse: bool) -> Result<(), String> {
        match self.remote_project()? {
            None => diff::apply_to_index(self.path(), patch, reverse),
            Some(project) => Self::ask::<serde_json::Value>(
                &project,
                DashboardRequest::ApplyPatch {
                    worktree: self.path().to_path_buf(),
                    patch: patch.to_string(),
                    reverse,
                },
                true,
            )
            .map(drop),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::multiplexer::RemoteAgentPane;

    pub(crate) fn agent(path: &Path, remote: Option<(&str, &str)>) -> AgentPane {
        AgentPane {
            session: "api".to_string(),
            window_name: "wm-feature".to_string(),
            pane_id: "%1".to_string(),
            window_id: String::new(),
            window_index: None,
            path: path.to_path_buf(),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: None,
            remote: remote.map(|(remote, project)| RemoteAgentPane {
                remote: remote.to_string(),
                project: project.to_string(),
                stale: false,
            }),
        }
    }

    #[test]
    fn a_remote_worktree_is_never_keyed_by_a_workstation_path_it_shares() {
        // The remote's worktree path is also a repository on the workstation.
        let temp = tempfile::tempdir().unwrap();
        crate::test_support::init_repo(temp.path());
        let local = WorktreeSite::of(&agent(temp.path(), None));
        let devbox = WorktreeSite::of(&agent(temp.path(), Some(("devbox", "api"))));
        let other = WorktreeSite::of(&agent(temp.path(), Some(("other", "api"))));

        assert!(devbox.is_remote() && !local.is_remote());
        assert_eq!(local.status_key(), temp.path());
        assert_ne!(devbox.status_key(), local.status_key());
        assert_ne!(devbox.status_key(), other.status_key());
        assert_eq!(devbox.repo_key(), Some(PathBuf::from("remote/devbox/api")));
        assert_ne!(local.repo_key(), devbox.repo_key());
    }
}
