//! The Transport interface: how the workstation runs processes on a remote.
//!
//! A transport runs commands on its remote three ways: captured, as a
//! long-lived stdio pipe (the control channel), and interactively with a TTY
//! (remote panes). Transports that manage the remote's lifecycle also
//! provide provisioning. The first element of a command is the program on
//! the remote; `workmux` means remote workmux.

use anyhow::{Context, Result, anyhow};
use std::io::Read;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much of a process's stderr is kept to explain a failure.
const STDERR_TAIL_BYTES: usize = 4096;

/// An interactive command for a remote pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TtyExec {
    pub argv: Vec<String>,
    /// Working directory on the remote.
    pub workdir: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Lifecycle of a remote as its transport reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteState {
    Running,
    Stopped,
    Missing,
}

/// Optional lifecycle hooks for transports that manage their remote, such as
/// an OpenShell sandbox.
pub trait Provisioning: Send + Sync {
    fn status(&self) -> Result<RemoteState>;
    fn create(&self) -> Result<()>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

pub trait Transport: Send + Sync {
    /// Run a command to completion, capturing its output.
    fn exec_captured(&self, argv: &[String]) -> Result<Output>;

    /// Start a long-lived command with piped stdin and stdout.
    fn exec_stdio(&self, argv: &[String]) -> Result<StdioProcess>;

    /// Build the workstation command that runs `exec` on the remote with a
    /// TTY. The caller runs it on a terminal.
    fn tty_command(&self, exec: &TtyExec) -> Result<Command>;

    /// Lifecycle hooks, for transports that manage their remote.
    fn provisioning(&self) -> Option<&dyn Provisioning> {
        None
    }
}

/// A long-lived workstation process speaking to a remote over stdio.
///
/// Its stderr is drained in the background and the tail kept, so a failure
/// can be reported in the transport's own words. Dropping it kills the
/// process.
pub struct StdioProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_done: Arc<AtomicBool>,
}

impl StdioProcess {
    pub fn spawn(mut command: Command) -> Result<Self> {
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {program}"))?;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_done = Arc::new(AtomicBool::new(false));
        if let Some(mut stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            let done = stderr_done.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(read) = stderr.read(&mut buf) {
                    if read == 0 {
                        break;
                    }
                    let mut tail = tail.lock().unwrap();
                    tail.extend_from_slice(&buf[..read]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    tail.drain(..excess);
                }
                done.store(true, Ordering::Release);
            });
        } else {
            stderr_done.store(true, Ordering::Release);
        }
        Ok(Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            child,
            stderr_tail,
            stderr_done,
        })
    }

    /// Take the process's stdin and stdout. Panics if already taken.
    pub fn take_stdio(&mut self) -> (ChildStdin, ChildStdout) {
        (
            self.stdin.take().expect("stdin already taken"),
            self.stdout.take().expect("stdout already taken"),
        )
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The transport's own account of why the process failed: its stderr,
    /// or else its exit status. Waits up to `wait` for the process to exit.
    pub fn failure(&mut self, wait: Duration) -> String {
        let deadline = Instant::now() + wait;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => break None,
            }
        };
        // Stderr may still be draining after the process exits.
        while !self.stderr_done.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let stderr = String::from_utf8_lossy(&self.stderr_tail.lock().unwrap())
            .trim()
            .replace("\r\n", "\n");
        if !stderr.is_empty() {
            return stderr;
        }
        match status {
            Some(status) => format!("transport process exited with {status}"),
            None => "transport process is still running".to_string(),
        }
    }
}

impl Drop for StdioProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Runs remote workmux as a local child process rooted in its own directory
/// and `HOME`, standing in for a remote's filesystem in tests.
pub struct Loopback {
    root: std::path::PathBuf,
    /// Binary that stands in for `workmux` on the remote.
    workmux: std::path::PathBuf,
}

/// Environment that points a process at the workstation's multiplexer or
/// sandbox supervisor; a loopback remote must not inherit it.
const WORKSTATION_ENV: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "KITTY_WINDOW_ID",
    "KITTY_LISTEN_ON",
];

impl Loopback {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Result<Self> {
        Ok(Self {
            root: root.into(),
            workmux: std::env::current_exe().context("failed to locate the workmux binary")?,
        })
    }

