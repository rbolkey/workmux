//! Remote panes: terminals the session holder keeps alive on a remote
//! (ADR 0003). A workstation pane is only a view onto one, attached through
//! the transport's TTY exec.
//!
//! The session holder is dtach (ticket 02): one `dtach -n` master per remote
//! pane, its socket at `<state dir>/panes/<project>/<worktree>/<slot>.sock`.
//! A remote pane is named by its project, worktree and pane slot, written
//! `<project>/<worktree>/<slot>`; the name is also its socket's path under
//! the panes directory, and the inner process sees it as
//! `WORKMUX_REMOTE_PANE`.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

/// Environment naming the remote pane a process runs in.
pub const PANE_ENV: &str = "WORKMUX_REMOTE_PANE";

/// Longest socket path dtach can bind: Unix socket addresses hold about 104
/// bytes on macOS and 108 on Linux.
const MAX_SOCKET_PATH: usize = 100;

/// Terminal type for a remote pane whose creator had none: the holder starts
/// before any workstation terminal attaches.
const DEFAULT_TERM: &str = "xterm-256color";

/// A remote pane's identity: project, worktree and pane slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePaneId {
    pub project: String,
    pub worktree: String,
    pub slot: u32,
}

impl fmt::Display for RemotePaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.project, self.worktree, self.slot)
    }
}

impl FromStr for RemotePaneId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = || anyhow!("invalid remote pane {s:?}: expected <project>/<worktree>/<slot>");
        let mut parts = s.split('/');
        let (Some(project), Some(worktree), Some(slot), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        if !is_component(project) || !is_component(worktree) {
            return Err(invalid());
        }
        Ok(Self {
            project: project.to_string(),
            worktree: worktree.to_string(),
            slot: slot.parse().map_err(|_| invalid())?,
        })
    }
}

impl Serialize for RemotePaneId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RemotePaneId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A path component that stays inside its parent directory.
fn is_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\0'])
}

/// Whether a remote pane still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Running,
    Ended,
}

impl PaneState {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneState::Running => "running",
            PaneState::Ended => "ended",
        }
    }
}

impl FromStr for PaneState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "running" => Ok(PaneState::Running),
            "ended" => Ok(PaneState::Ended),
            other => Err(anyhow!("unexpected remote pane state {other:?}")),
        }
    }
}

/// The session holder on this machine, the remote: creates, checks, ends
/// and attaches to remote panes kept under one directory.
pub struct SessionHolder {
    dir: PathBuf,
}

impl SessionHolder {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The session holder for this remote's panes, under its state dir.
    pub fn for_this_remote() -> Result<Self> {
        Ok(Self::new(crate::xdg::state_dir()?.join("panes")))
    }

    fn socket(&self, id: &RemotePaneId) -> PathBuf {
        self.worktree_dir(&id.project, &id.worktree)
            .join(format!("{}.sock", id.slot))
    }

    fn pid_file(&self, id: &RemotePaneId) -> PathBuf {
        self.worktree_dir(&id.project, &id.worktree)
            .join(format!("{}.pid", id.slot))
    }

    fn worktree_dir(&self, project: &str, worktree: &str) -> PathBuf {
        self.dir.join(project).join(worktree)
    }

