use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::git;
use crate::multiplexer::{AgentPane, Multiplexer};
use crate::state::StateStore;
use crate::util::canon_or_self;

/// Parsed agent target selector.
enum AgentSelector {
    /// Plain name, resolved locally first then globally.
    Local(String),
    /// Qualified `project:handle` for cross-project targeting.
    Qualified { project: String, handle: String },
}

impl AgentSelector {
    fn parse(s: &str) -> Self {
        // Colon is invalid in git branch/ref names, so no collision with branch names
        if let Some((project, handle)) = s.split_once(':')
            && !project.is_empty()
            && !handle.is_empty()
        {
            return Self::Qualified {
                project: project.to_string(),
                handle: handle.to_string(),
            };
        }
        Self::Local(s.to_string())
    }
}

/// Walk up from `path` to find the containing worktree/repo root.
///
/// Git worktrees have a `.git` file; regular repos have a `.git` directory.
/// Returns `None` if no `.git` is found (e.g. path is outside any repo).
pub fn find_worktree_root(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Resolve a worktree name to its agent panes.
///
/// Resolution strategy:
/// 1. Parse the selector (`project:handle` or plain name)
/// 2. For plain names: try local git worktree first, fall back to global on
///    `WorktreeNotFound` or when not in a git repo
/// 3. For qualified names: go straight to global resolution
/// 4. Global resolution matches by worktree root directory name, with
///    disambiguation on ambiguity
///
/// Returns the worktree path and matching agent panes (may be empty if no agent is running).
pub fn resolve_worktree_agents(
    name: &str,
    mux: &dyn Multiplexer,
) -> Result<(PathBuf, Vec<AgentPane>)> {
    let agent_panes = StateStore::new().and_then(|store| store.load_reconciled_agents(mux))?;
    let name = targeted_name(name, crate::remote::router::resolve()?.as_ref());
    resolve_worktree_agents_from_snapshot(&name, &agent_panes)
}

/// `name` as a command run for `project` means it: a plain worktree name
/// typed for a remote project (in a clone of its repository or one of its
/// windows) names that project's worktree.
pub fn targeted_name(name: &str, project: Option<&crate::remote::router::RemoteProject>) -> String {
    match (AgentSelector::parse(name), project) {
        (AgentSelector::Local(handle), Some(project)) => {
            format!("{}:{handle}", project.project().group_label())
        }
        _ => name.to_string(),
    }
}

/// Resolve a worktree name against one reconciled agent snapshot.
pub fn resolve_worktree_agents_from_snapshot(
    name: &str,
    agent_panes: &[AgentPane],
) -> Result<(PathBuf, Vec<AgentPane>)> {
    match AgentSelector::parse(name) {
        AgentSelector::Qualified { project, handle } => {
            resolve_global_agents(agent_panes, &handle, Some(&project))
        }
        AgentSelector::Local(local_name) => {
            // Try local git resolution first
            let in_git_repo = git::get_repo_root_if_present()?.is_some();
            let local_result = if in_git_repo {
                match git::find_worktree(&local_name) {
                    Ok((worktree_path, _branch)) => {
                        Some(Ok(resolve_local_agents(agent_panes, &worktree_path)))
                    }
                    Err(e) if e.downcast_ref::<git::WorktreeNotFound>().is_some() => None,
                    Err(e) => Some(Err(e)),
                }
            } else {
                None
            };

            match local_result {
                Some(Ok(result)) => Ok(result),
                Some(Err(e)) => Err(e),
                None => resolve_global_agents(agent_panes, &local_name, None),
            }
        }
    }
}

/// Match agents against a known local worktree path.
fn resolve_local_agents(
    agent_panes: &[AgentPane],
    worktree_path: &Path,
) -> (PathBuf, Vec<AgentPane>) {
    let canon_wt_path = canon_or_self(worktree_path);
    let matching: Vec<AgentPane> = agent_panes
        .iter()
        .filter(|a| {
            let canon_agent_path = canon_or_self(&a.path);
            canon_agent_path == canon_wt_path || canon_agent_path.starts_with(&canon_wt_path)
        })
        .cloned()
        .collect();
    (worktree_path.to_path_buf(), matching)
}

/// Project qualifiers for an agent worktree.
pub struct AgentProject {
    name: Option<String>,
    parent_name: Option<String>,
}

impl AgentProject {
    pub fn for_worktree(root: &Path) -> Self {
        Self {
            name: git::project_identity(root).0,
            parent_name: root
                .parent()
                .and_then(|p| p.file_name())
                .map(|name| name.to_string_lossy().into_owned()),
        }
    }

    fn matches(&self, qualifier: &str) -> bool {
        // Parent-directory aliases participate equally so collisions cannot
        // silently redirect a command to a different agent.
        self.name.as_deref() == Some(qualifier) || self.parent_name.as_deref() == Some(qualifier)
    }

    /// Prefer the repository name; retain parent names when Git is unavailable.
    pub fn selector(&self, handle: &str) -> Option<String> {
        self.name
            .as_ref()
            .or(self.parent_name.as_ref())
            .map(|name| format!("{name}:{handle}"))
    }
}

/// The worktree an agent works in: a workstation directory, or a worktree
/// of a remote project. A remote agent's path is the remote's, so it is
/// keyed by its project host instead.
#[derive(PartialEq, Eq, Hash)]
enum AgentWorktree {
    Workstation(PathBuf),
    Remote {
        remote: String,
        project: String,
        worktree: String,
    },
}

impl AgentWorktree {
    fn of(agent: &AgentPane) -> Self {
        match &agent.remote {
            Some(remote) => Self::Remote {
                remote: remote.remote.clone(),
                project: remote.project.clone(),
                worktree: remote.worktree.clone(),
            },
            None => Self::Workstation(
                find_worktree_root(&agent.path).unwrap_or_else(|| agent.path.clone()),
            ),
        }
    }

    fn handle(&self) -> String {
        match self {
            Self::Workstation(root) => root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            Self::Remote { worktree, .. } => worktree.clone(),
        }
    }

    fn project(&self) -> AgentProject {
        match self {
            Self::Workstation(root) => AgentProject::for_worktree(root),
            Self::Remote {
                remote, project, ..
            } => AgentProject {
                name: Some(crate::project::Project::remote(remote, project).group_label()),
                parent_name: Some(project.clone()),
            },
        }
    }
}

/// Search all reconciled agents globally by worktree name.
///
/// Groups agents by their worktree. If `project` is provided, also filters
/// by the repository name or parent-directory alias (for a remote project,
/// its name or `project@remote`). Returns an error on ambiguity.
fn resolve_global_agents(
    agent_panes: &[AgentPane],
    handle: &str,
    project: Option<&str>,
) -> Result<(PathBuf, Vec<AgentPane>)> {
    let mut by_worktree: HashMap<AgentWorktree, Vec<&AgentPane>> = HashMap::new();
    for agent in agent_panes {
        let worktree = AgentWorktree::of(agent);
        if worktree.handle() == handle {
            by_worktree.entry(worktree).or_default().push(agent);
        }
    }

    // Resolve identity once per matching worktree, regardless of pane count.
    // A unique unqualified handle needs no project lookup.
    let mut groups: Vec<(AgentWorktree, Vec<&AgentPane>, Option<AgentProject>)> = by_worktree
        .into_iter()
        .map(|(worktree, agents)| (worktree, agents, None))
        .collect();
    if project.is_some() || groups.len() > 1 {
        for (worktree, _, identity) in &mut groups {
            *identity = Some(worktree.project());
        }
    }
    if let Some(qualifier) = project {
        groups.retain(|(_, _, identity)| identity.as_ref().unwrap().matches(qualifier));
    }

    // A remote worktree's root on the remote is the shallowest agent path.
    let root = |worktree: &AgentWorktree, agents: &[&AgentPane]| match worktree {
        AgentWorktree::Workstation(root) => root.clone(),
        AgentWorktree::Remote { .. } => agents
            .iter()
            .map(|agent| agent.path.clone())
            .min_by_key(|path| path.components().count())
            .unwrap_or_default(),
    };

    match groups.len() {
        0 => Err(anyhow!(
            "No agent found matching '{}'",
            format_selector(handle, project)
        )),
        1 => {
            let (worktree, agents, _) = groups.pop().unwrap();
            Ok((
                root(&worktree, &agents),
                agents.into_iter().cloned().collect(),
            ))
        }
        _ => {
            let mut options: Vec<String> = groups
                .iter()
                .map(|(worktree, agents, identity)| {
                    let selector = identity
                        .as_ref()
                        .and_then(|identity| identity.selector(handle))
                        .unwrap_or_else(|| handle.to_string());
                    format!("{} ({})", selector, root(worktree, agents).display())
                })
                .collect();
            options.sort();
            Err(anyhow!(
                "Ambiguous agent name '{}'. Found in multiple worktrees:\n  {}\n\nUse a unique 'project:handle' selector or run from the intended repository with a plain handle.",
                format_selector(handle, project),
                options.join("\n  ")
            ))
        }
    }
}

fn format_selector(handle: &str, project: Option<&str>) -> String {
    match project {
        Some(proj) => format!("{}:{}", proj, handle),
        None => handle.to_string(),
    }
}

/// Resolve a worktree name to exactly one agent pane (the first/primary).
///
/// Returns an error if no agent is running in the worktree.
pub fn resolve_worktree_agent(name: &str, mux: &dyn Multiplexer) -> Result<(PathBuf, AgentPane)> {
    let (path, agents) = resolve_worktree_agents(name, mux)?;
    let agent = agents
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No agent running in worktree '{}'", name))?;
    Ok((path, agent))
}

/// Match agents to a worktree path from a pre-loaded agent list.
///
/// Used by `status` and `wait` commands that load agents once and match
/// multiple worktrees, avoiding repeated calls to `load_reconciled_agents`.
pub fn match_agents_to_worktree<'a>(
    agents: &'a [AgentPane],
    worktree_path: &Path,
) -> Vec<&'a AgentPane> {
    let canon_wt = canon_or_self(worktree_path);
    agents
        .iter()
        .filter(|a| {
            let canon_agent = canon_or_self(&a.path);
            canon_agent == canon_wt || canon_agent.starts_with(&canon_wt)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{init_repo, run_git};

    fn agent_at(path: &Path, pane_id: &str) -> AgentPane {
        serde_json::from_value(serde_json::json!({
            "session": "test",
            "window_name": "wm-feature",
            "pane_id": pane_id,
            "path": path,
        }))
        .unwrap()
    }

    fn linked_worktree(repo: &Path, root: &Path) -> AgentPane {
        std::fs::create_dir_all(repo).unwrap();
        init_repo(repo);
        run_git(
            repo,
            &["worktree", "add", "-b", "feature", root.to_str().unwrap()],
        );
        let cwd = root.join("src");
        std::fs::create_dir(&cwd).unwrap();
        agent_at(&cwd, "%1")
    }

    #[test]
    fn qualified_selectors_use_git_project_identity_across_layouts() {
        for layout in ["quiver__worktrees", "quiver/.worktrees", "custom", "quiver"] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("quiver");
            let root = temp.path().join(layout).join("feature");
            let agent = linked_worktree(&repo, &root);
            let panes = [agent.clone(), agent_at(&root, "%2")];
            let (resolved, matched) =
                resolve_worktree_agents_from_snapshot("quiver:feature", &panes).unwrap();
            assert_eq!(resolved, root);
            assert_eq!(matched, panes);

            let project = AgentProject::for_worktree(&root);
            assert_eq!(
                project.selector("feature").as_deref(),
                Some("quiver:feature")
            );
            assert_eq!(
                git::project_identity(&agent.path).0.as_deref(),
                Some("quiver")
            );
            let alias = format!(
                "{}:feature",
                root.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            );
            assert_eq!(
                resolve_worktree_agents_from_snapshot(&alias, &panes)
                    .unwrap()
                    .0,
                root
            );
            assert_eq!(
                resolve_global_agents(&panes, "feature", None).unwrap().0,
                root
            );
            assert_eq!(
                resolve_worktree_agents_from_snapshot("wrong:feature", &panes)
                    .unwrap_err()
                    .to_string(),
                "No agent found matching 'wrong:feature'"
            );
        }
    }

    fn remote_agent(project: &str, worktree: &str, pane_id: &str) -> AgentPane {
        // The remote's directory, which does not exist on the workstation.
        let mut agent = agent_at(
            Path::new(&format!("/remote/{project}__worktrees/{worktree}")),
            pane_id,
        );
        agent.remote = Some(crate::multiplexer::RemoteAgentPane {
            remote: "devbox".to_string(),
            project: project.to_string(),
            worktree: worktree.to_string(),
            stale: false,
        });
        agent
    }

    #[test]
    fn a_plain_name_typed_for_a_remote_project_targets_that_project() {
        let panes = [
            remote_agent("api", "feature", "%1"),
            remote_agent("web", "feature", "%2"),
        ];
        let api = crate::remote::testing::devbox_api();

        let targeted = targeted_name("feature", Some(&api));
        let (_, matched) = resolve_worktree_agents_from_snapshot(&targeted, &panes).unwrap();
        assert_eq!(matched, [panes[0].clone()]);
        // A qualified name says which project it means; on the workstation
        // a plain name stays plain.
        assert_eq!(targeted_name("web:feature", Some(&api)), "web:feature");
        assert_eq!(targeted_name("feature", None), "feature");
    }

    #[test]
    fn remote_agents_resolve_by_their_remote_worktree_and_project() {
        let panes = [
            remote_agent("api", "feature", "%1"),
            remote_agent("web", "feature", "%2"),
            remote_agent("api", "fix", "%3"),
        ];

        let (root, matched) = resolve_worktree_agents_from_snapshot("fix", &panes).unwrap();
        assert_eq!(root, PathBuf::from("/remote/api__worktrees/fix"));
        assert_eq!(matched, [panes[2].clone()]);
        for selector in ["api:feature", "api@devbox:feature"] {
            let (_, matched) = resolve_worktree_agents_from_snapshot(selector, &panes).unwrap();
            assert_eq!(matched, [panes[0].clone()], "{selector}");
        }
        let error = resolve_worktree_agents_from_snapshot("feature", &panes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("api@devbox:feature"), "{error}");
        assert!(error.contains("web@devbox:feature"), "{error}");
        assert!(resolve_worktree_agents_from_snapshot("api@other:feature", &panes).is_err());
    }

    #[test]
    fn main_worktree_uses_its_own_project_name() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("quiver");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let panes = [agent_at(&repo, "%1")];
        assert_eq!(
            resolve_worktree_agents_from_snapshot("quiver:quiver", &panes)
                .unwrap()
                .0,
            repo
        );
    }

    #[test]
    fn ambiguity_suggestions_use_project_names_and_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha/.worktrees/feature");
        let beta = temp.path().join("beta/.worktrees/feature");
        let panes = [
            linked_worktree(&temp.path().join("alpha"), &alpha),
            linked_worktree(&temp.path().join("beta"), &beta),
        ];
        let error = resolve_global_agents(&panes, "feature", None)
            .unwrap_err()
            .to_string();
        for (selector, root) in [("alpha:feature", alpha), ("beta:feature", beta)] {
            assert!(error.contains(selector), "{error}");
            assert_eq!(
                resolve_worktree_agents_from_snapshot(selector, &panes)
                    .unwrap()
                    .0,
                root
            );
        }
        assert!(!error.contains(".worktrees:feature"), "{error}");
        assert!(
            resolve_worktree_agents_from_snapshot(".worktrees:feature", &panes)
                .unwrap_err()
                .to_string()
                .contains("Ambiguous")
        );
    }

    #[test]
    fn alias_collision_cannot_silently_select_an_agent() {
        let temp = tempfile::tempdir().unwrap();
        let panes = [
            linked_worktree(
                &temp.path().join("alpha"),
                &temp.path().join("beta/feature"),
            ),
            linked_worktree(
                &temp.path().join("other/beta"),
                &temp.path().join("other/trees/feature"),
            ),
        ];
        let error = resolve_worktree_agents_from_snapshot("beta:feature", &panes)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Ambiguous agent name 'beta:feature'"),
            "{error}"
        );
        assert!(error.contains("alpha:feature"), "{error}");
    }

    #[test]
    fn duplicate_project_names_report_distinct_paths() {
        let temp = tempfile::tempdir().unwrap();
        let roots = [
            temp.path().join("one/trees/feature"),
            temp.path().join("two/trees/feature"),
        ];
        let panes = [
            linked_worktree(&temp.path().join("one/quiver"), &roots[0]),
            linked_worktree(&temp.path().join("two/quiver"), &roots[1]),
        ];
        let error = resolve_worktree_agents_from_snapshot("quiver:feature", &panes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Ambiguous"), "{error}");
        for root in roots {
            assert!(
                error.contains(&format!("quiver:feature ({})", root.display())),
                "{error}"
            );
        }
        assert!(
            error.contains("run from the intended repository"),
            "{error}"
        );
    }

    #[test]
    fn git_identity_failure_preserves_parent_alias() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("custom/feature");
        std::fs::create_dir_all(&root).unwrap();
        let panes = [agent_at(&root, "%1")];
        assert_eq!(git::project_identity(&root), (None, None));
        assert_eq!(
            AgentProject::for_worktree(&root)
                .selector("feature")
                .as_deref(),
            Some("custom:feature")
        );
        assert_eq!(
            resolve_worktree_agents_from_snapshot("custom:feature", &panes)
                .unwrap()
                .0,
            root
        );
    }

    #[test]
    fn bare_repository_identity_matches_status() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let root = temp.path().join("trees/feature");
        linked_worktree(&source, &root);
        let bare = temp.path().join(".bare");
        run_git(
            temp.path(),
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let linked = temp.path().join("bare-trees/feature");
        run_git(
            &bare,
            &["worktree", "add", linked.to_str().unwrap(), "feature"],
        );
        let panes = [agent_at(&linked, "%1")];
        assert_eq!(git::project_identity(&linked).0.as_deref(), Some(".bare"));
        assert_eq!(
            resolve_worktree_agents_from_snapshot(".bare:feature", &panes)
                .unwrap()
                .0,
            linked
        );
    }
}
