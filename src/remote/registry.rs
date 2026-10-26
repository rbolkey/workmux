//! Remote registry: where a repository's config puts its project (ADR 0004),
//! and the transports that reach remotes.
//!
//! A repository whose resolved config has `sandbox.backend: openshell` (or
//! the hidden `loopback`) lives on a remote. Each time a command resolves
//! such a project, the workstation records it, so processes with no
//! repository to resolve from (the workstation daemon, remote panes, the
//! sidebar and dashboard) know the project and the remote it is on. The
//! record follows config: the next command run in the repository rewrites
//! it.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::channels::TransportFactory;
use super::git_url::GitUrl;
use super::openshell::OpenShell;
use super::router::RemoteProject;
use super::session::ProjectSite;
use super::ssh::Ssh;
use super::transport::{Loopback, Transport};
use crate::config::{
    Config, OpenshellRemote, ProjectPlacement, RemoteConfig, SandboxBackend, TransportKind,
    openshell_sandbox_name,
};

/// Build the transport that reaches `remote`.
pub fn transport(_name: &str, remote: &RemoteConfig) -> Result<Arc<dyn Transport>> {
    match remote.transport {
        TransportKind::Loopback => Ok(Arc::new(Loopback::new(crate::util::expand_tilde(
            &remote.target,
        ))?)),
        TransportKind::Ssh => Ok(Arc::new(Ssh::new(&remote.target))),
        TransportKind::Openshell => Ok(Arc::new(OpenShell::new(
            &remote.target,
            remote.openshell.clone(),
        ))),
    }
}

pub fn transport_factory() -> TransportFactory {
    Arc::new(transport)
}

/// A remote project as the workstation recorded it when a command last
/// resolved it from its repository's config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Recorded {
    /// The workstation clone the project's config was resolved in.
    root: PathBuf,
    site: ProjectSite,
    remote: RemoteConfig,
}

/// The remote project the repository at `dir` lives in, by its resolved
/// config; `None` when the config keeps it on the workstation or `dir` is
/// in no repository. The project is recorded for processes that have no
/// repository to resolve from.
pub fn resolve_at(dir: &Path) -> Result<Option<RemoteProject>> {
    let (config, _) = Config::load_with_location_from(dir, None)?;
    if config.sandbox.remote_backend().is_none() {
        return Ok(None);
    }
    let Some(root) = crate::git::get_repo_root_if_present_in(Some(dir))? else {
        return Ok(None);
    };
    let root = crate::git::get_main_worktree_root_in(Some(&root)).unwrap_or(root);
    let origin = crate::git::get_remote_url_in("origin", Some(&root)).ok();
    let records = Records::open()?;
    let project = project_in(&config, &root, origin.as_deref(), &|remote, name| {
        records
            .get(remote, name)
            .map(|recorded| recorded.site.git_url)
    })?;
    records.record(&root, &project)?;
    Ok(Some(project))
}

