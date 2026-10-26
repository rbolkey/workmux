//! Test transports. Everything above the Transport interface stays real.

use anyhow::{Result, anyhow};
use std::collections::VecDeque;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use super::transport::{Provisioning, RemoteState, StdioProcess, Transport, TtyExec};

pub const SERVE_CHILD: &str = "remote::testing::serve_child";

/// Runs `workmux remote serve` as a child of this test binary. Records each
/// child's pid so a test can kill it to simulate a disconnect. Its remote
/// keeps its state (the shell socket, shims) in a directory of its own.
pub struct ServeTransport {
    pub pids: Mutex<Vec<u32>>,
    /// Extra environment for the remote, such as another state dir.
    pub env: Vec<(String, std::path::PathBuf)>,
    /// The remote's own state dir, unless `env` names another.
    pub state: tempfile::TempDir,
}

impl Default for ServeTransport {
    fn default() -> Self {
        Self {
            pids: Mutex::new(Vec::new()),
            env: Vec::new(),
            state: tempfile::Builder::new()
                .prefix("wms")
                .tempdir_in("/tmp")
                .expect("a state directory"),
        }
    }
}

impl ServeTransport {
    /// The socket commands run in this remote's shells reach the
    /// workstation through.
    pub fn shell_socket(&self) -> std::path::PathBuf {
        self.state
            .path()
            .join("workmux")
            .join(super::shell::SOCKET_NAME)
    }

    /// The directory of this remote's shims.
    pub fn shims(&self) -> std::path::PathBuf {
        self.state.path().join("workmux").join("shims/bin")
    }

    pub fn kill_latest(&self) {
        let pid = *self.pids.lock().unwrap().last().expect("a served child");
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    pub fn exec_count(&self) -> usize {
        self.pids.lock().unwrap().len()
    }
}

impl Transport for ServeTransport {
    fn exec_captured(&self, _argv: &[String]) -> Result<Output> {
        unimplemented!()
    }

    fn exec_stdio(&self, argv: &[String]) -> Result<StdioProcess> {
        assert_eq!(argv, ["workmux", "remote", "serve"]);
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([SERVE_CHILD, "--exact", "--nocapture", "--ignored"])
            .env(crate::test_support::ISOLATED_TEST_ENV, SERVE_CHILD)
            .env("XDG_STATE_HOME", self.state.path())
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        let process = StdioProcess::spawn(command)?;
        self.pids.lock().unwrap().push(process.id());
        Ok(process)
    }

    /// Runs the remote terminal as a workstation process in its workdir.
    fn tty_command(&self, exec: &TtyExec) -> Result<Command> {
        let (program, args) = exec
            .argv
            .split_first()
            .ok_or_else(|| anyhow!("no command"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(exec.env.iter().map(|(k, v)| (k, v)));
        if let Some(workdir) = &exec.workdir {
            command.current_dir(workdir);
        }
        Ok(command)
    }
}

#[test]
#[ignore = "runs as the remote of ServeTransport"]
fn serve_child() {
    if crate::test_support::is_isolated_child(SERVE_CHILD) {
        super::serve::run().unwrap();
    }
}

/// Runs each control channel as a shell script on the workstation: the
/// queued scripts in order, then the last one for every later attempt.
pub struct ScriptTransport {
    scripts: Mutex<VecDeque<String>>,
    pub calls: Mutex<Vec<Instant>>,
}

impl ScriptTransport {
    pub fn new<S: Into<String>>(scripts: impl IntoIterator<Item = S>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().map(Into::into).collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn exec_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl Transport for ScriptTransport {
    fn exec_captured(&self, _argv: &[String]) -> Result<Output> {
        unimplemented!()
    }

    fn exec_stdio(&self, _argv: &[String]) -> Result<StdioProcess> {
        self.calls.lock().unwrap().push(Instant::now());
        let script = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.len() > 1 {
                scripts.pop_front()
            } else {
                scripts.front().cloned()
            }
        }
        .ok_or_else(|| anyhow!("no script"))?;
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        StdioProcess::spawn(command)
    }

    fn tty_command(&self, _exec: &TtyExec) -> Result<Command> {
        unimplemented!()
    }
}

/// A remote whose transport reports it stopped, or missing. Counts what it
/// is asked to do.
#[derive(Default)]
pub struct StoppedTransport {
    pub execs: AtomicUsize,
    pub starts: AtomicUsize,
    /// Reports no remote yet rather than a stopped one.
    pub missing: bool,
}

impl Transport for StoppedTransport {
    fn exec_captured(&self, _argv: &[String]) -> Result<Output> {
        self.execs.fetch_add(1, Ordering::SeqCst);
        Err(anyhow!("stopped"))
    }

    fn exec_stdio(&self, _argv: &[String]) -> Result<StdioProcess> {
        self.execs.fetch_add(1, Ordering::SeqCst);
        Err(anyhow!("stopped"))
    }

    fn tty_command(&self, _exec: &TtyExec) -> Result<Command> {
        unimplemented!()
    }

    fn provisioning(&self) -> Option<&dyn Provisioning> {
        Some(self)
    }
}

impl Provisioning for StoppedTransport {
    fn status(&self) -> Result<RemoteState> {
        Ok(if self.missing {
            RemoteState::Missing
        } else {
            RemoteState::Stopped
        })
    }

    fn create(&self) -> Result<()> {
        unimplemented!()
    }

    fn start(&self) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        unimplemented!()
    }

    fn delete(&self) -> Result<()> {
        unimplemented!()
    }
}

/// Project `api` on loopback remote `devbox`, cloned from GitHub over SSH;
/// its workstation clone is `/src/api`.
pub fn devbox_api() -> super::router::RemoteProject {
    super::router::RemoteProject {
        root: std::path::PathBuf::from("/src/api"),
        site: super::session::ProjectSite {
            remote: "devbox".to_string(),
            name: "api".to_string(),
            path: "~/api".to_string(),
            git_url: Some(super::git_url::GitUrl::new("git@github.com:me/api.git")),
            session: Some("api".to_string()),
        },
        remote_config: crate::config::RemoteConfig {
            transport: crate::config::TransportKind::Loopback,
            target: "/tmp/devbox".to_string(),
            builtin_shims: None,
            host_commands: Vec::new(),
            env_passthrough: Vec::new(),
            env: Default::default(),
            openshell: None,
        },
    }
}

/// Project `api` in its OpenShell sandbox `api`.
pub fn openshell_api() -> super::router::RemoteProject {
    let mut project = devbox_api();
    project.site.remote = "api".to_string();
    project.site.git_url = Some(super::git_url::GitUrl::new("https://example.com/api.git"));
    project.remote_config = crate::config::RemoteConfig {
        transport: crate::config::TransportKind::Openshell,
        target: "api".to_string(),
        builtin_shims: None,
        host_commands: Vec::new(),
        env_passthrough: Vec::new(),
        env: Default::default(),
        openshell: Some(crate::config::OpenshellRemote {
            project: "api".to_string(),
            git_url: project.site.git_url.clone(),
            path: "~/api".to_string(),
            settings: crate::config::OpenshellProject::default(),
            agents: vec!["claude".to_string()],
            extra_mounts: false,
        }),
    };
    project
}
