//! Where a repository's config puts its project (ADR 0004).
//!
//! A repository whose resolved config has `sandbox.backend: openshell` (or
//! the hidden `loopback`) lives on a remote. Nothing about a project is
//! stored: its workstation sessions and windows are tagged with the root of
//! the clone it was resolved in, and every process that starts from a tag
//! (the workstation daemon, remote panes, the sidebar and dashboard)
//! resolves the config at that root again, as a command run in the clone
//! does.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::Path;

use super::git_url::GitUrl;
use super::router::RemoteProject;
use super::session::ProjectSite;
use crate::config::{
    Config, OpenshellRemote, RemoteConfig, SandboxBackend, TransportKind, openshell_sandbox_name,
};
use crate::multiplexer::{InvocationContext, TmuxBackend};
use crate::project::{ProjectTag, RemoteProjectId};

/// The remote project the repository at `dir` lives in, by its resolved
/// config; `None` when the config keeps it on the workstation or `dir` is
/// in no repository.
pub fn at(dir: &Path) -> Result<Option<RemoteProject>> {
    let (config, _) = Config::load_with_location_from(dir, None)?;
    if config.sandbox.remote_backend().is_none() {
        return Ok(None);
    }
    let Some(root) = crate::git::get_repo_root_if_present_in(Some(dir))? else {
        return Ok(None);
    };
    let root = crate::git::get_main_worktree_root_in(Some(&root)).unwrap_or(root);
    let root = root.canonicalize().unwrap_or(root);
    let origin = crate::git::get_remote_url_in("origin", Some(&root)).ok();
    project_in(&config, &root, origin.as_deref()).map(Some)
}

/// The remote project `tag` names, resolved again from the config of the
/// clone it was resolved in: `None` when that config no longer puts the
/// clone in this project, so the window is no longer the project's.
pub fn tagged(tag: &ProjectTag) -> Result<Option<RemoteProject>> {
    #[cfg(test)]
    if let Some(project) = testing::tagged(tag) {
        return Ok(project);
    }
    if !tag.root.is_dir() {
        bail!(
            "The clone of project {} at {} no longer exists; its windows cannot reach its sandbox",
            tag.id.name,
            tag.root.display()
        );
    }
    Ok(at(&tag.root)?.filter(|project| project.id() == tag.id))
}

/// The workstation's tmux server, as this process was invoked in it.
pub fn workstation_tmux() -> TmuxBackend {
    TmuxBackend::for_socket(
        InvocationContext::current()
            .tmux_socket_path()
            .unwrap_or("default"),
    )
}

