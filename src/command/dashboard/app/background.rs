//! Background thread spawning for git and GitHub status fetches.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::github::RepoSource as PrSource;
use crate::project::Project;
use crate::remote::dashboard::repo_source as pr_source;

use super::super::site::WorktreeSite;
use super::App;
use super::types::AppEvent;

/// Group branches by repository identity while retaining where to ask gh.
fn group_pr_requests<'a>(
    requests: impl IntoIterator<Item = (&'a PathBuf, PrSource, &'a str)>,
) -> HashMap<PathBuf, (PrSource, Vec<String>)> {
    let mut grouped: HashMap<PathBuf, (PrSource, Vec<String>)> = HashMap::new();
    for (key, source, branch) in requests {
        let (_, branches) = grouped
            .entry(key.clone())
            .or_insert_with(|| (source, Vec::new()));
        branches.push(branch.to_string());
    }
    for (_, branches) in grouped.values_mut() {
        branches.sort();
        branches.dedup();
    }
    grouped
}

impl App {
    /// Spawn a background thread to fetch git status for all agent worktrees
    pub(super) fn spawn_git_status_fetch(&self) {
        // Skip if a fetch is already in progress (prevents thread pile-up)
        if self
            .is_git_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let tx = self.event_tx.clone();
        let is_fetching = self.is_git_fetching.clone();
        let main_branch = self.config.main_branch.clone();
        // Include both agent and worktree view worktrees so the worktree view gets git status too
        let mut sites: Vec<WorktreeSite> = self.all_agents.iter().map(WorktreeSite::of).collect();
        for wt in &self.worktrees {
            let site = self.worktree_site(&wt.path);
            if !sites.contains(&site) {
                sites.push(site);
            }
        }

        std::thread::spawn(move || {
            // Reset flag when thread completes (even on panic)
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            for site in sites {
                // A remote worktree's own config names its main branch.
                let main_branch = (!site.is_remote())
                    .then_some(main_branch.as_deref())
                    .flatten();
                if let Some(status) = site.git_status(main_branch) {
                    let _ = tx.send(AppEvent::GitStatus(site.status_key(), status));
                }
            }
        });
    }

    /// Spawn a background thread to fetch GitHub status for all repos.
    /// Returns true if a fetch was started, false if one is already in progress.
    pub(super) fn spawn_pr_status_fetch(&self) -> bool {
        // Skip if already fetching
        if self
            .is_pr_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }

        // Use unfiltered snapshots so filtering cannot omit branches from a fetch.
        let worktree_requests = self.all_worktrees.iter().filter_map(|wt| {
            let source = match &self.worktree_project_override {
                Some(scope) => scope.repo_source(&self.config, &wt.path)?,
                None => PrSource::Checkout(wt.path.clone()),
            };
            Some((&wt.repo_key, source, wt.branch.as_str()))
        });
        let agent_requests = self.all_agents.iter().filter_map(|agent| {
            let site = WorktreeSite::of(agent).status_key();
            let branch = self.git_statuses.get(&site)?.branch.as_deref()?;
            let key = self.repo_keys.get(&site)?;
            Some((key, pr_source(&self.config, agent)?, branch))
        });
        let repo_branches = group_pr_requests(worktree_requests.chain(agent_requests));

        if repo_branches.is_empty() {
            self.is_pr_fetching.store(false, Ordering::SeqCst);
            // Return false so the caller doesn't reset the timer — no fetch
            // was actually started, and we want to retry once git statuses
            // or worktree data arrive.
            return false;
        }

        let tx = self.event_tx.clone();
        let is_fetching = self.is_pr_fetching.clone();

        let priority_path = match &self.worktree_project_override {
            Some(super::super::site::ProjectScope::Workstation(path)) => Some(path.clone()),
            Some(super::super::site::ProjectScope::Remote { .. }) => None,
            None => self.current_worktree.clone(),
        };

        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            let priority_repo = priority_path
                .as_deref()
                .and_then(|path| Project::of_workstation_path(path).cache_key(path).ok());

            // Sort repos so the priority repo (current project) is fetched first
            let mut repos: VecDeque<_> = repo_branches.into_iter().collect();
            if let Some(ref priority) = priority_repo {
                repos
                    .make_contiguous()
                    .sort_by_key(|(repo, _)| repo != priority);
            }

            // Fetch repos in parallel with bounded concurrency
            let queue = Arc::new(Mutex::new(repos));
            let workers = queue.lock().unwrap().len().min(4);

            std::thread::scope(|s| {
                for _ in 0..workers {
                    let queue = Arc::clone(&queue);
                    let tx = tx.clone();
                    s.spawn(move || {
                        loop {
                            let Some((repo_key, (source, branches))) =
                                queue.lock().unwrap().pop_front()
                            else {
                                break;
                            };
                            let summaries =
                                crate::github::list_branch_summaries_from(&source, &branches);
                            match summaries {
                                Ok(summaries) => {
                                    let _ = tx.send(AppEvent::GithubStatus(repo_key, summaries));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to fetch GitHub status for {:?}: {}",
                                        source,
                                        e
                                    );
                                }
                            }
                        }
                    });
                }
            });
        });

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(path: &std::path::Path) -> PrSource {
        PrSource::Checkout(path.to_path_buf())
    }

    #[test]
    fn requests_use_stable_keys_and_checkout_paths_without_main_rows() {
        let key = PathBuf::from("/project/.git");
        let first = PathBuf::from("/project/worktrees/branch-1");
        let second = PathBuf::from("/project/worktrees/branch-2");
        let agent = second.join("src");
        let grouped = group_pr_requests([
            (&key, checkout(&first), "branch-1"),
            (&key, checkout(&second), "branch-2"),
            (&key, checkout(&agent), "branch-2"),
        ]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(
            grouped[&key],
            (checkout(&first), vec!["branch-1".into(), "branch-2".into()])
        );
        let refreshed = group_pr_requests([(&key, checkout(&second), "branch-2")]);
        assert_eq!(
            refreshed[&key],
            (checkout(&second), vec!["branch-2".into()])
        );
    }

    #[test]
    fn same_named_repositories_have_separate_requests() {
        let key_a = PathBuf::from("/a/project/.git");
        let key_b = PathBuf::from("/b/project/.git");
        let checkout_a = PathBuf::from("/a/project/worktrees/feature");
        let checkout_b = PathBuf::from("/b/project/worktrees/feature");
        let grouped = group_pr_requests([
            (&key_a, checkout(&checkout_a), "feature"),
            (&key_b, checkout(&checkout_b), "feature"),
        ]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&key_a].0, checkout(&checkout_a));
        assert_eq!(grouped[&key_b].0, checkout(&checkout_b));
    }
}
