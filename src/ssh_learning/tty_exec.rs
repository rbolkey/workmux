//! Q: does `ssh -t` give the remote a TTY with our workdir, env and size,
//! and does a local resize reach it?

use std::process::Stdio;
use std::time::Duration;

use super::harness::{Pty, Reaped, local_sshd, ssh_harness, stderr_of, stdout_of};

#[test]
fn tty_exec_runs_in_workdir_with_env_and_initial_size() {
    let harness = ssh_harness!();
    let pty = Pty::open(30, 100);
    let workdir = harness.remote_dir();

    // Workdir and env travel inside the command string: `cd` then `env`.
    let remote = format!(
        "cd '{workdir}' && exec env WMX_PANE=pane-1 sh -c \
         'echo \"pwd=$(pwd -P)\"; echo \"env=$WMX_PANE\"; echo \"size=$(stty size)\"; \
          tty >/dev/null && echo has-tty; echo done'"
    );
    let _child = Reaped(pty.spawn(&mut harness.ssh(&["-t"], &remote)));
    let screen = pty.reader();

    assert!(
        screen.wait_for("done", Duration::from_secs(5)),
        "{}",
        screen.text()
    );
    let text = screen.text();
    let workdir_physical = stdout_of(&harness.run(&[], &format!("cd '{workdir}' && pwd -P")));
    assert!(
        text.contains(&format!("pwd={}\r\n", workdir_physical.trim())),
        "{text}"
    );
    assert!(text.contains("env=pane-1\r\n"), "{text}");
    assert!(text.contains("size=30 100\r\n"), "{text}");
    assert!(text.contains("has-tty\r\n"), "{text}");
}

#[test]
fn local_resize_propagates_to_the_remote_tty() {
    let harness = ssh_harness!();
    let pty = Pty::open(30, 100);

    let remote = "exec sh -c 'trap \"echo resized-to=\\$(stty size)\" WINCH; \
                  echo ready; while :; do sleep 0.05; done'";
    let _child = Reaped(pty.spawn(&mut harness.ssh(&["-t"], remote)));
    let screen = pty.reader();
    assert!(
        screen.wait_for("ready", Duration::from_secs(5)),
        "{}",
        screen.text()
    );

    pty.resize(42, 133);

    assert!(
        screen.wait_for("resized-to=42 133", Duration::from_secs(5)),
        "{}",
        screen.text()
    );
}

#[test]
fn dash_t_without_a_local_tty_allocates_none_unless_forced() {
    let harness = ssh_harness!();
    let check = "tty >/dev/null && echo has-tty || echo no-tty";

    let out = harness
        .ssh(&["-t"], check)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(stdout_of(&out), "no-tty\n");

    let out = harness
        .ssh(&["-tt"], check)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // A remote TTY translates newlines to CRLF.
    assert_eq!(stdout_of(&out), "has-tty\r\n");
}

#[test]
fn send_env_only_passes_variables_the_server_accepts() {
    let harness = ssh_harness!();
    // The harness's sshd has `AcceptEnv WMX_ACCEPTED_*` and nothing else.
    local_sshd!(harness);

    let out = harness
        .ssh(
            &["-T", "-o", "SendEnv=WMX_ACCEPTED_A WMX_REJECTED_B"],
            "echo \"a=$WMX_ACCEPTED_A b=$WMX_REJECTED_B\"",
        )
        .env("WMX_ACCEPTED_A", "1")
        .env("WMX_REJECTED_B", "2")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(stdout_of(&out), "a=1 b=\n");
    assert_eq!(stderr_of(&out), "");
}
