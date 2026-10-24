//! Project identity: a project is named by its project host and its name.

use std::path::{Path, PathBuf};

use crate::remote::session::ServedProject;

/// Where a project's repository, worktrees and agents live.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProjectHost {
    /// The machine running the user's multiplexer.
    Workstation,
    /// A named project host reached through a transport.
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

    /// The project whose main worktree is `root`: the remote project remote
    /// workmux runs this command for, else the workstation project named
    /// after the directory.
    pub fn at_main_worktree_root(root: &Path) -> Option<Self> {
        Self::at_main_worktree_root_served(root, ServedProject::current())
    }

    fn at_main_worktree_root_served(root: &Path, served: Option<ServedProject>) -> Option<Self> {
        match served {
            Some(served) => Some(Self::remote(served.remote, served.name)),
            None => Self::from_main_worktree_root(root),
        }
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
        match self.remote_id() {
            None => self.name.clone(),
            Some(id) => id.label(),
        }
    }

    /// This project's identity on its remote; `None` on the workstation.
    pub fn remote_id(&self) -> Option<RemoteProjectId> {
        match &self.host {
            ProjectHost::Workstation => None,
            ProjectHost::Remote(remote) => Some(RemoteProjectId::new(remote, &self.name)),
        }
    }

    /// Key the git and PR caches file this project's `worktree` under. A
    /// workstation project uses its repository's git common dir, shared by
    /// every worktree; a remote project uses its remote and project names,
    /// which no absolute git common dir can equal.
    pub fn cache_key(&self, worktree: &Path) -> anyhow::Result<PathBuf> {
        match self.remote_id() {
            None => crate::git::get_repository_key_in(Some(worktree)),
            Some(id) => Ok(id.cache_key()),
        }
    }
}

/// A project on a remote, named by its remote and the name it is
/// registered under in global config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteProjectId {
    pub remote: String,
    pub name: String,
}

impl RemoteProjectId {
    pub fn new(remote: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            name: name.into(),
        }
    }

    /// The project named by `tag`, the tag on its sessions and windows.
    pub fn from_tag(tag: &str) -> Option<Self> {
        tag.trim()
            .split_once(':')
            .filter(|(remote, name)| !remote.is_empty() && !name.is_empty())
            .map(|(remote, name)| Self::new(remote, name))
    }

    /// The tag workmux puts on the project's sessions and windows.
    pub fn tag(&self) -> String {
        format!("{}:{}", self.remote, self.name)
    }

    /// How the project is shown: its name and its remote, so it never reads
    /// as a same-named project on another project host.
    pub fn label(&self) -> String {
        format!("{}@{}", self.name, self.remote)
    }

    /// Key the git and PR caches file the project under. It is relative and
    /// names the remote and project, so no absolute git common dir and no
    /// other remote's project can equal it.
    pub fn cache_key(&self) -> PathBuf {
        Path::new("remote").join(&self.remote).join(&self.name)
    }

    /// Key of the project's `worktree` in the workstation's caches. It
    /// never equals a workstation path or another remote's worktree, even
    /// when the remote path also exists on the workstation.
    pub fn worktree_key(&self, worktree: &Path) -> PathBuf {
        self.cache_key()
            .join(worktree.strip_prefix("/").unwrap_or(worktree))
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
    fn a_served_remote_project_is_named_by_its_registration_not_its_clone() {
        let served = ServedProject {
            remote: "devbox".to_string(),
            name: "api".to_string(),
            session: None,
        };
        let project = Project::at_main_worktree_root_served(
            Path::new("/home/me/code/api-clone"),
            Some(served),
        )
        .unwrap();
        assert_eq!(project, Project::remote("devbox", "api"));
        assert_eq!(project.expand_placeholder("wm-{project}-"), "wm-api-");
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
    fn a_remote_project_is_spelled_as_tag_label_and_cache_key() {
        let id = RemoteProjectId::new("devbox", "api");
        assert_eq!(id.tag(), "devbox:api");
        assert_eq!(id.label(), "api@devbox");
        assert_eq!(id.cache_key(), Path::new("remote/devbox/api"));
        assert_eq!(
            id.worktree_key(Path::new("/home/me/api__worktrees/feature")),
            Path::new("remote/devbox/api/home/me/api__worktrees/feature")
        );
    }

    #[test]
    fn a_tag_names_its_remote_project() {
        assert_eq!(
            RemoteProjectId::from_tag(" devbox:api\n"),
            Some(RemoteProjectId::new("devbox", "api"))
        );
        assert_eq!(RemoteProjectId::from_tag("garbage"), None);
        assert_eq!(RemoteProjectId::from_tag(":api"), None);
        assert_eq!(RemoteProjectId::from_tag("devbox:"), None);
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