/// The project tags on the sessions and windows of `tmux`, each once.
pub fn open_tags(tmux: &TmuxBackend) -> Vec<ProjectTag> {
    #[cfg(test)]
    if let Some(tags) = testing::tags() {
        return tags;
    }
    let format = format!(
        "#{{{}}}\t#{{{}}}",
        super::forwarded::WINDOW_TAG,
        super::forwarded::SESSION_TAG
    );
    let listing = tmux
        .query(&["list-windows", "-a", "-F", &format])
        .unwrap_or_default();
    let mut tags: Vec<ProjectTag> = Vec::new();
    for tag in listing
        .lines()
        .flat_map(|line| line.split('\t'))
        .filter_map(ProjectTag::parse)
    {
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

/// The remote projects with open workstation windows, each resolved from
/// its clone's config now. A window whose clone is gone or no longer in a
/// sandbox names none.
pub fn open_projects() -> Vec<RemoteProject> {
    let mut projects: Vec<RemoteProject> = Vec::new();
    for tag in open_tags(&workstation_tmux()) {
        match tagged(&tag) {
            Ok(Some(project)) => {
                if !projects.iter().any(|known| known.id() == project.id()) {
                    projects.push(project);
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(error = %format!("{error:#}"), "skipping a tagged window")
            }
        }
    }
    projects
}

/// Remote project `name` on `remote`, resolved from the clone its open
/// workstation windows are tagged with.
pub fn open_project(remote: &str, name: &str) -> Result<RemoteProject> {
    let id = RemoteProjectId::new(remote, name);
    let tags = open_tags(&workstation_tmux());
    let tag = tags.iter().find(|tag| tag.id == id).ok_or_else(|| {
        anyhow!("Project {name} on {remote} has no open workstation window; run workmux in a clone of its repository")
    })?;
    tagged(tag)?.ok_or_else(|| {
        anyhow!(
            "The config of {} no longer puts project {name} on {remote}",
            tag.root.display()
        )
    })
}

/// The remote project `config` puts the repository at `root` in, given its
/// `origin` URL.
fn project_in(config: &Config, root: &Path, origin: Option<&str>) -> Result<RemoteProject> {
    let backend = config
        .sandbox
        .remote_backend()
        .ok_or_else(|| anyhow!("{} is not in a sandbox", root.display()))?;
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("Repository {} has no name", root.display()))?;
    validate_name(&name)?;
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
    let path = match backend {
        SandboxBackend::Loopback => config.sandbox.loopback.path.clone(),
        _ => config.sandbox.openshell.path.clone(),
    }
    .unwrap_or_else(|| format!("~/{name}"));
    let passthrough: Vec<String> = config
        .sandbox
        .env_passthrough()
        .into_iter()
        .map(String::from)
        .collect();
    let env: BTreeMap<String, String> = config
        .sandbox
        .env_vars()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    let (remote_name, remote_config) = match backend {
        SandboxBackend::Loopback => (
            config
                .sandbox
                .loopback
                .remote
                .clone()
                .unwrap_or_else(|| name.clone()),
            RemoteConfig {
                transport: TransportKind::Loopback,
                target: config.sandbox.loopback.root.clone().ok_or_else(|| {
                    anyhow!("sandbox.loopback.root is required for the loopback backend")
                })?,
                builtin_shims: None,
                host_commands: config.sandbox.host_commands().to_vec(),
                env_passthrough: passthrough.clone(),
                env: env.clone(),
                openshell: None,
            },
        ),
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
            let sandbox = openshell_sandbox_name(root);
            (
                sandbox.clone(),
                RemoteConfig {
                    transport: TransportKind::Openshell,
                    target: sandbox,
                    builtin_shims: None,
                    host_commands: config.sandbox.host_commands().to_vec(),
                    env_passthrough: passthrough.clone(),
                    env: env.clone(),
                    openshell: Some(OpenshellRemote {
                        project: name.clone(),
                        git_url: Some(git_url.clone()),
                        path: path.clone(),
                        settings,
                        agents,
                        nix: crate::sandbox::toolchain::resolve_toolchain(
                            &config.sandbox.toolchain(),
                            root,
                        ) == crate::sandbox::toolchain::DetectedToolchain::Flake,
                        extra_mounts: !config.sandbox.extra_mounts().is_empty(),
                    }),
                },
            )
        }
    };
    Ok(RemoteProject {
        root: root.to_path_buf(),
        site: ProjectSite {
            remote: remote_name,
            name,
            path,
            git_url: Some(git_url),
        },
        remote_config,
    })
}

/// Check that `name` can name a remote project.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Project name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!(
            "Repository name {name:?} may only contain letters, digits, '-', '_' and '.' to live in a sandbox"
        );
    }
    Ok(())
}

/// Test-only: run `f` as though the workstation had windows open for
/// `projects`, each resolved from its root's config as given.
#[cfg(test)]
pub fn with_open<T>(projects: &[RemoteProject], f: impl FnOnce() -> T) -> T {
    testing::OPEN.with(|open| *open.borrow_mut() = Some(projects.to_vec()));
    let result = with_tags(
        &projects.iter().map(RemoteProject::tag).collect::<Vec<_>>(),
        f,
    );
    testing::OPEN.with(|open| *open.borrow_mut() = None);
    result
}

/// Test-only: run `f` as though the workstation's windows carried `tags`.
#[cfg(test)]
pub fn with_tags<T>(tags: &[ProjectTag], f: impl FnOnce() -> T) -> T {
    testing::TAGS.with(|open| *open.borrow_mut() = Some(tags.to_vec()));
    let result = f();
    testing::TAGS.with(|open| *open.borrow_mut() = None);
    result
}

#[cfg(test)]
mod testing {
    use super::*;

    thread_local! {
        pub static OPEN: std::cell::RefCell<Option<Vec<RemoteProject>>> = const { std::cell::RefCell::new(None) };
        pub static TAGS: std::cell::RefCell<Option<Vec<ProjectTag>>> = const { std::cell::RefCell::new(None) };
    }

    pub fn tags() -> Option<Vec<ProjectTag>> {
        TAGS.with(|tags| tags.borrow().clone())
    }

