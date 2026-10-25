//! Test harness: a target to `ssh` into, plus process and PTY helpers.

use std::fs;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Host alias the local harness writes into its `ssh_config`.
const LOCAL_HOST: &str = "wmx-test";
/// Same server, but offering a key the server does not accept.
pub const LOCAL_BAD_KEY_HOST: &str = "wmx-test-badkey";

/// Yields an [`SshHarness`], or returns from the test when SSH tests are off.
macro_rules! ssh_harness {
    () => {
        match $crate::ssh_learning::harness::SshHarness::from_env() {
            Some(harness) => harness,
            None => return,
        }
    };
}
pub(crate) use ssh_harness;

/// Yields an [`SshHarness`] whose remote login shell is fish, or returns
/// from the test when SSH tests are off, the target is external, or fish is
/// not on PATH.
macro_rules! fish_ssh_harness {
    () => {
        match $crate::ssh_learning::harness::SshHarness::fish_from_env() {
            Some(harness) => harness,
            None => {
                eprintln!(
                    "skipped: needs WORKMUX_TEST_SSH=1, the local sshd harness and fish on PATH"
                );
                return;
            }
        }
    };
}
pub(crate) use fish_ssh_harness;

/// Yields the harness's [`LocalSshd`], or returns from the test when running
/// against an external target whose server configuration we cannot control.
macro_rules! local_sshd {
    ($harness:expr) => {
        match $harness.local() {
            Some(sshd) => sshd,
            None => {
                eprintln!("skipped: needs the local sshd harness");
                return;
            }
        }
    };
}
pub(crate) use local_sshd;

pub struct SshHarness {
    target: Target,
    /// Scratch directory on the remote, for marker files.
    remote_dir: String,
    /// Short local directory for ControlMaster sockets (104-byte path limit).
    control_dir: TempDir,
}

enum Target {
    Local(LocalSshd),
    External {
        host: String,
        config: Option<PathBuf>,
    },
}

impl SshHarness {
    /// What the environment asks for: `None` when SSH tests are off, else
    /// the external target's host, or `None` inside for the local harness.
    fn requested_target() -> Option<Option<String>> {
        if std::env::var("WORKMUX_TEST_SSH").as_deref() != Ok("1") {
            return None;
        }
        Some(
            std::env::var("WORKMUX_TEST_SSH_TARGET")
                .ok()
                .filter(|host| !host.is_empty()),
        )
    }

    pub fn from_env() -> Option<Self> {
        let target = match Self::requested_target()? {
            Some(host) => Target::External {
                host,
                config: std::env::var_os("WORKMUX_TEST_SSH_CONFIG").map(PathBuf::from),
            },
            None => Target::Local(LocalSshd::start(None)),
        };
        Some(Self::with_target(target))
    }

    /// A local harness whose remote login shell is fish: the user's own
    /// login shell hands every remote command to fish unchanged, as sshd
    /// would hand it to a fish login shell.
    pub fn fish_from_env() -> Option<Self> {
        if Self::requested_target()?.is_some() {
            return None;
        }
        let fish = which::which("fish").ok()?;
        Some(Self::with_target(Target::Local(LocalSshd::start(Some(
            &fish,
        )))))
    }

