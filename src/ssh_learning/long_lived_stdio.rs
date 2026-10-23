//! Q: can `ssh -T host cmd` hold a bidirectional pipe for the control
//! channel, how fast are small messages, and what happens when either side
//! closes?

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::harness::{Collector, Reaped, ssh_harness, wait_timeout};

#[test]
fn stdio_pipe_round_trips_small_messages_quickly() {
    let harness = ssh_harness!();
    let mut child = Reaped(
        harness
            .ssh(&["-T"], "exec cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = Collector::spawn(child.0.stdout.take().unwrap());

    let mut latencies = Vec::new();
    for i in 0..200 {
        let line = format!("{{\"id\":{i},\"method\":\"ping\"}}\n");
        let expected = stdout.len() + line.len();
        let start = Instant::now();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        assert!(
            stdout.wait_until(Duration::from_secs(5), |text| text.len() >= expected),
            "message {i} never came back"
        );
        latencies.push(start.elapsed());
    }
    latencies.sort();
    let median = latencies[latencies.len() / 2];
    let p99 = latencies[latencies.len() * 99 / 100];
    eprintln!("round trip over ssh stdio: median {median:?}, p99 {p99:?}");
    assert!(
        p99 < Duration::from_millis(250),
        "p99 round trip {p99:?} is too slow for a control channel"
    );
}

#[test]
fn closing_local_stdin_delivers_eof_and_ssh_exits_with_remote_status() {
    let harness = ssh_harness!();
    let mut child = Reaped(
        harness
            .ssh(&["-T"], "cat >/dev/null; echo saw-eof; exit 3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = Collector::spawn(child.0.stdout.take().unwrap());
    child.0.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    // Dropping stdin closes the pipe.

    let status = wait_timeout(&mut child.0, Duration::from_secs(5)).expect("ssh exits");
    assert_eq!(status.code(), Some(3));
    assert!(stdout.wait_eof(Duration::from_secs(1)));
    assert_eq!(stdout.text(), "saw-eof\n");
}

#[test]
fn remote_exit_closes_local_stdout_and_breaks_the_pipe() {
    let harness = ssh_harness!();
    let mut child = Reaped(
        harness
            .ssh(&["-T"], "read line; echo bye; exit 0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = Collector::spawn(child.0.stdout.take().unwrap());
    stdin.write_all(b"go\n").unwrap();

    // Our stdin is still open, yet ssh exits as soon as the remote does.
    assert!(
        stdout.wait_eof(Duration::from_secs(5)),
        "stdout reaches EOF"
    );
    assert_eq!(stdout.text(), "bye\n");
    let status = wait_timeout(&mut child.0, Duration::from_secs(5)).expect("ssh exits");
    assert_eq!(status.code(), Some(0));

    // Rust ignores SIGPIPE, so further writes fail with BrokenPipe.
    let err = (0..100)
        .find_map(|_| stdin.write_all(&[b'x'; 4096]).err())
        .expect("writes eventually fail");
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
}