/// The remote project `config` puts the repository at `root` in, given its
/// `origin` URL. `recorded` answers the git URL of a project already
/// recorded under a remote and name: a repository named like a recorded
/// project of another origin gets a name of its own.
fn project_in(
    config: &Config,
    root: &Path,
    origin: Option<&str>,
    recorded: &dyn Fn(&str, &str) -> Option<Option<GitUrl>>,
) -> Result<RemoteProject> {
    let backend = config
        .sandbox
        .remote_backend()
        .ok_or_else(|| anyhow!("{} is not in a sandbox", root.display()))?;
    let repository = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("Repository {} has no name", root.display()))?;
    let origin = origin.ok_or_else(|| {
        anyhow!(
            "Repository {} has no origin remote; its sandbox clones the repository from it",
            root.display()
        )
    })?;
    let git_url = match backend {
        SandboxBackend::Openshell => match super::openshell::repo::https_for_ssh(origin)? {
            Some(https) => GitUrl {
                url: https,
                ssh: Some(origin.to_string()),
            },
            None => GitUrl::new(origin),
        },
        _ => GitUrl::new(origin),
    };
    let remote_name = |name: &str| match backend {
        SandboxBackend::Loopback => config
            .sandbox
            .loopback
            .remote
            .clone()
            .unwrap_or_else(|| name.to_string()),
        _ => name.to_string(),
    };
    let name = match recorded(&remote_name(&repository), &repository) {
        Some(Some(other)) if other.original() != git_url.original() => {
            format!("{repository}-{}", short_hash(git_url.original()))
        }
        _ => repository,
    };
    let remote_name = remote_name(&name);
    validate_name(&name)?;
    let path = match backend {
        SandboxBackend::Loopback => config.sandbox.loopback.path.clone(),
        _ => config.sandbox.openshell.path.clone(),
    }
    .unwrap_or_else(|| format!("~/{name}"));
    let remote = match backend {
        SandboxBackend::Loopback => RemoteConfig {
            transport: TransportKind::Loopback,
            target: config.sandbox.loopback.root.clone().ok_or_else(|| {
                anyhow!("sandbox.loopback.root is required for the loopback backend")
            })?,
            builtin_shims: None,
            host_commands: Vec::new(),
            openshell: None,
        },
        _ => {
            let mut settings = config.sandbox.openshell.clone();
            settings.policy = settings
                .policy
                .map(|policy| root.join(crate::util::expand_tilde(&policy)))
                .map(|policy| policy.display().to_string());
            let agents = if settings.agents.is_empty() {
                config.default_agent_type().into_iter().collect()
            } else {
                settings.agents.clone()
            };
            RemoteConfig {
                transport: TransportKind::Openshell,
                target: settings
                    .sandbox
                    .clone()
                    .unwrap_or_else(|| openshell_sandbox_name(&name)),
                builtin_shims: None,
                host_commands: Vec::new(),
                openshell: Some(OpenshellRemote {
                    project: name.clone(),
                    git_url: Some(git_url.clone()),
                    path: path.clone(),
                    settings,
                    agents,
                }),
            }
        }
    };
    let session = match config.project_placement.unwrap_or_default() {
        ProjectPlacement::ProjectSession => Some(
            config
                .project_session
                .clone()
                .filter(|session| !session.trim().is_empty())
                .unwrap_or_else(|| name.clone()),
        ),
        ProjectPlacement::CurrentSession => None,
    };
    Ok(RemoteProject {
        site: ProjectSite {
            remote: remote_name,
            name,
            path,
            git_url: Some(git_url),
            session,
        },
        remote,
    })
}

/// Six hex digits of `text`'s hash.
fn short_hash(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())[..3]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The remote project recorded under `name`.
pub fn project(name: &str) -> Result<RemoteProject> {
    let records = Records::open()?;
    let mut found = records
        .all()?
        .into_iter()
        .filter(|recorded| recorded.site.name == name);
    let recorded = found.next().ok_or_else(|| {
        anyhow!("No remote project named {name}; run workmux in a clone of its repository first")
    })?;
    Ok(RemoteProject {
        site: recorded.site,
        remote: recorded.remote,
    })
}

/// The remote project recorded on `remote` under `name`, if any.
pub fn project_on(remote: &str, name: &str) -> Result<Option<RemoteProject>> {
    Ok(Records::open()?
        .get(remote, name)
        .map(|recorded| RemoteProject {
            site: recorded.site,
            remote: recorded.remote,
        }))
}

/// Every recorded remote project.
pub fn projects() -> Result<Vec<RemoteProject>> {
    Ok(Records::open()?
        .all()?
        .into_iter()
        .map(|recorded| RemoteProject {
            site: recorded.site,
            remote: recorded.remote,
        })
        .collect())
}

/// The remotes recorded projects are on, by name.
pub fn remotes() -> Result<BTreeMap<String, RemoteConfig>> {
    Ok(projects()?
        .into_iter()
        .map(|project| (project.site.remote, project.remote))
        .collect())
}