    fn with_target(target: Target) -> Self {
        let mut harness = Self {
            target,
            remote_dir: String::new(),
            control_dir: short_tempdir(),
        };
        // Through sh, so it reads the same to any login shell.
        let out = harness.run(&[], r#"sh -c 'mktemp -d "${TMPDIR:-/tmp}/wmx-ssh.XXXXXX"'"#);
        assert!(out.status.success(), "remote mktemp failed: {out:?}");
        harness.remote_dir = stdout_of(&out).trim().to_string();
        harness
    }

    pub fn host(&self) -> &str {
        match &self.target {
            Target::Local(_) => LOCAL_HOST,
            Target::External { host, .. } => host,
        }
    }

    pub fn local(&self) -> Option<&LocalSshd> {
        match &self.target {
            Target::Local(sshd) => Some(sshd),
            Target::External { .. } => None,
        }
    }

    pub fn remote_dir(&self) -> &str {
        &self.remote_dir
    }

    pub fn control_dir(&self) -> &Path {
        self.control_dir.path()
    }

    /// The `ssh_config` the harness passes with `-F`, if any.
    pub fn ssh_config(&self) -> Option<&Path> {
        match &self.target {
            Target::Local(sshd) => Some(sshd.ssh_config.as_path()),
            Target::External { config, .. } => config.as_deref(),
        }
    }

    /// `ssh [-F config] <options>` with no host yet.
    pub fn ssh_base(&self, options: &[&str]) -> Command {
        let mut cmd = Command::new("ssh");
        if let Some(config) = self.ssh_config() {
            cmd.arg("-F").arg(config);
        }
        cmd.args(options);
        cmd
    }

    /// `ssh <options> <host> <remote_command>`.
    pub fn ssh(&self, options: &[&str], remote_command: &str) -> Command {
        let mut cmd = self.ssh_base(options);
        cmd.arg(self.host()).arg(remote_command);
        cmd
    }

    /// Runs a non-interactive exec and captures its output.
    pub fn run(&self, options: &[&str], remote_command: &str) -> Output {
        let mut opts = vec!["-T"];
        opts.extend_from_slice(options);
        self.ssh(&opts, remote_command)
            .stdin(Stdio::null())
            .output()
            .expect("spawn ssh")
    }

    /// Reads a remote file, or returns an empty string if it is missing.
    pub fn remote_read(&self, path: &str) -> String {
        stdout_of(&self.run(&[], &format!("cat '{path}' 2>/dev/null")))
    }

    /// Polls a remote file until `done` accepts its contents.
    pub fn wait_remote_file(
        &self,
        path: &str,
        timeout: Duration,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let contents = self.remote_read(path);
            if done(&contents) || Instant::now() >= deadline {
                return contents;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Sends a signal to a remote process by pid.
    pub fn remote_kill(&self, signal: &str, pid: &str) {
        self.run(&[], &format!("kill -{signal} {pid}"));
    }
}

impl Drop for SshHarness {
    fn drop(&mut self) {
        if !self.remote_dir.is_empty() {
            self.run(&[], &format!("rm -rf '{}'", self.remote_dir));
        }
    }
}

/// A user-mode `sshd` on a free localhost port, configured entirely from a
/// temp directory. Never touches `~/.ssh` or the system sshd.
pub struct LocalSshd {
    dir: TempDir,
    process: Child,
    pub port: u16,
    pub ssh_config: PathBuf,
}

impl LocalSshd {
    /// Starts the server. With `fish`, remote commands run in that fish
    /// instead of the user's login shell.
    fn start(fish: Option<&Path>) -> Self {
        let dir = short_tempdir();
        let path = |name: &str| dir.path().join(name);
        for key in ["host_key", "client_key", "bad_key"] {
            let status = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", ""])
                .arg("-f")
                .arg(path(key))
                .stdin(Stdio::null())
                .status()
                .expect("spawn ssh-keygen");
            assert!(status.success(), "ssh-keygen failed");
        }
        fs::copy(path("client_key.pub"), path("authorized_keys")).unwrap();
        fs::create_dir(path("rc")).unwrap();
        fs::write(path("rc/.zshenv"), "").unwrap();
        fs::write(path("rc/.bashrc"), "").unwrap();
        fs::create_dir(path("rc/fish")).unwrap();
        fs::write(path("rc/fish/config.fish"), "").unwrap();
        fs::write(path("banner"), "WMX-BANNER\n").unwrap();

        let port = free_port();
        let d = dir.path().display();
        // sshd runs ForceCommand through the user's login shell, which
        // expands $SSH_ORIGINAL_COMMAND without parsing it: fish parses the
        // command as a fish login shell would. XDG_CONFIG_HOME points fish
        // at the temp dir's config.fish.
        let force_command = fish
            .map(|fish| {
                format!(
                    "ForceCommand exec env XDG_CONFIG_HOME={d}/rc {} -c \"$SSH_ORIGINAL_COMMAND\"\n",
                    fish.display()
                )
            })
            .unwrap_or_default();
        // ZDOTDIR and BASH_ENV point the login shell's startup files at the
        // temp dir, so tests control shell-startup output and the user's own
        // rc files stay out of the way.
        let sshd_config = format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {d}/host_key\n\
             AuthorizedKeysFile {d}/authorized_keys\n\
             PidFile {d}/sshd.pid\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             UsePAM no\n\
             StrictModes no\n\
             AcceptEnv WMX_ACCEPTED_*\n\
             Banner {d}/banner\n\
             SetEnv ZDOTDIR={d}/rc BASH_ENV={d}/rc/.bashrc\n\
             LogLevel VERBOSE\n\
             {force_command}"
        );
        fs::write(path("sshd_config"), sshd_config).unwrap();

        let user = std::env::var("USER").expect("USER is set");
        let host_block = |alias: &str, key: &str| {
            format!(
                "Host {alias}\n\
                 \x20 HostName 127.0.0.1\n\
                 \x20 Port {port}\n\
                 \x20 User {user}\n\
                 \x20 IdentityFile {d}/{key}\n\
                 \x20 IdentitiesOnly yes\n\
                 \x20 IdentityAgent none\n\
                 \x20 UserKnownHostsFile /dev/null\n\
                 \x20 StrictHostKeyChecking no\n\
                 \x20 BatchMode yes\n\
                 \x20 LogLevel ERROR\n"
            )
        };
        let ssh_config = path("ssh_config");
        fs::write(
            &ssh_config,
            host_block(LOCAL_HOST, "client_key") + &host_block(LOCAL_BAD_KEY_HOST, "bad_key"),
        )
        .unwrap();

        let mut process = Command::new("/usr/sbin/sshd")
            .arg("-D")
            .arg("-f")
            .arg(path("sshd_config"))
            .arg("-E")
            .arg(path("sshd.log"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sshd");
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Ok(Some(status)) = process.try_wait() {
                panic!(
                    "sshd exited with {status}: {}",
                    fs::read_to_string(path("sshd.log")).unwrap_or_default()
                );
            }
            assert!(Instant::now() < deadline, "sshd did not start listening");
            thread::sleep(Duration::from_millis(20));
        }

        Self {
            dir,
            process,
            port,
            ssh_config,
        }
    }

    /// A file in the harness's temp dir, such as `client_key.pub`.
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Replaces what the remote login shell prints while starting up.
    pub fn set_shell_startup(&self, script: &str) {
        fs::write(self.dir.path().join("rc/.zshenv"), script).unwrap();
        fs::write(self.dir.path().join("rc/.bashrc"), script).unwrap();
    }

    /// Replaces what a fish login shell runs while starting up.
    pub fn set_fish_startup(&self, script: &str) {
        fs::write(self.dir.path().join("rc/fish/config.fish"), script).unwrap();
    }
}

impl Drop for LocalSshd {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Temp dirs live under `/tmp` so socket paths stay short.
fn short_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("wmx")
        .tempdir_in("/tmp")
        .expect("create temp dir")
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Waits for a child to exit, killing it if it outlives `timeout`.
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Kills a child on drop, so a failing assertion never leaks an `ssh`.
pub struct Reaped(pub Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Accumulates everything read from a stream on a background thread.
#[derive(Clone)]
pub struct Collector {
    state: Arc<(Mutex<CollectorState>, Condvar)>,
}

#[derive(Default)]
struct CollectorState {
    bytes: Vec<u8>,
    eof: bool,
}

impl Collector {
    pub fn spawn(mut reader: impl Read + Send + 'static) -> Self {
        let collector = Self {
            state: Arc::new((Mutex::new(CollectorState::default()), Condvar::new())),
        };
        let state = Arc::clone(&collector.state);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                // A PTY master reports EIO once the slave closes: treat as EOF.
                let n = reader.read(&mut buf).unwrap_or(0);
                let (lock, cvar) = &*state;
                let mut st = lock.lock().unwrap();
                if n == 0 {
                    st.eof = true;
                    cvar.notify_all();
                    return;
                }
                st.bytes.extend_from_slice(&buf[..n]);
                cvar.notify_all();
            }
        });
        collector
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.state.0.lock().unwrap().bytes).into_owned()
    }

    pub fn len(&self) -> usize {
        self.state.0.lock().unwrap().bytes.len()
    }

    /// Waits until the collected text satisfies `done`; returns whether it did.
    pub fn wait_until(&self, timeout: Duration, done: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.state;
        let mut st = lock.lock().unwrap();
        loop {
            if done(&String::from_utf8_lossy(&st.bytes)) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline || st.eof {
                return false;
            }
            st = cvar.wait_timeout(st, deadline - now).unwrap().0;
        }
    }

    pub fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        self.wait_until(timeout, |text| text.contains(needle))
    }

