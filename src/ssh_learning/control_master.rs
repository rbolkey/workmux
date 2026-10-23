//! Q: does reusing a ControlMaster/ControlPersist make repeated exec cheap,
//! and what are the pitfalls (stale and frozen masters, socket path length)?
//!
//! Each test uses its own ControlPath in a temp dir, never the user's.

use std::fs;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::harness::{
    Collector, Reaped, SshHarness, ssh_harness, stderr_of, stdout_of, wait_timeout,
};

/// A ControlPersist master started for one test, killed on drop.
struct Master<'a> {
    harness: &'a SshHarness,
    pid: i32,
}

impl<'a> Master<'a> {
    fn start(harness: &'a SshHarness) -> Self {
        let mut master = Self { harness, pid: 0 };
        let out = master.run("true");
        assert!(out.status.success(), "{out:?}");
        master.pid = master.check().expect("master is running");
        master
    }

    /// Takes charge of whatever master is already behind the ControlPath.
    fn adopt(harness: &'a SshHarness) -> Self {
        let mut master = Self { harness, pid: 0 };
        master.pid = master.check().unwrap_or(0);
        master
    }

    /// `ssh <mux options> <extra>` with no host yet.
    fn ssh_base(&self, extra: &[&str]) -> Command {
        let mut cmd = self.harness.ssh_base(&[
            "-o",
            "ControlMaster=auto",
            "-o",
            &format!("ControlPath={}/%C", self.harness.control_dir().display()),
            "-o",
            "ControlPersist=60",
        ]);
        cmd.args(extra);
        cmd
    }

    fn ssh(&self, extra: &[&str], remote: &str) -> Command {
        let mut cmd = self.ssh_base(extra);
        cmd.arg(self.harness.host()).arg(remote);
        cmd
    }

    fn run(&self, remote: &str) -> Output {
        self.ssh(&["-T"], remote)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The master's pid from `ssh -O check`; gives up on a frozen master.
    fn check(&self) -> Option<i32> {
        let mut child = Reaped(
            self.ssh_base(&["-O", "check"])
                .arg(self.harness.host())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = Collector::spawn(child.0.stderr.take().unwrap());
        wait_timeout(&mut child.0, Duration::from_secs(2))?;
        stderr.wait_eof(Duration::from_secs(1));
        let text = stderr.text();
        text.split("pid=").nth(1)?.split(')').next()?.parse().ok()
    }

    fn signal(&self, signal: i32) {
        // kill(0, …) would signal our own process group.
        assert!(self.pid > 0, "no master to signal");
        // SAFETY: plain kill(2) on a pid this test started.
        unsafe { libc::kill(self.pid, signal) };
    }

    fn socket_exists(&self) -> bool {
        fs::read_dir(self.harness.control_dir())
            .unwrap()
            .next()
            .is_some()
    }
}

impl Drop for Master<'_> {
    fn drop(&mut self) {
        // A stale-socket test leaves a newer master behind the same path.
        if let Some(pid) = self.check() {
            // SAFETY: plain kill(2) on a pid this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        if self.pid > 0 {
            self.signal(libc::SIGKILL);
        }
    }
}

fn average_exec_time(runs: u32, mut exec: impl FnMut()) -> Duration {
    let start = Instant::now();
    for _ in 0..runs {
        exec();
    }
    start.elapsed() / runs
}

#[test]
fn control_master_makes_repeated_exec_cheaper() {
    let harness = ssh_harness!();
    let direct = average_exec_time(5, || {
        assert!(harness.run(&[], "true").status.success());
    });
    let master = Master::start(&harness);
    let muxed = average_exec_time(5, || {
        assert!(master.run("true").status.success());
    });

    eprintln!("exec without master {direct:?}, through master {muxed:?}");
    assert!(muxed < direct, "muxed {muxed:?} vs direct {direct:?}");
}

#[test]
fn control_persist_master_does_not_hold_the_callers_pipes() {
    let harness = ssh_harness!();
    let no_master_yet = Master::adopt(&harness);

    // `output()` waits for EOF on stdout and stderr; a backgrounded master
    // that kept them open would hang it.
    let start = Instant::now();
    let out = no_master_yet.run("echo first");
    assert_eq!(stdout_of(&out), "first\n");
    assert!(start.elapsed() < Duration::from_secs(5));

    let master = Master::adopt(&harness);
    assert!(master.pid > 0, "master persists after the caller");
}

#[test]
fn stale_master_socket_is_replaced_transparently() {
    let harness = ssh_harness!();
    let master = Master::start(&harness);
    master.signal(libc::SIGKILL);
    thread::sleep(Duration::from_millis(200));
    assert!(master.socket_exists(), "SIGKILL leaves the socket behind");

    let out = master.run("echo after-stale");

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_of(&out), "after-stale\n");
    assert_eq!(stderr_of(&out), "");
}

#[test]
fn sessions_through_a_master_die_when_it_exits() {
    let harness = ssh_harness!();
    let master = Master::start(&harness);
    let mut session = Reaped(
        master
            .ssh(&["-T"], "echo ready; exec cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = Collector::spawn(session.0.stdout.take().unwrap());
    assert!(stdout.wait_for("ready", Duration::from_secs(5)));

    // What a user's `ssh -O exit host` would do to a channel riding their master.
    master.signal(libc::SIGTERM);

    let status = wait_timeout(&mut session.0, Duration::from_secs(5)).expect("session ends");
    assert_eq!(status.code(), Some(255));
}

#[test]
fn frozen_master_hangs_new_sessions_without_connect_timeout() {
    let harness = ssh_harness!();
    let master = Master::start(&harness);
    master.signal(libc::SIGSTOP);

    let mut hung = Reaped(
        master
            .ssh(&["-T"], "echo through-master")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    assert!(
        wait_timeout(&mut hung.0, Duration::from_secs(3)).is_none(),
        "exec through a frozen master should hang"
    );
}

#[test]
fn connect_timeout_falls_back_to_a_direct_connection_past_a_frozen_master() {
    let harness = ssh_harness!();
    let master = Master::start(&harness);
    master.signal(libc::SIGSTOP);

    let start = Instant::now();
    let out = master
        .ssh(&["-T", "-o", "ConnectTimeout=1"], "echo direct")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    eprintln!("fell back after {:?}", start.elapsed());
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_of(&out), "direct\n");
    assert!(
        stderr_of(&out).contains("master hello exchange failed"),
        "{out:?}"
    );
}

#[test]
fn control_path_over_the_socket_length_limit_fails() {
    let harness = ssh_harness!();
    let long_dir = harness.control_dir().join("x".repeat(80));
    fs::create_dir(&long_dir).unwrap();
    let control_path = format!("ControlPath={}/%C", long_dir.display());

    let out = harness.run(
        &["-o", "ControlMaster=auto", "-o", &control_path],
        "echo unreachable",
    );

    assert_eq!(out.status.code(), Some(255));
    assert!(stderr_of(&out).contains("ControlPath too long"), "{out:?}");
    assert!(long_dir.read_dir().unwrap().next().is_none());
}
