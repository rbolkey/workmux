//! Project identity: a project is named by its project host and its name.

use std::path::{Path, PathBuf};

/// Where a project's repository, worktrees and agents live.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProjectHost {
    /// The machine running the user's multiplexer.
    Workstation,
    /// A named project host reached through a transport.
    #[allow(dead_code)] // Constructed once remote projects are registered
    Remote(String),
}

/// A repository and its worktrees as workmux manages them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Project {
    pub host: ProjectHost,
    pub name: String,
}

impl Project {
    pub fn workstation(name: impl Into<String>) -> Self {
        Self {
            host: ProjectHost::Workstation,
            name: name.into(),
        }
    }

    #[allow(dead_code)] // Constructed once remote projects are registered
    pub fn remote(remote: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: ProjectHost::Remote(remote.into()),
            name: name.into(),
        }
    }

    /// The workstation project whose main worktree is `root`, named after
    /// its directory. `None` when the path has no final component.
    pub fn from_main_worktree_root(root: &Path) -> Option<Self> {
        Some(Self::workstation(root.file_name()?.to_string_lossy()))
    }

    /// The workstation project a worktree or agent at `path` belongs to.
    pub fn of_workstation_path(path: &Path) -> Self {
        Self::workstation(crate::agent_display::extract_project_name(path))
    }

    /// Replace `{project}` in a config template with this project's name.
    pub fn expand_placeholder(&self, template: &str) -> String {
        template.replace(crate::util::PROJECT_PLACEHOLDER, &self.name)
    }

    /// Label of the sidebar and dashboard group holding this project's
    /// agents. A remote project's label names its remote, so it never merges
    /// with a same-named project on another project host.
    pub fn group_label(&self) -> String {
        match &self.host {
            ProjectHost::Workstation => self.name.clone(),
            ProjectHost::Remote(remote) => format!("{}@{}", self.name, remote),
        }
    }

    /// Key the git and PR caches file this project's `worktree` under. A
    /// workstation project uses its repository's git common dir, shared by
    /// every worktree; a remote project uses its remote and project names,
    /// which no absolute git common dir can equal.
    pub fn cache_key(&self, worktree: &Path) -> anyhow::Result<PathBuf> {
        match &self.host {
            ProjectHost::Workstation => crate::git::get_repository_key_in(Some(worktree)),
            ProjectHost::Remote(remote) => Ok(Path::new("remote").join(remote).join(&self.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workstation_project_is_named_after_its_main_worktree_directory() {
        let project = Project::from_main_worktree_root(Path::new("/src/api")).unwrap();
        assert_eq!(project.host, ProjectHost::Workstation);
        assert_eq!(project.name, "api");
    }

    #[test]
    fn same_named_projects_on_different_project_hosts_group_separately() {
        let local = Project::workstation("api");
        let remote = Project::remote("devbox", "api");
        assert_ne!(local, remote);
        assert_eq!(local.group_label(), "api");
        assert_eq!(remote.group_label(), "api@devbox");
    }

    #[test]
    fn workstation_cache_key_is_shared_by_every_worktree_of_the_repository() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("api");
        std::fs::create_dir(&repo).unwrap();
        crate::test_support::init_repo(&repo);
        let linked = temp.path().join("api__worktrees").join("feature");
        crate::test_support::run_git(
            &repo,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        let project = Project::workstation("api");
        let expected = repo.join(".git").canonicalize().unwrap();
        assert_eq!(project.cache_key(&repo).unwrap(), expected);
        assert_eq!(project.cache_key(&linked).unwrap(), expected);
    }

    #[test]
    fn remote_cache_key_is_the_remote_name_plus_the_project_name() {
        let worktree = Path::new("/home/me/api");
        let devbox = Project::remote("devbox", "api")
            .cache_key(worktree)
            .unwrap();
        let other = Project::remote("other", "api").cache_key(worktree).unwrap();
        assert_eq!(devbox, Path::new("remote/devbox/api"));
        assert_ne!(devbox, other);
    }

    #[test]
    fn project_placeholder_is_the_bare_name_on_every_project_host() {
        let template = "~/.workmux/{project}";
        assert_eq!(
            Project::workstation("api").expand_placeholder(template),
            "~/.workmux/api"
        );
        assert_eq!(
            Project::remote("devbox", "api").expand_placeholder(template),
            "~/.workmux/api"
        );
    }
}