/// Forget the projects recorded on `remote`, as when its sandbox is
/// deleted.
pub fn forget_remote(remote: &str) -> Result<()> {
    let dir = Records::open()?.dir.join(remote);
    match std::fs::remove_dir_all(&dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("Failed to remove {}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
thread_local! {
    static TEST_RECORDS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Test-only: run `f` with remote projects recorded in `dir`, as though a
/// command had resolved `projects` there.
#[cfg(test)]
pub fn with_recorded<T>(dir: &Path, projects: &[RemoteProject], f: impl FnOnce() -> T) -> T {
    TEST_RECORDS.with(|records| *records.borrow_mut() = Some(dir.to_path_buf()));
    let records = Records::in_dir(dir.to_path_buf());
    for project in projects {
        records
            .record(Path::new("/workstation"), project)
            .expect("records");
    }
    let result = f();
    TEST_RECORDS.with(|records| *records.borrow_mut() = None);
    result
}

/// The recorded projects: `<state dir>/remote-projects/<remote>/<name>.json`.
struct Records {
    dir: PathBuf,
}

impl Records {
    fn open() -> Result<Self> {
        #[cfg(test)]
        if let Some(dir) = TEST_RECORDS.with(|dir| dir.borrow().clone()) {
            return Ok(Self::in_dir(dir));
        }
        Ok(Self::in_dir(
            crate::xdg::state_dir()?.join("remote-projects"),
        ))
    }

    fn in_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, remote: &str, name: &str) -> PathBuf {
        self.dir.join(remote).join(format!("{name}.json"))
    }

    fn get(&self, remote: &str, name: &str) -> Option<Recorded> {
        let text = std::fs::read_to_string(self.path(remote, name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn all(&self) -> Result<Vec<Recorded>> {
        let mut all = Vec::new();
        let Ok(remotes) = std::fs::read_dir(&self.dir) else {
            return Ok(all);
        };
        for remote in remotes.flatten() {
            let Ok(files) = std::fs::read_dir(remote.path()) else {
                continue;
            };
            for file in files.flatten() {
                if let Some(recorded) = std::fs::read_to_string(file.path())
                    .ok()
                    .and_then(|text| serde_json::from_str::<Recorded>(&text).ok())
                {
                    all.push(recorded);
                }
            }
        }
        all.sort_by_key(|recorded| recorded.site.id());
        Ok(all)
    }

    /// Record `project` as resolved in `root`, unless it already is.
    fn record(&self, root: &Path, project: &RemoteProject) -> Result<()> {
        let recorded = Recorded {
            root: root.to_path_buf(),
            site: project.site.clone(),
            remote: project.remote.clone(),
        };
        if self.get(&project.site.remote, &project.site.name).as_ref() == Some(&recorded) {
            return Ok(());
        }
        let path = self.path(&project.site.remote, &project.site.name);
        let dir = path.parent().expect("a record has a directory");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut file, &recorded)?;
        file.persist(&path)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }
}

/// Check that `name` can name a remote project.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("project name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!(
            "repository name {name:?} may only contain letters, digits, '-', '_' and '.' to live in a sandbox"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("config parses")
    }

    const OPENSHELL: &str = "sandbox:\n  enabled: true\n  backend: openshell\n";

    fn none(_: &str, _: &str) -> Option<Option<GitUrl>> {
        None
    }

    #[test]
    fn an_openshell_repository_is_a_project_named_after_its_root_in_a_sandbox_of_that_name() {
        let config = config(
            "sandbox:\n  enabled: true\n  backend: openshell\n  openshell:\n    \
             image: ghcr.io/me/api-dev:1\n    policy: policy.yaml\n    providers: [github]\n",
        );

        let project = project_in(
            &config,
            Path::new("/src/api"),
            Some("git@github.com:me/api.git"),
            &none,
        )
        .unwrap();

        assert_eq!(project.site.name, "api");
        assert_eq!(project.site.remote, "api");
        assert_eq!(project.site.path, "~/api");
        assert_eq!(project.site.session.as_deref(), Some("api"));
        let git_url = GitUrl {
            url: "https://github.com/me/api.git".to_string(),
            ssh: Some("git@github.com:me/api.git".to_string()),
        };
        assert_eq!(project.site.git_url.as_ref(), Some(&git_url));
        assert_eq!(project.remote.transport, TransportKind::Openshell);
        assert_eq!(project.remote.target, "api");
        let openshell = project.remote.openshell.expect("openshell settings");
        assert_eq!(openshell.project, "api");
        assert_eq!(openshell.git_url, Some(git_url));
        assert_eq!(openshell.path, "~/api");
        assert_eq!(
            openshell.settings.image.as_deref(),
            Some("ghcr.io/me/api-dev:1")
        );
        // A policy path is relative to the repository.
        assert_eq!(
            openshell.settings.policy.as_deref(),
            Some("/src/api/policy.yaml")
        );
        assert_eq!(openshell.settings.providers, ["github"]);
        assert_eq!(openshell.agents, ["claude"]);
    }

    #[test]
    fn a_project_may_name_its_sandbox_clone_path_and_session() {
        let config = config(
            "project_session: work\nsandbox:\n  enabled: true\n  backend: openshell\n  openshell:\n    \
             sandbox: wm-api-dev\n    path: src/api\n",
        );

        let project = project_in(
            &config,
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
            &none,
        )
        .unwrap();

        assert_eq!(project.remote.target, "wm-api-dev");
        assert_eq!(project.site.path, "src/api");
        assert_eq!(project.site.session.as_deref(), Some("work"));

        let config = self::config(&format!("project_placement: current-session\n{OPENSHELL}"));
        let project = project_in(
            &config,
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
            &none,
        )
        .unwrap();
        assert_eq!(project.site.session, None);
    }

    #[test]
    fn a_long_or_odd_repository_name_gets_a_sandbox_name_openshell_accepts() {
        let project = project_in(
            &config(OPENSHELL),
            Path::new("/src/a-very-long-project-name"),
            Some("https://github.com/me/x.git"),
            &none,
        )
        .unwrap();

        assert_eq!(project.site.name, "a-very-long-project-name");
        assert!(
            project.remote.target.len() <= 19,
            "{}",
            project.remote.target
        );
    }

    #[test]
    fn a_repository_named_like_a_project_of_another_origin_gets_a_name_of_its_own() {
        let theirs = |_: &str, name: &str| {
            (name == "api").then(|| Some(GitUrl::new("https://github.com/them/api.git")))
        };
        let mine = |origin| {
            project_in(
                &config(OPENSHELL),
                Path::new("/src/api"),
                Some(origin),
                &theirs,
            )
            .unwrap()
        };

        let project = mine("https://github.com/me/api.git");
        assert!(
            project.site.name.starts_with("api-"),
            "{}",
            project.site.name
        );
        assert_eq!(project.site.name.len(), "api-".len() + 6);
        assert_eq!(project.site.remote, project.site.name);
        assert_eq!(project.remote.target, project.site.name);
        assert_eq!(
            mine("https://github.com/me/api.git").site.name,
            project.site.name
        );

        // The same origin is the same project, wherever it is cloned.
        assert_eq!(mine("https://github.com/them/api.git").site.name, "api");
    }

    #[test]
    fn a_repository_without_an_origin_cannot_live_in_a_sandbox() {
        let error = project_in(&config(OPENSHELL), Path::new("/src/api"), None, &none)
            .unwrap_err()
            .to_string();

        assert!(error.contains("has no origin remote"), "{error}");
    }

    #[test]
    fn an_openshell_projects_agents_default_to_the_default_agents_type() {
        let agents_with = |yaml: &str, openshell: &str| {
            project_in(
                &config(&format!("{yaml}{OPENSHELL}{openshell}")),
                Path::new("/src/api"),
                Some("https://github.com/me/api.git"),
                &none,
            )
            .unwrap()
            .remote
            .openshell
            .unwrap()
            .agents
        };
        let agents = |yaml: &str| agents_with(yaml, "");

        assert_eq!(agents(""), ["claude"]);
        // A workstation path or a named agent means its plain CLI.
        assert_eq!(
            agents("agent: /Users/me/.local/bin/codex --yolo\n"),
            ["codex"]
        );
        assert_eq!(
            agents("agent: cc\nagents:\n  cc:\n    command: ~/bin/my-claude\n    type: claude\n"),
            ["claude"]
        );
        assert_eq!(
            agents_with("", "  openshell:\n    agents: [codex, claude]\n"),
            ["codex", "claude"]
        );
        // An agent workmux knows no type of names none.
        assert!(agents("agent: my-agent\n").is_empty());
    }

    #[test]
    fn a_loopback_repository_lives_on_the_remote_its_config_names() {
        let config = config(
            "sandbox:\n  enabled: true\n  backend: loopback\n  loopback:\n    \
             root: /tmp/lb\n    remote: lb\n    path: code/api\n",
        );

        let project =
            project_in(&config, Path::new("/ws/api"), Some("/origins/api"), &none).unwrap();

        assert_eq!(project.site.remote, "lb");
        assert_eq!(project.site.name, "api");
        assert_eq!(project.site.path, "code/api");
        assert_eq!(project.remote.transport, TransportKind::Loopback);
        assert_eq!(project.remote.target, "/tmp/lb");
        assert_eq!(project.remote.openshell, None);
    }

    #[test]
    fn a_resolved_project_is_recorded_and_found_again() {
        let dir = tempfile::tempdir().unwrap();
        let records = Records::in_dir(dir.path().to_path_buf());
        let project = project_in(
            &config(OPENSHELL),
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
            &none,
        )
        .unwrap();

        records.record(Path::new("/src/api"), &project).unwrap();

        let recorded = records.get("api", "api").expect("recorded");
        assert_eq!(recorded.root, Path::new("/src/api"));
        assert_eq!(recorded.site, project.site);
        assert_eq!(recorded.remote, project.remote);
        assert_eq!(records.all().unwrap(), vec![recorded]);
        assert!(records.get("api", "web").is_none());
    }
}
