//! Test-only helpers shared across modules.

use std::path::Path;
use std::process::Command;

pub const ISOLATED_TEST_ENV: &str = "WM_ISOLATED_TEST";
pub const ISOLATED_TEST_CANARY: &str = "WM_ISOLATED_TEST_EXECUTED";

pub fn is_isolated_child(test_name: &str) -> bool {
    std::env::var_os(ISOLATED_TEST_ENV).as_deref() == Some(std::ffi::OsStr::new(test_name))
}

pub fn run_isolated_test(test_name: &str, cwd: &Path, envs: &[(&str, &Path)]) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(cwd)
        .env(ISOLATED_TEST_ENV, test_name);
    clear_local_git_env(&mut command);

    for (key, value) in envs {
        command.env(key, value);
    }

    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated test {test_name} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );
    assert!(
        stdout.contains(ISOLATED_TEST_CANARY),
        "isolated test {test_name} did not execute\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
}

pub fn run_git(repo: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run a git command and return its trimmed stdout.
pub fn run_git_output(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn init_repo(dir: &Path) {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .output()
        .expect("git init should run");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test User"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    run_git(dir, &["add", "README.md"]);
    run_git(dir, &["commit", "-m", "initial"]);
}

pub(crate) fn clear_local_git_env(command: &mut Command) {
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_DIR",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_SHALLOW_FILE",
        "GIT_WORK_TREE",
    ] {
        command.env_remove(key);
    }
    command.env_remove("GIT_CONFIG_COUNT");
    command.env_remove("GIT_CONFIG_PARAMETERS");
    for i in 0..32 {
        command.env_remove(format!("GIT_CONFIG_KEY_{i}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{i}"));
    }
}

/// A tmux server on its own socket, killed on drop.
pub struct PrivateTmuxServer {
    _temp: tempfile::TempDir,
    pub socket: String,
}

impl PrivateTmuxServer {
    pub fn start() -> Option<Self> {
        which::which("tmux").ok()?;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("tmux.sock").to_string_lossy().into_owned();
        Some(Self {
            _temp: temp,
            socket,
        })
    }

    pub fn query(&self, args: &[&str]) -> String {
        crate::cmd::Cmd::new("tmux")
            .args(&["-S", &self.socket])
            .args(args)
            .run_and_capture_stdout()
            .unwrap()
            .trim()
            .to_string()
    }

    /// Starts a detached session and returns its first pane.
    pub fn new_session(&self, name: &str, cwd: &Path) -> String {
        let cwd = cwd.to_string_lossy();
        self.query(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-s",
            name,
            "-c",
            &cwd,
        ])
    }

    pub fn new_window(&self, session: &str) -> String {
        self.query(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-t",
            &format!("{session}:"),
        ])
    }

    pub fn window_of(&self, pane_id: &str) -> String {
        self.query(&["display-message", "-p", "-t", pane_id, "#{window_id}"])
    }
}

impl Drop for PrivateTmuxServer {
    fn drop(&mut self) {
        let _ = crate::cmd::Cmd::new("tmux")
            .args(&["-S", &self.socket, "kill-server"])
            .run();
    }
}