    pub fn wait_eof(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.state;
        let mut st = lock.lock().unwrap();
        while !st.eof {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = cvar.wait_timeout(st, deadline - now).unwrap().0;
        }
        true
    }
}

/// A pseudo-terminal pair, so `ssh -t` sees a real local TTY.
pub struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

impl Pty {
    pub fn open(rows: u16, cols: u16) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = winsize(rows, cols);
        // SAFETY: openpty writes two fds into the provided out-pointers.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: openpty succeeded, so both fds are open and owned by us.
        unsafe {
            Self {
                master: OwnedFd::from_raw_fd(master),
                slave: OwnedFd::from_raw_fd(slave),
            }
        }
    }

    /// Spawns `cmd` as a session leader with the slave as controlling TTY.
    pub fn spawn(&self, cmd: &mut Command) -> Child {
        let stdio = || Stdio::from(self.slave.try_clone().unwrap());
        cmd.stdin(stdio()).stdout(stdio()).stderr(stdio());
        // SAFETY: setsid and ioctl are async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn under pty")
    }

    /// Collects what the child writes to the terminal.
    pub fn reader(&self) -> Collector {
        Collector::spawn(fs::File::from(self.master.try_clone().unwrap()))
    }

    /// Resizes the terminal; the kernel sends SIGWINCH to the child.
    pub fn resize(&self, rows: u16, cols: u16) {
        let size = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ reads a winsize from the pointer.
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        assert_eq!(rc, 0, "TIOCSWINSZ failed");
    }
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