    #[cfg(test)]
    pub fn with_workmux(root: impl Into<std::path::PathBuf>, workmux: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            workmux: workmux.into(),
        }
    }

    fn command(&self, argv: &[String], workdir: Option<&str>) -> Result<Command> {
        if !self.root.is_dir() {
            return Err(anyhow!(
                "loopback root {} does not exist",
                self.root.display()
            ));
        }
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow!("no command to run on the remote"))?;
        let home = self.root.join("home");
        std::fs::create_dir_all(&home)
            .with_context(|| format!("failed to create {}", home.display()))?;

        let mut command = if program == "workmux" {
            Command::new(&self.workmux)
        } else {
            Command::new(program)
        };
        command
            .args(args)
            .current_dir(workdir.map_or_else(|| self.root.clone(), |dir| self.root.join(dir)))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"));
        for name in WORKSTATION_ENV {
            command.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("WORKMUX_") {
                command.env_remove(name);
            }
        }
        Ok(command)
    }
}

impl Transport for Loopback {
    fn exec_captured(&self, argv: &[String]) -> Result<Output> {
        let mut command = self.command(argv, None)?;
        command
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run {}", argv[0]))
    }

    fn exec_stdio(&self, argv: &[String]) -> Result<StdioProcess> {
        StdioProcess::spawn(self.command(argv, None)?)
    }

    fn tty_command(&self, exec: &TtyExec) -> Result<Command> {
        let mut command = self.command(&exec.argv, exec.workdir.as_deref())?;
        command.envs(exec.env.iter().map(|(k, v)| (k, v)));
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn loopback_runs_commands_in_its_root_with_its_own_home() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let loopback = Loopback::with_workmux(&root_path, "/bin/echo");

        let output = loopback
            .exec_captured(&argv(&["sh", "-c", "pwd; echo $HOME; echo ${TMUX:-none}"]))
            .unwrap();

        let lines: Vec<String> = stdout(&output).lines().map(str::to_string).collect();
        assert_eq!(
            lines,
            vec![
                root_path.display().to_string(),
                root_path.join("home").display().to_string(),
                "none".to_string(),
            ]
        );
    }

    #[test]
    fn loopback_runs_its_workmux_binary_for_workmux() {
        let root = tempfile::tempdir().unwrap();
        let loopback = Loopback::with_workmux(root.path(), "/bin/echo");

        let output = loopback
            .exec_captured(&argv(&["workmux", "remote", "serve"]))
            .unwrap();

        assert_eq!(stdout(&output), "remote serve");
    }

    #[test]
    fn loopback_tty_command_uses_the_workdir_and_env() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        std::fs::create_dir(root_path.join("repo")).unwrap();
        let loopback = Loopback::with_workmux(&root_path, "/bin/echo");

        let output = loopback
            .tty_command(&TtyExec {
                argv: argv(&["sh", "-c", "pwd; echo $WM_LEARN"]),
                workdir: Some("repo".to_string()),
                env: vec![("WM_LEARN".to_string(), "from-workstation".to_string())],
            })
            .unwrap()
            .output()
            .unwrap();

        assert_eq!(
            stdout(&output),
            format!("{}\nfrom-workstation", root_path.join("repo").display())
        );
    }

    #[test]
    fn loopback_with_a_missing_root_is_unreachable() {
        let loopback = Loopback::with_workmux("/nonexistent/wm-loopback", "/bin/echo");

        let error = loopback.exec_stdio(&argv(&["workmux"])).err().unwrap();

        assert_eq!(
            error.to_string(),
            "loopback root /nonexistent/wm-loopback does not exist"
        );
    }

    #[test]
    fn a_failed_stdio_process_reports_its_stderr() {
        let root = tempfile::tempdir().unwrap();
        let loopback = Loopback::with_workmux(root.path(), "/bin/echo");
        let mut process = loopback
            .exec_stdio(&argv(&[
                "sh",
                "-c",
                "echo 'ssh: connect to host devbox port 22: Connection refused\r' >&2; exit 255",
            ]))
            .unwrap();

        assert_eq!(
            process.failure(Duration::from_secs(5)),
            "ssh: connect to host devbox port 22: Connection refused"
        );
    }

    #[test]
    fn a_silent_failed_stdio_process_reports_its_exit_status() {
        let root = tempfile::tempdir().unwrap();
        let loopback = Loopback::with_workmux(root.path(), "/bin/echo");
        let mut process = loopback
            .exec_stdio(&argv(&["sh", "-c", "exit 127"]))
            .unwrap();

        assert!(
            process
                .failure(Duration::from_secs(5))
                .contains("exit status: 127")
        );
    }
}
