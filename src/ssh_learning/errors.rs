//! Q: what do host unreachable, auth failure and a missing remote command
//! look like, so the control channel can report them distinctly?
//!
//! Every ssh-level failure exits 255; only stderr tells them apart. ssh ends
//! its own messages with CRLF. A remote command that is missing exits 127
//! from the remote shell instead.

use std::fs;
use std::time::{Duration, Instant};

use super::harness::{
    LOCAL_BAD_KEY_HOST, free_port, local_sshd, ssh_harness, stderr_of, stdout_of,
};

#[test]
fn connection_refused() {
    let harness = ssh_harness!();
    let port = free_port().to_string();

    let out = harness.run(&["-o", "HostName=127.0.0.1", "-p", &port], "true");

    assert_eq!(out.status.code(), Some(255));
    assert_eq!(
        stderr_of(&out),
        format!("ssh: connect to host 127.0.0.1 port {port}: Connection refused\r\n")
    );
}

#[test]
fn unresolvable_host() {
    let harness = ssh_harness!();

    let out = harness.run(&["-o", "HostName=wmx-nonexistent.invalid"], "true");

    assert_eq!(out.status.code(), Some(255));
    assert!(
        stderr_of(&out).starts_with("ssh: Could not resolve hostname wmx-nonexistent.invalid: "),
        "{out:?}"
    );
}

#[test]
fn unreachable_host_times_out_after_connect_timeout() {
    let harness = ssh_harness!();

    // A non-routed private address: packets vanish rather than being refused.
    let start = Instant::now();
    let out = harness.run(
        &["-o", "HostName=10.255.255.1", "-o", "ConnectTimeout=1"],
        "true",
    );

    eprintln!("gave up after {:?}: {}", start.elapsed(), stderr_of(&out));
    assert_eq!(out.status.code(), Some(255));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(stderr_of(&out).contains("timed out"), "{out:?}");
}

#[test]
fn auth_failure() {
    let harness = ssh_harness!();
    local_sshd!(harness);

    let out = harness
        .ssh_base(&["-T"])
        .args([LOCAL_BAD_KEY_HOST, "true"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(255));
    assert!(
        stderr_of(&out).ends_with(": Permission denied (publickey).\r\n"),
        "{out:?}"
    );
}

#[test]
fn unknown_host_key_under_strict_checking() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    let known_hosts = sshd.file("known_hosts");
    fs::write(&known_hosts, "").unwrap();

    let out = harness.run(
        &[
            "-o",
            &format!("UserKnownHostsFile={}", known_hosts.display()),
            "-o",
            "StrictHostKeyChecking=yes",
        ],
        "true",
    );

    assert_eq!(out.status.code(), Some(255));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("No ED25519 host key is known for"),
        "{stderr}"
    );
    assert!(
        stderr.ends_with("Host key verification failed.\r\n"),
        "{stderr}"
    );
}

#[test]
fn changed_host_key() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    // Record the client key as the server's host key: a mismatch.
    let wrong_key = fs::read_to_string(sshd.file("client_key.pub")).unwrap();
    let wrong_key: Vec<&str> = wrong_key.split_whitespace().take(2).collect();
    let known_hosts = sshd.file("known_hosts");
    fs::write(
        &known_hosts,
        format!("[127.0.0.1]:{} {}\n", sshd.port, wrong_key.join(" ")),
    )
    .unwrap();

    let out = harness.run(
        &[
            "-o",
            &format!("UserKnownHostsFile={}", known_hosts.display()),
            "-o",
            "StrictHostKeyChecking=yes",
        ],
        "true",
    );

    assert_eq!(out.status.code(), Some(255));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "{stderr}"
    );
    assert!(
        stderr.ends_with("Host key verification failed.\r\n"),
        "{stderr}"
    );
}

#[test]
fn missing_remote_command_exits_127() {
    let harness = ssh_harness!();

    let out = harness.run(&[], "workmux-missing-wmx remote --stdio");

    assert_eq!(out.status.code(), Some(127));
    assert_eq!(stdout_of(&out), "");
    // Wording depends on the remote login shell, e.g.
    // zsh: "zsh:1: command not found: X", bash: "bash: line 1: X: command not found".
    let stderr = stderr_of(&out);
    assert!(stderr.contains("not found"), "{stderr}");
    assert!(stderr.contains("workmux-missing-wmx"), "{stderr}");
}

#[test]
fn command_v_probes_for_a_remote_command_quietly() {
    let harness = ssh_harness!();

    let missing = harness.run(&[], "command -v workmux-missing-wmx");
    let present = harness.run(&[], "command -v sh");

    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(stdout_of(&missing), "");
    assert_eq!(stderr_of(&missing), "");
    assert_eq!(present.status.code(), Some(0));
    assert!(stdout_of(&present).trim_end().ends_with("/sh"));
}