    pub fn tagged(tag: &ProjectTag) -> Option<Option<RemoteProject>> {
        OPEN.with(|open| {
            open.borrow().as_ref().map(|projects| {
                projects
                    .iter()
                    .find(|project| project.tag() == *tag)
                    .cloned()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("config parses")
    }

    const OPENSHELL: &str = "sandbox:\n  enabled: true\n  backend: openshell\n";

    fn openshell_at(root: &str) -> RemoteProject {
        project_in(
            &config(OPENSHELL),
            Path::new(root),
            Some("https://github.com/me/api.git"),
        )
        .unwrap()
    }

    #[test]
    fn an_openshell_repository_is_a_project_named_after_its_root_in_a_sandbox_named_after_its_path()
    {
        let config = config(
            "sandbox:\n  enabled: true\n  backend: openshell\n  openshell:\n    \
             image: ghcr.io/me/api-dev:1\n    policy: policy.yaml\n    providers: [github]\n",
        );

        let project = project_in(
            &config,
            Path::new("/src/api"),
            Some("git@github.com:me/api.git"),
        )
        .unwrap();

        let sandbox = openshell_sandbox_name(Path::new("/src/api"));
        assert!(sandbox.starts_with("api-"), "{sandbox}");
        assert_eq!(project.root, Path::new("/src/api"));
        assert_eq!(project.site.name, "api");
        assert_eq!(project.site.remote, sandbox);
        assert_eq!(project.site.path, "~/api");
        let git_url = GitUrl {
            url: "https://github.com/me/api.git".to_string(),
            ssh: Some("git@github.com:me/api.git".to_string()),
        };
        assert_eq!(project.site.git_url.as_ref(), Some(&git_url));
        assert_eq!(project.remote_config.transport, TransportKind::Openshell);
        assert_eq!(project.remote_config.target, sandbox);
        let openshell = project.remote_config.openshell.expect("openshell settings");
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
    fn a_project_may_name_its_clone_path() {
        let config = config(
            "sandbox:\n  enabled: true\n  backend: openshell\n  openshell:\n    path: src/api\n",
        );

        let project = project_in(
            &config,
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
        )
        .unwrap();

        assert_eq!(project.site.path, "src/api");
    }

    #[test]
    fn same_named_repositories_get_sandboxes_of_their_own() {
        let mine = openshell_at("/src/api");
        let theirs = openshell_at("/work/api");

        assert_eq!(mine.site.name, theirs.site.name);
        assert_ne!(mine.remote_config.target, theirs.remote_config.target);
        assert_ne!(mine.id(), theirs.id());
        // A sandbox's name depends on its own clone alone: renaming or
        // removing another changes nothing.
        assert_eq!(
            openshell_at("/src/api").remote_config.target,
            mine.remote_config.target
        );
    }

    #[test]
    fn a_long_or_odd_repository_name_gets_a_sandbox_name_openshell_accepts() {
        let project = openshell_at("/src/a-very-long-project-name");

        assert_eq!(project.site.name, "a-very-long-project-name");
        assert!(
            project.remote_config.target.len() <= 19,
            "{}",
            project.remote_config.target
        );
    }

    #[test]
    fn a_repository_without_an_origin_cannot_live_in_a_sandbox() {
        let error = project_in(&config(OPENSHELL), Path::new("/src/api"), None)
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
            )
            .unwrap()
            .remote_config
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
    fn an_openshell_sandbox_gets_nix_when_the_toolchain_resolves_to_a_flake() {
        let nix = |files: &[&str], toolchain: &str| {
            let root = tempfile::tempdir().unwrap();
            for file in files {
                std::fs::write(root.path().join(file), "{}").unwrap();
            }
            project_in(
                &config(&format!("{OPENSHELL}  toolchain: {toolchain}\n")),
                root.path(),
                Some("https://github.com/me/api.git"),
            )
            .unwrap()
            .remote_config
            .openshell
            .unwrap()
            .nix
        };

        assert!(nix(&["flake.nix"], "auto"));
        assert!(!nix(&[], "auto"));
        // devbox wins over a flake, as with Lima.
        assert!(!nix(&["devbox.json", "flake.nix"], "auto"));
        assert!(nix(&[], "flake"));
        assert!(!nix(&["flake.nix"], "off"));
        assert!(!nix(&["flake.nix"], "devbox"));
    }

    #[test]
    fn a_loopback_repository_lives_on_the_remote_its_config_names() {
        let config = config(
            "sandbox:\n  enabled: true\n  backend: loopback\n  loopback:\n    \
             root: /tmp/lb\n    remote: lb\n    path: code/api\n",
        );

        let project = project_in(&config, Path::new("/ws/api"), Some("/origins/api")).unwrap();

        assert_eq!(project.site.remote, "lb");
        assert_eq!(project.site.name, "api");
        assert_eq!(project.site.path, "code/api");
        assert_eq!(project.remote_config.transport, TransportKind::Loopback);
        assert_eq!(project.remote_config.target, "/tmp/lb");
        assert_eq!(project.remote_config.openshell, None);
    }

    #[test]
    fn a_remote_may_run_the_host_commands_global_config_allows() {
        let global = config("sandbox:\n  host_commands: [just]\n");
        let project = config(&format!("{OPENSHELL}  host_commands: [curl]\n"));

        let project = project_in(
            &global.merge_for_test(project),
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
        )
        .unwrap();

        assert_eq!(project.remote_config.host_commands, ["just"]);
    }

    #[test]
    fn a_remote_gets_the_environment_global_config_passes_through() {
        let global = config("sandbox:\n  env_passthrough: [GITHUB_TOKEN]\n");
        let project = config(&format!(
            "{OPENSHELL}  env_passthrough: [AWS_SECRET_ACCESS_KEY]\n"
        ));

        let project = project_in(
            &global.merge_for_test(project),
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
        )
        .unwrap();

        assert_eq!(project.remote_config.env_passthrough, ["GITHUB_TOKEN"]);
    }

    #[test]
    fn a_remote_gets_the_environment_global_config_sets() {
        let global = config("sandbox:\n  env:\n    API_URL: https://api.test\n");
        let project = config(&format!(
            "{OPENSHELL}  env:\n    API_URL: https://evil.test\n"
        ));

        let project = project_in(
            &global.merge_for_test(project),
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
        )
        .unwrap();

        assert_eq!(
            project.remote_config.env,
            BTreeMap::from([("API_URL".to_string(), "https://api.test".to_string())])
        );
    }

    #[test]
    fn an_openshell_sandbox_knows_the_config_asks_for_extra_mounts() {
        let global = config("sandbox:\n  extra_mounts: [/notes]\n");

        let project = project_in(
            &global.merge_for_test(config(OPENSHELL)),
            Path::new("/src/api"),
            Some("https://github.com/me/api.git"),
        )
        .unwrap();

        assert!(project.remote_config.openshell.unwrap().extra_mounts);
        let plain = openshell_at("/src/api").remote_config.openshell.unwrap();
        assert!(!plain.extra_mounts);
    }

    #[test]
    fn a_tagged_window_follows_its_clones_config_now() {
        const TEST_NAME: &str =
            "remote::resolve::tests::a_tagged_window_follows_its_clones_config_now";
        if !crate::test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp.path().join("config")).unwrap();
            crate::test_support::run_isolated_test(
                TEST_NAME,
                temp.path(),
                &[
                    ("XDG_CONFIG_HOME", &temp.path().join("config")),
                    ("HOME", temp.path()),
                ],
            );
            return;
        }
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);

        let home = std::env::current_dir().unwrap();
        let repo = home.join("api");
        std::fs::create_dir(&repo).unwrap();
        crate::test_support::init_repo(&repo);
        crate::test_support::run_git(
            &repo,
            &["remote", "add", "origin", "https://github.com/me/api.git"],
        );
        let configure = |yaml: &str| std::fs::write(repo.join(".workmux.yaml"), yaml).unwrap();
        configure(&format!("{OPENSHELL}  openshell:\n    image: one:1\n"));
        let project = at(&repo).unwrap().expect("in a sandbox");
        let tag = project.tag();
        let image = |project: Option<RemoteProject>| {
            project
                .expect("still in its sandbox")
                .remote_config
                .openshell
                .unwrap()
                .settings
                .image
        };
        assert_eq!(image(tagged(&tag).unwrap()).as_deref(), Some("one:1"));
        let open = || with_tags(std::slice::from_ref(&tag), open_projects);
        assert_eq!(open().len(), 1);

        // A changed image or policy applies at once.
        configure(&format!(
            "{OPENSHELL}  openshell:\n    image: two:2\n    policy: p.yaml\n"
        ));
        let changed = tagged(&tag).unwrap();
        assert_eq!(image(changed.clone()).as_deref(), Some("two:2"));
        assert_eq!(
            changed
                .unwrap()
                .remote_config
                .openshell
                .unwrap()
                .settings
                .policy,
            Some(
                repo.canonicalize()
                    .unwrap()
                    .join("p.yaml")
                    .display()
                    .to_string()
            )
        );

        // Back on the workstation: the window is no longer the project's,
        // so nothing reconciles its sandbox.
        configure("sandbox:\n  enabled: false\n");
        assert_eq!(tagged(&tag).unwrap(), None);
        assert!(open().is_empty());

        // A clone that is gone fails, saying so.
        std::fs::remove_dir_all(&repo).unwrap();
        let error = tagged(&tag).unwrap_err().to_string();
        assert!(error.contains("no longer exists"), "{error}");
    }
}
