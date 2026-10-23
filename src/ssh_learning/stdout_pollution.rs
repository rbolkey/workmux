//! Q: with a chatty shell startup on the remote, what arrives before the
//! command's own output, does a sentinel hello line (ADR 0001) recover the
//! stream, and does `sh -c 'exec …'` reduce the noise?

use std::process::Stdio;
use std::time::Duration;

use super::harness::{Collector, Reaped, local_sshd, ssh_harness, stderr_of, stdout_of};

const SENTINEL: &str = "WORKMUX-HELLO";

/// Discards everything up to and including the first line that is exactly
/// the sentinel, returning the rest of the stream.
fn after_sentinel(stream: &str) -> Option<&str> {
    let mut offset = 0;
    for line in stream.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == SENTINEL {
            return Some(&stream[offset..]);
        }
    }
    None
}

#[test]
fn shell_startup_output_arrives_on_stdout_before_command_output() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    sshd.set_shell_startup("echo 'Welcome to the remote'\n");

    let out = harness.run(&[], "echo command-output");

    assert_eq!(stdout_of(&out), "Welcome to the remote\ncommand-output\n");
    assert!(!stderr_of(&out).contains("Welcome"));
}

#[test]
fn sh_exec_wrapper_does_not_avoid_startup_output() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    sshd.set_shell_startup("echo 'Welcome to the remote'\n");

    // sshd always hands the command string to the user's login shell, which
    // runs its startup files before it ever reaches `sh`.
    let out = harness.run(&[], "sh -c 'exec echo command-output'");

    assert_eq!(stdout_of(&out), "Welcome to the remote\ncommand-output\n");
}

#[test]
fn startup_output_without_newline_joins_the_first_output_line() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    sshd.set_shell_startup("printf 'no trailing newline'\n");

    let out = harness.run(&[], &format!("echo {SENTINEL}"));

    assert_eq!(stdout_of(&out), format!("no trailing newline{SENTINEL}\n"));
    assert_eq!(after_sentinel(&stdout_of(&out)), None);
}

#[test]
fn sentinel_after_a_leading_newline_recovers_the_stream() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    sshd.set_shell_startup(
        "echo 'Last login: never'\nprintf '\\033[1;32mfortune\\033[0m'\nprintf 'partial'\n",
    );

    // The remote prints a newline before the sentinel, so it always starts
    // a fresh line even after unterminated noise.
    let out = harness.run(
        &[],
        &format!(r#"printf '\n{SENTINEL}\n{{"jsonrpc":"hello"}}\n'"#),
    );

    let stream = stdout_of(&out);
    assert!(stream.starts_with("Last login: never\n"));
    assert_eq!(after_sentinel(&stream), Some("{\"jsonrpc\":\"hello\"}\n"));
}

#[test]
fn login_banner_goes_to_stderr() {
    let harness = ssh_harness!();
    local_sshd!(harness);

    let out = harness.run(&["-o", "LogLevel=INFO"], "echo command-output");

    assert_eq!(stdout_of(&out), "command-output\n");
    assert!(stderr_of(&out).contains("WMX-BANNER"));
}

#[test]
fn startup_output_reaches_a_long_lived_pipe_first_too() {
    let harness = ssh_harness!();
    let sshd = local_sshd!(harness);
    sshd.set_shell_startup("echo noise\n");

    let mut child = Reaped(
        harness
            .ssh(&["-T"], &format!("printf '\\n{SENTINEL}\\n'; exec cat"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = Collector::spawn(child.0.stdout.take().unwrap());
    let _stdin = child.0.stdin.take();

    assert!(stdout.wait_for(&format!("{SENTINEL}\n"), Duration::from_secs(5)));
    assert_eq!(stdout.text(), format!("noise\n\n{SENTINEL}\n"));
}
