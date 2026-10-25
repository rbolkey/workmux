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

    /// The project this worktree belongs to.
    pub fn scope(&self) -> ProjectScope {
        match self {
            Self::Workstation(path) => ProjectScope::Workstation(path.clone()),
            Self::Remote {
                remote, project, ..
            } => ProjectScope::Remote {
                remote: remote.clone(),
                project: project.clone(),
            },
        }
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
            } => crate::project::RemoteProjectId::new(remote, project).worktree_key(worktree),
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
        let resolved = crate::remote::router::global_config()
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

    /// Whether the worktree has uncommitted changes; `true` when unknown.
    pub fn is_dirty(&self) -> bool {
        match self.remote_project() {
            Ok(None) => crate::git::has_uncommitted_changes(self.path()).unwrap_or(true),
            Ok(Some(project)) => Self::ask::<GitStatus>(
                &project,
                DashboardRequest::GitStatus {
                    worktree: self.path().to_path_buf(),
                },
                true,
            )
            .map(|status| status.is_dirty)
            .unwrap_or(true),
            Err(_) => true,
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

    /// The worktree's recent commits, as `%h\t%ar\t%s` lines.
    pub fn recent_commits(&self) -> Result<String, String> {
        match self.remote_project()? {
            None => recent_commits(self.path()).map_err(|error| format!("{error:#}")),
            Some(project) => Self::ask(
                &project,
                DashboardRequest::Log {
                    worktree: self.path().to_path_buf(),
                },
                true,
            ),
        }
    }

    /// Record `base` as the base branch of `branch`, checked out here.
    pub fn set_base(&self, branch: &str, base: &str) -> Result<(), String> {
        match self.remote_project()? {
            None => crate::git::set_branch_base_in(branch, base, Some(self.path()))
                .map_err(|error| format!("{error:#}")),
            Some(project) => Self::ask::<serde_json::Value>(
                &project,
                DashboardRequest::SetBase {
                    worktree: self.path().to_path_buf(),
                    branch: branch.to_string(),
                    base: base.to_string(),
                },
                true,
            )
            .map(drop),
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

/// The project whose worktrees the Worktrees tab shows: a workstation
/// repository, or a project registered on a remote, whose worktrees remote
/// workmux lists and changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectScope {
    /// A path inside the repository.
    Workstation(PathBuf),
    Remote {
        remote: String,
        project: String,
    },
}

impl ProjectScope {
    pub fn project(&self) -> Project {
        match self {
            Self::Workstation(path) => Project::of_workstation_path(path),
            Self::Remote { remote, project } => Project::remote(remote, project),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    /// Where the worktree at `path`, one of this project's, lives.
    pub fn site_of(&self, path: &Path) -> WorktreeSite {
        match self {
            Self::Workstation(_) => WorktreeSite::Workstation(path.to_path_buf()),
            Self::Remote { remote, project } => WorktreeSite::Remote {
                remote: remote.clone(),
                project: project.clone(),
                worktree: path.to_path_buf(),
            },
        }
    }

    /// Where to ask GitHub about the worktree at `path`, never a remote
    /// path: a remote project is asked about by its registered git URL.
    pub fn repo_source(
        &self,
        config: &crate::config::Config,
        path: &Path,
    ) -> Option<crate::github::RepoSource> {
        match self {
            Self::Workstation(_) => Some(crate::github::RepoSource::Checkout(path.to_path_buf())),
            Self::Remote { remote, project } => {
                let entry = config.projects.get(project)?;
                (entry.remote.as_deref() == Some(remote.as_str()))
                    .then(|| {
                        entry
                            .git_url
                            .clone()
                            .map(|git_url| crate::github::RepoSource::GitUrl(git_url.url))
                    })
                    .flatten()
            }
        }
    }

    fn remote_project(&self) -> anyhow::Result<Option<RemoteProject>> {
        match self {
            Self::Workstation(_) => Ok(None),
            Self::Remote { remote, project } => {
                let resolved = crate::remote::router::remote_project(
                    &*crate::remote::router::global_config()?,
                    project,
                )?;
                if resolved.site.remote != *remote {
                    anyhow::bail!("project {project} is no longer on remote {remote}");
                }
                Ok(Some(resolved))
            }
        }
    }

    fn ask<T: serde::de::DeserializeOwned>(
        project: &RemoteProject,
        request: DashboardRequest,
    ) -> anyhow::Result<T> {
        crate::remote::dashboard::request(project, &request, true)
    }

    /// Run `workmux <args>` for a remote project on its remote, as typed
    /// there, without touching the dashboard's terminal.
    fn relay(project: &RemoteProject, args: &[&str]) -> anyhow::Result<()> {
        let argv: Vec<&str> = std::iter::once("workmux")
            .chain(args.iter().copied())
            .collect();
        let ran = crate::remote::router::captured(project, &argv)?;
        if ran.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&ran.stderr);
            anyhow::bail!(
                "{}",
                stderr
                    .lines()
                    .rfind(|line| !line.trim().is_empty())
                    .unwrap_or("remote workmux failed")
                    .trim()
            );
        }
        Ok(())
    }

    /// The project's worktrees.
    pub fn list(
        &self,
        config: &crate::config::Config,
        mux: &dyn crate::multiplexer::Multiplexer,
    ) -> anyhow::Result<Vec<crate::workflow::types::WorktreeInfo>> {
        match self.remote_project()? {
            // fetch_pr_status=false: the dashboard fetches PR status separately,
            // and workflow::list's spinner would corrupt the TUI output
            None => {
                let Self::Workstation(path) = self else {
                    unreachable!()
                };
                crate::workflow::list_in(config, mux, false, &[], Some(path))
            }
            Some(project) => crate::command::list::remote_worktrees(&project),
        }
    }

    /// The repository's local branches.
    pub fn branches(&self) -> anyhow::Result<Vec<String>> {
        match (self, self.remote_project()?) {
            (Self::Workstation(path), _) => crate::git::list_local_branches_in(Some(path)),
            (_, Some(project)) => Self::ask(&project, DashboardRequest::Branches),
            _ => unreachable!(),
        }
    }

    /// The branch a new worktree starts from by default.
    pub fn default_base(&self) -> String {
        match (self, self.remote_project()) {
            (Self::Workstation(path), _) => default_add_worktree_base(path),
            (_, Ok(Some(project))) => Self::ask(&project, DashboardRequest::DefaultBase)
                .unwrap_or_else(|_| "main".to_string()),
            _ => "main".to_string(),
        }
    }

    /// Branches whose upstream branch was deleted.
    pub fn gone_branches(&self) -> std::collections::HashSet<String> {
        match (self, self.remote_project()) {
            (Self::Workstation(path), _) => {
                crate::git::get_gone_branches_in(Some(path)).unwrap_or_default()
            }
            (_, Ok(Some(project))) => {
                Self::ask::<Vec<String>>(&project, DashboardRequest::GoneBranches)
                    .map(|gone| gone.into_iter().collect())
                    .unwrap_or_default()
            }
            _ => Default::default(),
        }
    }

    /// Open pull requests of the project's repository.
    pub fn open_prs(
        &self,
        config: &crate::config::Config,
    ) -> anyhow::Result<Vec<crate::github::PrListEntry>> {
        let path = match self {
            Self::Workstation(path) => path.as_path(),
            Self::Remote { .. } => Path::new(""),
        };
        let source = self
            .repo_source(config, path)
            .ok_or_else(|| anyhow::anyhow!("project {} has no git URL", self.project().name))?;
        crate::github::list_open_prs_from(&source)
    }

    /// Remove a remote project's worktree `handle` as `workmux remove`
    /// does. The dashboard has already confirmed it.
    pub fn remove_remote(&self, handle: &str, keep_branch: bool) -> anyhow::Result<()> {
        let project = self
            .remote_project()?
            .ok_or_else(|| anyhow::anyhow!("not a remote project"))?;
        let mut args = vec!["remove", handle, "--force"];
        if keep_branch {
            args.push("--keep-branch");
        }
        Self::relay(&project, &args)
    }

    /// Create a remote project's worktree for `branch`, from `base` when
    /// given, or for pull request `pr`, without switching to its window.
    pub fn add_remote(
        &self,
        branch: Option<&str>,
        base: Option<&str>,
        pr: Option<u32>,
    ) -> anyhow::Result<()> {
        let project = self
            .remote_project()?
            .ok_or_else(|| anyhow::anyhow!("not a remote project"))?;
        let pr = pr.map(|pr| pr.to_string());
        let mut args = vec!["add", "--background"];
        args.extend(branch);
        if let Some(base) = base {
            args.extend(["--base", base]);
        }
        if let Some(pr) = &pr {
            args.extend(["--pr", pr]);
        }
        Self::relay(&project, &args)
    }

    /// Close a remote project's worktree window, keeping the worktree.
    pub fn close_remote(&self, handle: &str) -> anyhow::Result<()> {
        let project = self
            .remote_project()?
            .ok_or_else(|| anyhow::anyhow!("not a remote project"))?;
        Self::relay(&project, &["close", handle])
    }

    /// Open (or switch to) a remote project's worktree window.
    pub fn open_remote(&self, handle: &str) -> anyhow::Result<()> {
        let project = self
            .remote_project()?
            .ok_or_else(|| anyhow::anyhow!("not a remote project"))?;
        Self::relay(&project, &["open", handle])
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
                worktree: "feature".to_string(),
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

    #[test]
    fn a_remote_projects_worktree_rows_are_keyed_and_asked_about_by_remote_and_project() {
        let temp = tempfile::tempdir().unwrap();
        crate::test_support::init_repo(temp.path());
        let config: crate::config::Config = serde_yaml::from_str(
            "projects:\n  api:\n    remote: devbox\n    git_url: git@github.com:me/api.git\n",
        )
        .unwrap();
        let remote = ProjectScope::Remote {
            remote: "devbox".to_string(),
            project: "api".to_string(),
        };
        let local = ProjectScope::Workstation(temp.path().to_path_buf());

        let row = remote.site_of(temp.path());

        assert_eq!(
            row,
            WorktreeSite::of(&agent(temp.path(), Some(("devbox", "api"))))
        );
        assert_eq!(row.scope(), remote);
        assert_eq!(remote.project().group_label(), "api@devbox");
        assert_eq!(
            remote.repo_source(&config, temp.path()),
            Some(crate::github::RepoSource::GitUrl(
                "git@github.com:me/api.git".to_string()
            ))
        );
        assert_eq!(
            local.repo_source(&config, temp.path()),
            Some(crate::github::RepoSource::Checkout(
                temp.path().to_path_buf()
            ))
        );
        let elsewhere = ProjectScope::Remote {
            remote: "other".to_string(),
            project: "api".to_string(),
        };
        assert_eq!(elsewhere.repo_source(&config, temp.path()), None);
    }
}