    /// Start `argv` in a new remote pane for `project`, in `workdir`.
    /// The pane's worktree is the git worktree holding `workdir`.
    pub fn create(&self, project: &str, workdir: &Path, argv: &[String]) -> Result<RemotePaneId> {
        if argv.is_empty() {
            bail!("no command for the remote pane");
        }
        let id = self.allocate(project, &worktree_name(workdir))?;
        let socket = self.socket(&id);
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            let _ = std::fs::remove_file(self.pid_file(&id));
            bail!(
                "remote pane socket path {} is too long for a Unix socket",
                socket.display()
            );
        }
        let mut command = Command::new("dtach");
        // The inner shell records its pid (the command's, after exec) so
        // the pane can be ended.
        command
            .arg("-n")
            .arg(&socket)
            .args(["-E", "sh", "-c", "echo $$ > \"$0\"; exec \"$@\""])
            .arg(self.pid_file(&id))
            .args(argv)
            .current_dir(workdir)
            .env(PANE_ENV, id.to_string())
            .env_remove(super::forwarding::SOCKET_ENV)
            .env_remove(super::forwarding::SESSION_ENV)
            // The holder outlives this process: it must not hold our stdio.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if std::env::var("TERM").map_or(true, |term| term.is_empty() || term == "dumb") {
            command.env("TERM", DEFAULT_TERM);
        }
        let status = command.status();
        let failure = match status {
            Ok(status) if status.success() => return Ok(id),
            Ok(status) => anyhow!("the session holder (dtach) exited with {status}"),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                anyhow!("the session holder dtach is not installed on this remote")
            }
            Err(error) => anyhow::Error::from(error).context("failed to run dtach"),
        };
        let _ = std::fs::remove_file(self.pid_file(&id));
        Err(failure)
    }

    /// Reserve the lowest free slot in `project`'s `worktree`.
    fn allocate(&self, project: &str, worktree: &str) -> Result<RemotePaneId> {
        let dir = self.worktree_dir(project, worktree);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        for slot in 0.. {
            let id = RemotePaneId {
                project: project.to_string(),
                worktree: worktree.to_string(),
                slot,
            };
            if self.socket(&id).exists() {
                continue;
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.pid_file(&id))
            {
                Ok(_) => return Ok(id),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(anyhow::Error::from(error)
                        .context(format!("failed to reserve a slot in {}", dir.display())));
                }
            }
        }
        unreachable!("slots are unbounded")
    }

    /// Whether remote pane `id` still runs. An ended pane's leftovers (a
    /// stale socket from a killed holder, its pid file) are removed.
    pub fn state(&self, id: &RemotePaneId) -> Result<PaneState> {
        let socket = self.socket(id);
        match UnixStream::connect(&socket) {
            Ok(_) => Ok(PaneState::Running),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                let _ = std::fs::remove_file(&socket);
                let _ = std::fs::remove_file(self.pid_file(id));
                Ok(PaneState::Ended)
            }
            Err(error) => {
                Err(anyhow::Error::from(error).context(format!("failed to check remote pane {id}")))
            }
        }
    }

    /// The remote panes of `project` that still run, in the order they were
    /// started.
    pub fn running(&self, project: &str) -> Result<Vec<RemotePaneId>> {
        let mut running = Vec::new();
        for worktree in subdirectories(&self.dir.join(project))? {
            let dir = self.worktree_dir(project, &worktree);
            for entry in std::fs::read_dir(&dir)
                .with_context(|| format!("failed to read {}", dir.display()))?
            {
                let name = entry?.file_name();
                let Some(slot) = name
                    .to_str()
                    .and_then(|name| name.strip_suffix(".sock"))
                    .and_then(|slot| slot.parse().ok())
                else {
                    continue;
                };
                let id = RemotePaneId {
                    project: project.to_string(),
                    worktree: worktree.clone(),
                    slot,
                };
                // The pid file is made when the slot is reserved.
                let started = std::fs::metadata(self.pid_file(&id))
                    .and_then(|meta| meta.created().or_else(|_| meta.modified()))
                    .ok();
                if self.state(&id)? == PaneState::Running {
                    running.push((started, id));
                }
            }
        }
        running.sort_by(|(a, a_id), (b, b_id)| a.cmp(b).then(a_id.slot.cmp(&b_id.slot)));
        Ok(running.into_iter().map(|(_, id)| id).collect())
    }

    /// The pid of the process remote pane `id` started, once it is known.
    pub fn pid(&self, id: &RemotePaneId) -> Option<i32> {
        std::fs::read_to_string(self.pid_file(id))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 0)
    }

    /// End remote pane `id`: hang up its process, as closing a terminal
    /// would, and kill it if it does not exit.
    pub fn kill(&self, id: &RemotePaneId) -> Result<()> {
        // A pane just created may not have recorded its pid yet.
        let deadline = Instant::now() + Duration::from_secs(2);
        let pid = loop {
            match self.pid(id) {
                Some(pid) => break pid,
                None if Instant::now() < deadline && self.state(id)? == PaneState::Running => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                None => return Ok(()),
            }
        };
        unsafe { libc::kill(pid, libc::SIGHUP) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.state(id)? == PaneState::Running {
            if Instant::now() >= deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// The command that attaches this terminal to remote pane `id`. The
    /// detach and suspend keys are off, so every keystroke reaches the pane.
    pub fn attach_command(&self, id: &RemotePaneId) -> Command {
        let mut command = Command::new("dtach");
        command
            .arg("-a")
            .arg(self.socket(id))
            .args(["-E", "-z", "-r", "winch"]);
        command
    }
}

/// The names of the directories in `dir`; none when it does not exist.
fn subdirectories(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(
                anyhow::Error::from(error).context(format!("failed to read {}", dir.display()))
            );
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str().filter(|n| is_component(n))
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// The name of the worktree holding `workdir`: the nearest directory with a
/// `.git` entry, else `workdir` itself.
pub fn worktree_name(workdir: &Path) -> String {
    let root = workdir
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(workdir);
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| is_component(name))
        .unwrap_or_else(|| "pane".to_string())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A session holder rooted in a short temp dir (socket paths are
    /// limited), or `None` when dtach is not installed.
    pub fn holder() -> Option<(tempfile::TempDir, SessionHolder)> {
        which::which("dtach").ok()?;
        let temp = tempfile::Builder::new()
            .prefix("wmp")
            .tempdir_in("/tmp")
            .unwrap();
        let holder = SessionHolder::new(temp.path().join("panes"));
        Some((temp, holder))
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn worktree(temp: &tempfile::TempDir, name: &str) -> PathBuf {
        let dir = temp.path().join(name);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir.canonicalize().unwrap()
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "{} never written",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_ended(holder: &SessionHolder, id: &RemotePaneId) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while holder.state(id).unwrap() != PaneState::Ended {
            assert!(Instant::now() < deadline, "{id} never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_remote_pane_runs_its_command_in_the_worktree_and_knows_its_name() {
        let Some((temp, holder)) = holder() else {
            return;
        };
        let feature = worktree(&temp, "feature");
        std::fs::create_dir(feature.join("src")).unwrap();

        let id = holder
            .create(
                "api",
                &feature.join("src"),
                &argv(&[
                    "sh",
                    "-c",
                    "echo \"$(pwd -P) $WORKMUX_REMOTE_PANE $TERM\" > ran; exec sleep 30",
                ]),
            )
            .unwrap();

        assert_eq!(id.to_string(), "api/feature/0");
        let ran = wait_for_file(&feature.join("src/ran"));
        let term = std::env::var("TERM")
            .ok()
            .filter(|t| !t.is_empty() && t != "dumb")
            .unwrap_or_else(|| DEFAULT_TERM.to_string());
        assert_eq!(
            ran.trim(),
            format!("{} api/feature/0 {term}", feature.join("src").display())
        );
        assert_eq!(holder.state(&id).unwrap(), PaneState::Running);
        holder.kill(&id).unwrap();
    }

    #[test]
    fn panes_of_one_worktree_take_the_next_free_slot() {
        let Some((temp, holder)) = holder() else {
            return;
        };
        let feature = worktree(&temp, "feature");
        let sleep = argv(&["sleep", "30"]);

        let first = holder.create("api", &feature, &sleep).unwrap();
        let second = holder.create("api", &feature, &sleep).unwrap();
        let other = holder.create("web", &feature, &sleep).unwrap();

        assert_eq!(
            [first.slot, second.slot, other.slot],
            [0, 1, 0],
            "{first} {second} {other}"
        );
        holder.kill(&first).unwrap();
        assert_eq!(holder.state(&first).unwrap(), PaneState::Ended);
        assert_eq!(holder.state(&second).unwrap(), PaneState::Running);
        let reused = holder.create("api", &feature, &sleep).unwrap();
        assert_eq!(reused.slot, 0);
        for id in [reused, second, other] {
            holder.kill(&id).unwrap();
        }
    }

    #[test]
    fn a_projects_running_panes_are_listed_in_the_order_they_were_started() {
        let Some((temp, holder)) = holder() else {
            return;
        };
        let feature = worktree(&temp, "feature");
        let bugfix = worktree(&temp, "bugfix");
        let sleep = argv(&["sleep", "30"]);
        let first = holder.create("api", &feature, &sleep).unwrap();
        let second = holder.create("api", &feature, &sleep).unwrap();
        let other = holder.create("web", &feature, &sleep).unwrap();
        let ended = holder
            .create("api", &bugfix, &argv(&["sh", "-c", "exit 0"]))
            .unwrap();
        wait_ended(&holder, &ended);
        holder.kill(&first).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // Started last, though it reuses the lowest slot.
        let third = holder.create("api", &feature, &sleep).unwrap();
        assert_eq!(third.slot, 0);

        let running = holder.running("api").unwrap();

        assert_eq!(running, vec![second.clone(), third.clone()]);
        assert_eq!(holder.running("nothing").unwrap(), vec![]);
        for id in [second, third, other] {
            holder.kill(&id).unwrap();
        }
    }

    #[test]
    fn a_pane_whose_process_exits_has_ended() {
        let Some((temp, holder)) = holder() else {
            return;
        };
        let feature = worktree(&temp, "feature");

        let id = holder
            .create("api", &feature, &argv(&["sh", "-c", "sleep 0.2"]))
            .unwrap();

        wait_ended(&holder, &id);
    }

    #[test]
    fn a_pane_whose_holder_was_killed_has_ended() {
        let Some((temp, holder)) = holder() else {
            return;
        };
        let feature = worktree(&temp, "feature");
        let id = holder
            .create("api", &feature, &argv(&["sleep", "30"]))
            .unwrap();
        let socket = holder.socket(&id);
        // SIGKILL the dtach master, leaving its socket behind.
        let masters = Command::new("pgrep")
            .args(["-f", &format!("dtach -n {}", socket.display())])
            .output()
            .unwrap();
        for pid in String::from_utf8_lossy(&masters.stdout).split_whitespace() {
            unsafe { libc::kill(pid.parse().unwrap(), libc::SIGKILL) };
        }

        wait_ended(&holder, &id);
        assert!(!socket.exists());
    }

    #[test]
    fn a_missing_command_is_refused() {
        let holder = SessionHolder::new("/nonexistent");

        let error = holder.create("api", Path::new("/"), &[]).unwrap_err();

        assert_eq!(error.to_string(), "no command for the remote pane");
    }

    #[test]
    fn remote_pane_names_round_trip_and_stay_inside_the_panes_directory() {
        let id: RemotePaneId = "api/feature-x/3".parse().unwrap();
        assert_eq!(
            id,
            RemotePaneId {
                project: "api".to_string(),
                worktree: "feature-x".to_string(),
                slot: 3,
            }
        );
        assert_eq!(serde_json::to_value(&id).unwrap(), "api/feature-x/3");
        for bad in [
            "api/feature",
            "../x/0",
            "api/../0",
            "api/x/y",
            "a/b/0/c",
            "/x/0",
        ] {
            assert!(bad.parse::<RemotePaneId>().is_err(), "{bad}");
        }
    }
}
