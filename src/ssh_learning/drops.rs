//! Q: how does a killed connection surface on each side (exit codes,
//! timing), and what does the remote process see (SIGHUP, EOF, SIGPIPE)?

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use super::harness::{Collector, Reaped, SshHarness, ssh_harness, stderr_of, wait_timeout};

/// Remote script that records a hangup or end of input in `marker`.
fn record_hup_or_eof(marker: &str) -> String {
    format!(
        "exec sh -c 'trap \"echo HUP >> {marker}; exit 129\" HUP; echo ready; \
         while IFS= read -r line; do :; done; echo EOF >> {marker}'"
    )
}

fn spawn_piped(harness: &SshHarness, options: &[&str], remote: &str) -> (Reaped, Collector) {
    let mut child = harness
        .ssh(options, remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = Collector::spawn(child.stdout.take().unwrap());
    (Reaped(child), stdout)
}

fn stderr_collector(child: &mut Child) -> Collector {
    Collector::spawn(child.stderr.take().unwrap())
}

/// Starts `ssh -T host` printing the pid of its remote sshd session process.
fn spawn_with_session_pid(harness: &SshHarness, options: &[&str]) -> (Reaped, String) {
    let mut opts = vec!["-T"];
    opts.extend_from_slice(options);
    let (child, stdout) = spawn_piped(harness, &opts, "echo \"session=$PPID\"; exec cat");
    assert!(
        stdout.wait_for("\n", Duration::from_secs(5)),
        "{}",
        stdout.text()
    );
    let pid = stdout
        .text()
        .trim()
        .strip_prefix("session=")
        .expect("session pid line")
        .to_string();
    (child, pid)
}

/// Kills remote processes on drop, so a failing test leaves none stopped.
struct KillOnDrop<'a>(&'a SshHarness, Vec<String>);

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        for pid in &self.1 {
            self.0.remote_kill("KILL", pid);
        }
    }
}

#[test]
fn killing_local_ssh_without_tty_gives_remote_eof_not_sighup() {
    let harness = ssh_harness!();
    let marker = format!("{}/marker", harness.remote_dir());
    let (mut child, stdout) = spawn_piped(&harness, &["-T"], &record_hup_or_eof(&marker));
    assert!(stdout.wait_for("ready", Duration::from_secs(5)));

    child.0.kill().unwrap();

    let seen = harness.wait_remote_file(&marker, Duration::from_secs(5), |s| !s.is_empty());
    assert_eq!(seen, "EOF\n");
}

#[test]
fn killing_local_ssh_with_tty_sends_sighup_to_remote() {
    let harness = ssh_harness!();
    let marker = format!("{}/marker", harness.remote_dir());
    let (mut child, stdout) = spawn_piped(&harness, &["-tt"], &record_hup_or_eof(&marker));
    assert!(stdout.wait_for("ready", Duration::from_secs(5)));

    child.0.kill().unwrap();

    let seen = harness.wait_remote_file(&marker, Duration::from_secs(5), |s| !s.is_empty());
    assert_eq!(seen, "HUP\n");
}

#[test]
fn remote_writer_gets_sigpipe_after_local_ssh_dies() {
    let harness = ssh_harness!();
    let marker = format!("{}/marker", harness.remote_dir());
    let remote = format!(
        "exec sh -c 'trap \"\" PIPE; echo ready; \
         while echo tick; do sleep 0.05; done; echo write-failed >> {marker}'"
    );
    let (mut child, stdout) = spawn_piped(&harness, &["-T"], &remote);
    assert!(stdout.wait_for("ready", Duration::from_secs(5)));

    child.0.kill().unwrap();

    let seen = harness.wait_remote_file(&marker, Duration::from_secs(5), |s| !s.is_empty());
    assert_eq!(seen, "write-failed\n");
}

#[test]
fn remote_session_killed_exits_ssh_255_promptly() {
    let harness = ssh_harness!();
    let (mut child, session_pid) = spawn_with_session_pid(&harness, &[]);
    let stderr = stderr_collector(&mut child.0);

    let start = Instant::now();
    harness.remote_kill("KILL", &session_pid);
    let status = wait_timeout(&mut child.0, Duration::from_secs(5)).expect("ssh exits");

    eprintln!(
        "ssh noticed the dropped session after {:?}",
        start.elapsed()
    );
    assert_eq!(status.code(), Some(255));
    assert!(stderr.wait_eof(Duration::from_secs(1)));
    assert!(
        stderr.text().contains("closed by remote host"),
        "{}",
        stderr.text()
    );
}

#[test]
fn silent_drop_is_noticed_only_with_server_alive_interval() {
    let harness = ssh_harness!();
    let (mut plain, plain_pid) = spawn_with_session_pid(&harness, &[]);
    let (mut alive, alive_pid) = spawn_with_session_pid(
        &harness,
        &[
            "-o",
            "ServerAliveInterval=1",
            "-o",
            "ServerAliveCountMax=2",
            // The timeout message is logged at INFO.
            "-o",
            "LogLevel=INFO",
        ],
    );
    let alive_stderr = stderr_collector(&mut alive.0);

    // A stopped sshd session neither answers nor closes the TCP connection,
    // like a network path that went dark.
    let _cleanup = KillOnDrop(&harness, vec![plain_pid.clone(), alive_pid.clone()]);
    let start = Instant::now();
    harness.remote_kill("STOP", &plain_pid);
    harness.remote_kill("STOP", &alive_pid);

    let status = wait_timeout(&mut alive.0, Duration::from_secs(10)).expect("keepalive ssh exits");
    let detected_after = start.elapsed();
    eprintln!("ServerAliveInterval=1 CountMax=2 noticed the drop after {detected_after:?}");
    assert_eq!(status.code(), Some(255));
    assert!(alive_stderr.wait_eof(Duration::from_secs(1)));
    assert!(
        alive_stderr.text().contains("not responding"),
        "{}",
        alive_stderr.text()
    );

    assert!(
        plain.0.try_wait().unwrap().is_none(),
        "without keepalives ssh is still waiting after {detected_after:?}"
    );
}

#[test]
fn remote_command_killed_by_signal_exits_255_without_a_message() {
    let harness = ssh_harness!();

    let out = harness.run(&[], "kill -TERM $$");

    // Indistinguishable by exit code from an ssh-level failure.
    assert_eq!(out.status.code(), Some(255));
    assert_eq!(stderr_of(&out), "");
}
