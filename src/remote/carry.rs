//! Carrying a project's committed work across a sandbox recreate (decision
//! C3). A recreate loses the sandbox's workspace, which may hold the only
//! copy of the work, so before it: refuse while any worktree has
//! uncommitted changes; bundle every ref with `git bundle create --all` and
//! copy the bundle out over exec. After it: clone from the bundle, restore
//! every local branch and its upstream, re-add each worktree on its branch
//! at its old path, and restore the origin remote URL.
//!
//! The git work runs in remote workmux (`workmux remote carry …`); the
//! workstation moves the bundle and drives the steps through the transport.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::transport::Transport;

/// A worktree or branch whose work exists only on the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtRisk {
    /// The worktree's path, or `None` for a branch no worktree has checked out.
    pub path: Option<PathBuf>,
    pub branch: Option<String>,
    /// Uncommitted or untracked changes.
    pub uncommitted: bool,
    /// Commits on no remote-tracking branch: unpushed, and unmerged into
    /// anything pushed.
    pub unpushed: usize,
}

impl AtRisk {
    /// One line naming the worktree or branch and what it would lose.
    pub fn describe(&self) -> String {
        let name = match (&self.path, &self.branch) {
            (Some(path), Some(branch)) => format!("{} (branch {branch})", path.display()),
            (Some(path), None) => format!("{} (detached)", path.display()),
            (None, Some(branch)) => format!("branch {branch}"),
            (None, None) => "?".to_string(),
        };
        let mut what = Vec::new();
        if self.uncommitted {
            what.push("uncommitted changes".to_string());
        }
        match self.unpushed {
            0 => {}
            1 => what.push("1 commit not on any remote".to_string()),
            n => what.push(format!("{n} commits not on any remote")),
        }
        format!("{name}: {}", what.join(", "))
    }
}

/// What a recreate restores besides the bundle's objects and refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The clone's absolute path on the remote.
    pub clone: PathBuf,
    pub origin_url: Option<String>,
    /// `refs/remotes/origin/HEAD`'s target, e.g. `refs/remotes/origin/main`.
    pub origin_head: Option<String>,
    /// The clone's checked-out branch, or its commit when detached.
    pub head: Head,
    /// Each linked worktree, at its path.
    pub worktrees: Vec<Worktree>,
    /// Each local branch's upstream, e.g. `feature` → `origin/feature`.
    pub upstreams: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Head {
    Branch(String),
    Detached(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Head,
}

/// Run git in `dir`, returning its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = crate::git::unattended_git(None)?
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git {} in {} failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// `git worktree list --porcelain`: each worktree's path and head, the
/// clone's first.
fn worktrees(clone: &Path) -> Result<Vec<Worktree>> {
    let listing = git(clone, &["worktree", "list", "--porcelain"])?;
    let mut worktrees = Vec::new();
    for block in listing.split("\n\n") {
        let mut path = None;
        let mut commit = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                commit = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
                branch = Some(value.to_string());
            }
        }
        let (Some(path), Some(commit)) = (path, commit) else {
            continue;
        };
        worktrees.push(Worktree {
            path,
            head: branch.map_or(Head::Detached(commit), Head::Branch),
        });
    }
    Ok(worktrees)
}

fn local_branches(clone: &Path) -> Result<Vec<String>> {
    Ok(git(
        clone,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?
    .lines()
    .map(str::to_string)
    .collect())
}

fn unpushed(clone: &Path, rev: &str) -> Result<usize> {
    let count = git(clone, &["rev-list", "--count", rev, "--not", "--remotes"])?;
    count
        .trim()
        .parse()
        .with_context(|| format!("unreadable commit count {count:?}"))
}

/// Every worktree and branch of the clone at `clone` whose work exists
/// only here.
pub fn at_risk(clone: &Path) -> Result<Vec<AtRisk>> {
    let worktrees = worktrees(clone)?;
    let mut risks = Vec::new();
    let mut checked_out = Vec::new();
    for worktree in &worktrees {
        let (branch, rev) = match &worktree.head {
            Head::Branch(branch) => (Some(branch.clone()), format!("refs/heads/{branch}")),
            Head::Detached(commit) => (None, commit.clone()),
        };
        checked_out.extend(branch.clone());
        let uncommitted = !git(&worktree.path, &["status", "--porcelain"])?.is_empty();
        let unpushed = unpushed(clone, &rev)?;
        if uncommitted || unpushed > 0 {
            risks.push(AtRisk {
                path: Some(worktree.path.clone()),
                branch,
                uncommitted,
                unpushed,
            });
        }
    }
    for branch in local_branches(clone)? {
        if checked_out.contains(&branch) {
            continue;
        }
        let unpushed = unpushed(clone, &format!("refs/heads/{branch}"))?;
        if unpushed > 0 {
            risks.push(AtRisk {
                path: None,
                branch: Some(branch),
                uncommitted: false,
                unpushed,
            });
        }
    }
    Ok(risks)
}

/// What [`restore`] needs to rebuild the clone at `clone`.
pub fn manifest(clone: &Path) -> Result<Manifest> {
    let mut worktrees = worktrees(clone)?;
    if worktrees.is_empty() {
        bail!("{} has no worktrees", clone.display());
    }
    let main = worktrees.remove(0);
    let mut upstreams = BTreeMap::new();
    for branch in local_branches(clone)? {
        if let Ok(upstream) = git(
            clone,
            &[
                "rev-parse",
                "--abbrev-ref",
                &format!("{branch}@{{upstream}}"),
            ],
        ) {
            upstreams.insert(branch, upstream);
        }
    }
    Ok(Manifest {
        clone: main.path,
        origin_url: git(clone, &["remote", "get-url", "origin"]).ok(),
        origin_head: git(clone, &["symbolic-ref", "refs/remotes/origin/HEAD"]).ok(),
        head: main.head,
        worktrees,
        upstreams,
    })
}

/// Rebuild the clone described by `manifest` from `bundle`. The clone's
/// path must not exist yet.
pub fn restore(bundle: &Path, manifest: &Manifest) -> Result<()> {
    let clone = &manifest.clone;
    if clone.exists() {
        bail!(
            "{} already exists; a restore needs a fresh remote",
            clone.display()
        );
    }
    std::fs::create_dir_all(clone)
        .with_context(|| format!("failed to create {}", clone.display()))?;
    git(clone, &["init", "--quiet"])?;
    let bundle = bundle.to_string_lossy();
    git(
        clone,
        &[
            "fetch",
            "--quiet",
            "--update-head-ok",
            &bundle,
            "refs/heads/*:refs/heads/*",
            "refs/remotes/*:refs/remotes/*",
            "refs/tags/*:refs/tags/*",
        ],
    )?;
    match &manifest.head {
        Head::Branch(branch) => git(clone, &["checkout", "--quiet", "-f", branch])?,
        Head::Detached(commit) => git(clone, &["checkout", "--quiet", "-f", "--detach", commit])?,
    };
    if let Some(url) = &manifest.origin_url {
        git(clone, &["remote", "add", "origin", url])?;
    }
    if let Some(target) = &manifest.origin_head {
        git(clone, &["symbolic-ref", "refs/remotes/origin/HEAD", target])?;
    }
    for (branch, upstream) in &manifest.upstreams {
        git(
            clone,
            &["branch", "--quiet", "--set-upstream-to", upstream, branch],
        )?;
    }
    for worktree in &manifest.worktrees {
        let path = worktree.path.to_string_lossy();
        match &worktree.head {
            Head::Branch(branch) => git(clone, &["worktree", "add", "--quiet", &path, branch])?,
            Head::Detached(commit) => git(
                clone,
                &["worktree", "add", "--quiet", "--detach", &path, commit],
            )?,
        };
    }
    Ok(())
}

/// The bundle of every ref of the clone at `clone`.
pub fn bundle(clone: &Path) -> Result<Vec<u8>> {
    let output = crate::git::unattended_git(None)?
        .arg("-C")
        .arg(clone)
        .args(["bundle", "create", "--quiet", "-", "--all"])
        .stdin(std::process::Stdio::null())
        .output()
        .context("failed to run git bundle")?;
    if !output.status.success() {
        bail!(
            "git bundle in {} failed: {}",
            clone.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// `workmux remote carry …` on the remote: `at-risk` and `manifest` print
/// JSON, `bundle` prints the bundle, and `restore` reads one from stdin.
pub fn serve(action: &str, path: &str, manifest_json: Option<&str>) -> Result<()> {
    let clone = super::session::clone_path(path)?;
    let mut stdout = std::io::stdout().lock();
    match action {
        "at-risk" => serde_json::to_writer(&mut stdout, &at_risk(&clone)?)?,
        "manifest" => serde_json::to_writer(&mut stdout, &manifest(&clone)?)?,
        "bundle" => stdout.write_all(&bundle(&clone)?)?,
        "restore" => {
            let manifest: Manifest = serde_json::from_str(
                manifest_json.ok_or_else(|| anyhow!("restore needs a manifest"))?,
            )
            .context("unreadable manifest")?;
            let parent = manifest
                .clone
                .parent()
                .ok_or_else(|| anyhow!("{} has no parent", manifest.clone.display()))?
                .to_path_buf();
            std::fs::create_dir_all(&parent)?;
            let mut file = tempfile::NamedTempFile::new_in(&parent)?;
            std::io::copy(&mut std::io::stdin().lock(), &mut file)?;
            file.flush()?;
            restore(file.path(), &manifest)?;
        }
        other => bail!("unknown carry action {other}"),
    }
    stdout.flush()?;
    Ok(())
}

/// Run `workmux remote carry <action> <path> [manifest]` on the remote.
fn carry(
    transport: &dyn Transport,
    action: &str,
    path: &str,
    extra: Option<&str>,
    input: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut argv: Vec<String> = ["workmux", "remote", "carry", action, path]
        .map(str::to_string)
        .to_vec();
    argv.extend(extra.map(str::to_string));
    let output = match input {
        Some(input) => transport.exec_with_input(&argv, input)?,
        None => transport.exec_captured(&argv)?,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "workmux remote carry {action} failed ({}): {stderr}",
            output.status
        );
    }
    Ok(output.stdout)
}

/// The project at `path` on the remote: its work that exists only there.
pub fn remote_at_risk(transport: &dyn Transport, path: &str) -> Result<Vec<AtRisk>> {
    serde_json::from_slice(&carry(transport, "at-risk", path, None, None)?)
        .context("remote workmux sent an unreadable list of worktrees")
}

/// Recreate the remote behind `transport` with `rebuild` (delete it and
/// create it anew), carrying the committed work of the project at `path`
/// across. The bundle and manifest are kept in `keep` until the work is
/// back, so a failed recreate loses nothing.
pub fn recreate(
    transport: &dyn Transport,
    path: &str,
    keep: &Path,
    rebuild: &mut dyn FnMut() -> Result<()>,
    notice: &mut dyn FnMut(&str),
) -> Result<()> {
    let uncommitted: Vec<String> = remote_at_risk(transport, path)?
        .into_iter()
        .filter(|risk| risk.uncommitted)
        .map(|risk| risk.describe())
        .collect();
    if !uncommitted.is_empty() {
        bail!(
            "these worktrees have uncommitted changes, which a recreate would lose; commit or discard them first:\n  {}",
            uncommitted.join("\n  ")
        );
    }
    notice("Bundling every branch of the project…");
    let manifest = carry(transport, "manifest", path, None, None)?;
    let bundle = carry(transport, "bundle", path, None, None)?;
    std::fs::create_dir_all(keep)?;
    let (bundle_file, manifest_file) = (keep.join("work.bundle"), keep.join("manifest.json"));
    std::fs::write(&bundle_file, &bundle)?;
    std::fs::write(&manifest_file, &manifest)?;
    let kept = || {
        format!(
            "the project's work is kept in {} (manifest {})",
            bundle_file.display(),
            manifest_file.display()
        )
    };
    rebuild().with_context(kept)?;
    notice("Restoring every branch and worktree…");
    let manifest = String::from_utf8(manifest).context("unreadable manifest")?;
    carry(transport, "restore", path, Some(&manifest), Some(bundle)).with_context(kept)?;
    std::fs::remove_dir_all(keep)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::transport::{StdioProcess, TtyExec};
    use crate::test_support::{init_repo, run_git, run_git_output};
    use std::process::{Command, Output};

    /// A project as remote workmux would find it: a clone of `origin`, with
    /// worktrees beside it.
    struct Project {
        _dir: tempfile::TempDir,
        clone: PathBuf,
        origin: PathBuf,
    }

    impl Project {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let origin = root.join("origin");
            std::fs::create_dir(&origin).unwrap();
            init_repo(&origin);
            let clone = root.join("api");
            run_git(
                &root,
                &["clone", "--quiet", origin.to_str().unwrap(), "api"],
            );
            run_git(&clone, &["config", "user.email", "test@example.com"]);
            run_git(&clone, &["config", "user.name", "Test User"]);
            Self {
                _dir: dir,
                clone,
                origin,
            }
        }

        fn worktree(&self, branch: &str) -> PathBuf {
            let path = self.clone.with_file_name("api__worktrees").join(branch);
            run_git(
                &self.clone,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
            path
        }

        fn commit(&self, worktree: &Path, file: &str) {
            std::fs::write(worktree.join(file), file).unwrap();
            run_git(worktree, &["add", file]);
            run_git(worktree, &["commit", "--quiet", "-m", file]);
        }
    }

    #[test]
    fn work_only_on_the_remote_is_named_with_what_it_would_lose() {
        let project = Project::new();
        let feature = project.worktree("feature");
        project.commit(&feature, "a.txt");
        project.commit(&feature, "b.txt");
        let dirty = project.worktree("dirty");
        std::fs::write(dirty.join("notes.txt"), "draft").unwrap();
        project.worktree("pushed");
        run_git(&project.clone, &["branch", "--quiet", "parked"]);
        run_git(&project.clone, &["branch", "--quiet", "spike", "feature"]);

        let risks: Vec<String> = at_risk(&project.clone)
            .unwrap()
            .iter()
            .map(AtRisk::describe)
            .collect();

        assert_eq!(
            risks,
            [
                format!("{} (branch dirty): uncommitted changes", dirty.display()),
                format!(
                    "{} (branch feature): 2 commits not on any remote",
                    feature.display()
                ),
                "branch spike: 2 commits not on any remote".to_string(),
            ]
        );
    }

    #[test]
    fn a_restore_brings_back_every_branch_worktree_and_the_origin() {
        let project = Project::new();
        let feature = project.worktree("feature");
        project.commit(&feature, "a.txt");
        run_git(&feature, &["push", "--quiet", "-u", "origin", "feature"]);
        project.commit(&feature, "b.txt");
        run_git(&project.clone, &["branch", "--quiet", "spike", "feature"]);
        let detached = project.clone.with_file_name("api__worktrees").join("look");
        run_git(
            &project.clone,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                detached.to_str().unwrap(),
                "HEAD",
            ],
        );
        let before = manifest(&project.clone).unwrap();
        let bundle_bytes = bundle(&project.clone).unwrap();
        let feature_head = run_git_output(&feature, &["rev-parse", "HEAD"]);

        // The recreate: everything in the old workspace is gone.
        std::fs::remove_dir_all(&project.clone).unwrap();
        std::fs::remove_dir_all(project.clone.with_file_name("api__worktrees")).unwrap();
        let bundle_file = project.origin.with_file_name("work.bundle");
        std::fs::write(&bundle_file, bundle_bytes).unwrap();
        restore(&bundle_file, &before).unwrap();

        assert_eq!(manifest(&project.clone).unwrap(), before);
        assert_eq!(
            run_git_output(&feature, &["rev-parse", "HEAD"]),
            feature_head
        );
        assert_eq!(
            std::fs::read_to_string(feature.join("b.txt")).unwrap(),
            "b.txt"
        );
        assert_eq!(
            run_git_output(&project.clone, &["rev-parse", "spike"]),
            feature_head
        );
        assert_eq!(
            run_git_output(&project.clone, &["remote", "get-url", "origin"]),
            project.origin.to_string_lossy()
        );
        // Pushed work still counts as pushed: the remote-tracking refs came back.
        let risks = at_risk(&project.clone).unwrap();
        assert_eq!(risks.len(), 2, "{risks:?}");
        assert!(risks.iter().all(|risk| risk.unpushed == 1), "{risks:?}");
    }

    /// Runs `workmux remote carry …` in this process, against paths on
    /// this machine, as remote workmux would on the remote.
    struct InProcess;

    impl InProcess {
        fn run(argv: &[String], input: Option<Vec<u8>>) -> Result<Output> {
            assert_eq!(argv[..3], ["workmux", "remote", "carry"]);
            let clone = PathBuf::from(&argv[4]);
            let stdout = match argv[3].as_str() {
                "at-risk" => serde_json::to_vec(&at_risk(&clone)?)?,
                "manifest" => serde_json::to_vec(&manifest(&clone)?)?,
                "bundle" => bundle(&clone)?,
                "restore" => {
                    let manifest: Manifest = serde_json::from_str(&argv[5])?;
                    let file = tempfile::NamedTempFile::new()?;
                    std::fs::write(file.path(), input.unwrap())?;
                    restore(file.path(), &manifest)?;
                    Vec::new()
                }
                other => panic!("unexpected {other}"),
            };
            Ok(Output {
                status: std::os::unix::process::ExitStatusExt::from_raw(0),
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    impl Transport for InProcess {
        fn exec_captured(&self, argv: &[String]) -> Result<Output> {
            Self::run(argv, None)
        }
        fn exec_with_input(&self, argv: &[String], input: Vec<u8>) -> Result<Output> {
            Self::run(argv, Some(input))
        }
        fn exec_stdio(&self, _: &[String]) -> Result<StdioProcess> {
            unimplemented!()
        }
        fn tty_command(&self, _: &TtyExec) -> Result<Command> {
            unimplemented!()
        }
    }

    #[test]
    fn a_recreate_carries_committed_work_across() {
        let project = Project::new();
        let feature = project.worktree("feature");
        project.commit(&feature, "a.txt");
        let head = run_git_output(&feature, &["rev-parse", "HEAD"]);
        let keep = project.origin.with_file_name("kept");
        let worktrees = project.clone.with_file_name("api__worktrees");

        recreate(
            &InProcess,
            project.clone.to_str().unwrap(),
            &keep,
            &mut || {
                std::fs::remove_dir_all(&project.clone)?;
                std::fs::remove_dir_all(&worktrees)?;
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(run_git_output(&feature, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            run_git_output(&feature, &["branch", "--show-current"]),
            "feature"
        );
        assert!(!keep.exists());
    }

    #[test]
    fn a_recreate_refuses_while_a_worktree_has_uncommitted_changes() {
        let project = Project::new();
        let feature = project.worktree("feature");
        std::fs::write(feature.join("draft.txt"), "draft").unwrap();
        let mut rebuilt = false;

        let error = recreate(
            &InProcess,
            project.clone.to_str().unwrap(),
            &project.origin.with_file_name("kept"),
            &mut || {
                rebuilt = true;
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap_err();

        assert!(!rebuilt);
        let error = error.to_string();
        assert!(error.contains("uncommitted changes"), "{error}");
        assert!(
            error.contains(&format!("{} (branch feature)", feature.display())),
            "{error}"
        );
    }

    #[test]
    fn a_failed_rebuild_keeps_the_bundle_and_says_where() {
        let project = Project::new();
        let keep = project.origin.with_file_name("kept");

        let error = recreate(
            &InProcess,
            project.clone.to_str().unwrap(),
            &keep,
            &mut || bail!("create failed"),
            &mut |_| {},
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("kept in"), "{error:#}");
        assert!(keep.join("work.bundle").is_file());
    }
}
