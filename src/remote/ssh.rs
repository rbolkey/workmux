//! The SSH transport: reaches a remote through the system `ssh`, so the
//! user's SSH config (keys, agents, jump hosts, ControlMaster) applies.

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use super::transport::{StdioProcess, Transport, TtyExec};

/// Printed on its own line before a captured command runs, so shell-startup
/// output before it can be discarded (as the control channel's sentinel is).
const EXEC_MARKER: &str = "WORKMUX-EXEC";

/// Options for commands that run without the user at a terminal: never
/// prompt, and give up on a host that does not answer instead of hanging
/// (a frozen ControlMaster hangs new sessions without a timeout).
const NON_INTERACTIVE: &[&str] = &["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"];

/// Keepalives on a control channel. Without them a silent drop (a dead
/// network path) is never noticed.
#[derive(Clone, Copy)]
struct Keepalive {
    /// Seconds between keepalives.
    interval: u32,
    /// How many may go unanswered before ssh gives up.
    count: u32,
}

const KEEPALIVE: Keepalive = Keepalive {
    interval: 15,
    count: 3,
};

/// The remote directory an uploaded workmux of this release lives in: a
/// per-user cache directory workmux owns, never a directory on the user's
/// PATH. The remote shell expands it.
pub fn uploaded_workmux_dir() -> String {
    format!(
        "${{XDG_CACHE_HOME:-$HOME/.cache}}/workmux/bin/{}",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Clone)]
pub struct Ssh {
    /// Host as the user's SSH config knows it, such as `devbox` or `me@devbox`.
    target: String,
    /// An `ssh_config` to use instead of the user's, for tests.
    config: Option<PathBuf>,
    keepalive: Keepalive,
    /// Whether remote commands look for workmux in
    /// [`uploaded_workmux_dir`] before the remote's PATH.
    uploaded_workmux: bool,
}

impl Ssh {
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            config: None,
            keepalive: KEEPALIVE,
            uploaded_workmux: false,
        }
    }

    /// Prefer the workmux uploaded to the remote's cache directory, if any,
    /// to the one on its PATH.
    pub fn with_uploaded_workmux(mut self, uploaded: bool) -> Self {
        self.uploaded_workmux = uploaded;
        self
    }

    /// Copy `binary` to the remote as [`uploaded_workmux_dir`]`/workmux`,
    /// replacing only a copy uploaded before.
    pub fn upload_workmux(&self, binary: &std::path::Path) -> Result<()> {
        let file = std::fs::File::open(binary)
            .with_context(|| format!("failed to read {}", binary.display()))?;
        // Written beside its final name and moved there, so a running copy
        // is never truncated and a failed upload leaves none half written.
        let remote_command = format!(
            "dir=\"{}\" && mkdir -p \"$dir\" && tmp=\"$dir/.workmux.$$\" && \
             cat > \"$tmp\" && chmod 755 \"$tmp\" && mv -f \"$tmp\" \"$dir/workmux\"",
            uploaded_workmux_dir()
        );
        let output = self
            .ssh(NON_INTERACTIVE, &remote_command)
            .stdin(file)
            .output()
            .context("failed to run ssh")?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to upload workmux to {}: {}",
                self.target,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    /// `command` as the remote's `sh` should run it.
    fn remote_command(&self, command: &str) -> String {
        if self.uploaded_workmux {
            format!(
                "PATH=\"{}:$PATH\"; export PATH; {command}",
                uploaded_workmux_dir()
            )
        } else {
            command.to_string()
        }
    }

    /// `ssh [-F config] <options> -- <target> <remote command>`. sshd hands
    /// the command to the user's login shell, whatever it is, so it travels
    /// as `exec sh -c <command>` and only `sh` parses `remote_command`.
    fn ssh(&self, options: &[&str], remote_command: &str) -> Command {
        let mut command = Command::new("ssh");
        if let Some(config) = &self.config {
            command.arg("-F").arg(config);
        }
        command
            .args(options)
            .arg("--")
            .arg(&self.target)
            .arg(format!(
                "exec sh -c {}",
                login_shell_quote(&self.remote_command(remote_command))
            ));
        command
    }
}

/// Quote `s` as one word that POSIX shells and fish read alike. Fish
/// unescapes `\\` and `\'` inside single quotes, so backslashes and quotes
/// are escaped outside them, where every such shell reads `\\` and `\'`
/// the same way.
fn login_shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let mut quoted = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        let special = c == '\\' || c == '\'';
        if special == in_quotes {
            quoted.push('\'');
            in_quotes = !in_quotes;
        }
        if special {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    if in_quotes {
        quoted.push('\'');
    }
    quoted
}

/// The remote command string for `argv`, for `sh` to parse, so each
/// argument is quoted.
fn command_line(argv: &[String]) -> Result<String> {
    if argv.is_empty() {
        return Err(anyhow!("no command to run on the remote"));
    }
    Ok(argv
        .iter()
        .map(|arg| crate::shell::shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Quote a remote path, leaving a leading `~` for the remote shell to expand.
fn quote_path(path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => {
            format!("~/{}", crate::shell::shell_quote(&rest[1..]))
        }
        _ => crate::shell::shell_quote(path),
    }
}

/// Drop everything up to and including the exec marker line. Output without
/// the marker (ssh failed before the command ran) is returned unchanged.
fn strip_startup_noise(stdout: Vec<u8>) -> Vec<u8> {
    let mut start = 0;
    for line in stdout.split_inclusive(|&b| b == b'\n') {
        start += line.len();
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line == EXEC_MARKER.as_bytes() {
            return stdout[start..].to_vec();
        }
    }
    stdout
}

impl Transport for Ssh {
    fn exec_captured(&self, argv: &[String]) -> Result<Output> {
        // The leading newline ends any partial noise line.
        let remote_command = format!(
            "printf '\\n%s\\n' {EXEC_MARKER}; exec {}",
            command_line(argv)?
        );
        let mut output = self
            .ssh(NON_INTERACTIVE, &remote_command)
            .stdin(Stdio::null())
            .output()
            .context("failed to run ssh")?;
        output.stdout = strip_startup_noise(output.stdout);
        Ok(output)
    }

    fn exec_stdio(&self, argv: &[String]) -> Result<StdioProcess> {
        let mut options = NON_INTERACTIVE.to_vec();
        let interval = format!("ServerAliveInterval={}", self.keepalive.interval);
        let count = format!("ServerAliveCountMax={}", self.keepalive.count);
        // ControlPath=none keeps the channel off the user's ControlMaster, so
        // it does not die when they run `ssh -O exit`.
        options.extend(["-o", "ControlPath=none", "-o", &interval, "-o", &count]);
        StdioProcess::spawn(self.ssh(&options, &format!("exec {}", command_line(argv)?)))
    }

    fn tty_command(&self, exec: &TtyExec) -> Result<Command> {
        // Workdir and env travel in the command string: sshd drops variables
        // its AcceptEnv does not list.
        let mut remote_command = String::new();
        if let Some(workdir) = &exec.workdir {
            remote_command.push_str(&format!("cd -- {} && ", quote_path(workdir)));
        }
        remote_command.push_str("exec ");
        if !exec.env.is_empty() {
            remote_command.push_str("env ");
            for (name, value) in &exec.env {
                remote_command.push_str(&crate::shell::shell_quote(&format!("{name}={value}")));
                remote_command.push(' ');
            }
        }
        remote_command.push_str(&command_line(&exec.argv)?);
        // Interactive: the user can answer a passphrase or host-key prompt.
        Ok(self.ssh(&["-t", "-o", "ConnectTimeout=10"], &remote_command))
    }
}

/// Contract tests, env-gated like the SSH learning tests
/// (`src/ssh_learning`) and run against the same user-mode `sshd` harness:
/// `WORKMUX_TEST_SSH=1`.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::remote::channel::Handler;
    use crate::remote::connection::Connection;
    use crate::remote::handshake::{Hello, PROTOCOL_VERSION, SENTINEL};
    use crate::remote::testing::SERVE_CHILD;
    use crate::ssh_learning::harness::{
        LOCAL_BAD_KEY_HOST, Pty, Reaped, SshHarness, fish_ssh_harness, local_sshd, ssh_harness,
    };
    use serde_json::Value;
    use std::time::Duration;

    /// Shell startup that prints to stdout, as a chatty `.bashrc` does.
    const NOISY_STARTUP: &str =
        "echo 'Welcome to devbox'; printf '\\033[1mhi\\033[0m\\n'; printf partial\n";

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    pub(crate) fn transport(harness: &SshHarness) -> Ssh {
        Ssh {
            target: harness.host().to_string(),
            config: harness.ssh_config().map(PathBuf::from),
            keepalive: KEEPALIVE,
            uploaded_workmux: false,
        }
    }

    /// Puts a `workmux` running `script` first on the remote's PATH, after
    /// shell startup that prints `noise`.
    pub(crate) fn install_remote_workmux(harness: &SshHarness, noise: &str, script: &str) {
        let bin = write_remote_workmux(&format!("{}/bin", harness.remote_dir()), script);
        local_sshd!(harness).set_shell_startup(&format!("{noise}\nexport PATH={bin}:$PATH\n"));
    }

    /// [`install_remote_workmux`] for a remote whose login shell is fish.
    fn install_remote_workmux_for_fish(harness: &SshHarness, noise: &str, script: &str) {
        let bin = write_remote_workmux(&format!("{}/bin", harness.remote_dir()), script);
        local_sshd!(harness).set_fish_startup(&format!("{noise}\nset -gx PATH {bin} $PATH\n"));
    }

    /// Writes a `workmux` running `script` into `bin` and returns `bin`.
    fn write_remote_workmux(bin: &str, script: &str) -> String {
        std::fs::create_dir_all(bin).unwrap();
        let workmux = format!("{bin}/workmux");
        std::fs::write(&workmux, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(
            &workmux,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        bin.to_string()
    }

    /// A remote `workmux` that serves a real control channel: this test
    /// binary's serve child, whose libtest output is more noise.
    pub(crate) fn serve_script() -> String {
        format!(
            "{}={SERVE_CHILD} exec '{}' {SERVE_CHILD} --exact --nocapture --ignored",
            crate::test_support::ISOLATED_TEST_ENV,
            std::env::current_exe().unwrap().display()
        )
    }

    struct NoRequests;

    impl Handler for NoRequests {
        fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
            Err(format!("unknown method: {method}"))
        }
    }

    fn open(harness: &SshHarness) -> Result<Connection> {
        Connection::open(
            harness.host(),
            &transport(harness),
            NoRequests,
            Duration::from_secs(10),
        )
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn captured_exec_passes_each_argument_intact_and_returns_the_exit_status() {
        let harness = ssh_harness!();

        let output = transport(&harness)
            .exec_captured(&argv(&[
                "sh",
                "-c",
                "printf '%s|' \"$@\"; exit 3",
                "sh",
                "two words",
                "it's",
                "$HOME",
                "",
            ]))
            .unwrap();

        assert_eq!(stdout(&output), "two words|it's|$HOME||");
        assert_eq!(output.status.code(), Some(3));
    }

    /// Arguments a login shell's quoting could alter: backslashes (which
    /// fish unescapes inside single quotes), quotes, spaces and expansions.
    const AWKWARD_ARGS: &[&str] = &[
        "back\\slash",
        "two\\\\backslashes",
        "trailing\\",
        "it's",
        "\\'",
        "say \"hi\"",
        "two words",
        "$HOME (x) *",
        "~",
        "",
        "line\nbreak",
    ];

    #[test]
    fn captured_exec_on_a_fish_login_shell_passes_each_argument_intact() {
        let harness = fish_ssh_harness!();
        let mut command = argv(&["sh", "-c", "printf '%s|' \"$@\"; exit 3", "sh"]);
        command.extend(argv(AWKWARD_ARGS));

        let output = transport(&harness).exec_captured(&command).unwrap();

        let expected: String = AWKWARD_ARGS.iter().map(|arg| format!("{arg}|")).collect();
        assert_eq!(
            stdout(&output),
            expected,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn captured_exec_output_excludes_shell_startup_noise() {
        let harness = ssh_harness!();
        local_sshd!(harness).set_shell_startup(NOISY_STARTUP);

        let output = transport(&harness)
            .exec_captured(&argv(&["echo", "clean"]))
            .unwrap();

        assert_eq!(stdout(&output), "clean\n");
        assert!(output.status.success());
    }

    #[test]
    fn a_control_channel_opens_through_a_chatty_shell_startup() {
        let harness = ssh_harness!();
        install_remote_workmux(&harness, NOISY_STARTUP, &serve_script());

        let connection = open(&harness).unwrap();
        let ping = connection.ping().unwrap();

        assert_eq!(connection.hello, Hello::current());
        assert_eq!(ping.workmux, env!("CARGO_PKG_VERSION"));
    }

    /// Fish startup that prints to stdout, as a chatty `config.fish` does.
    const NOISY_FISH_STARTUP: &str = "echo 'Welcome to devbox'; printf partial\n";

    #[test]
    fn a_control_channel_opens_through_a_chatty_fish_login_shell() {
        let harness = fish_ssh_harness!();
        install_remote_workmux_for_fish(&harness, NOISY_FISH_STARTUP, &serve_script());

        let connection = open(&harness).unwrap();

        assert!(connection.ping().is_ok());
    }

    #[test]
    fn an_uploaded_workmux_runs_on_a_fish_login_shell() {
        let harness = fish_ssh_harness!();
        let cache = format!("{}/cache", harness.remote_dir());
        local_sshd!(harness).set_fish_startup(&format!("set -gx XDG_CACHE_HOME {cache}\n"));
        let script = harness.local().unwrap().file("uploaded-workmux");
        std::fs::write(&script, "#!/bin/sh\nprintf 'uploaded %s|' \"$@\"\n").unwrap();
        let ssh = transport(&harness).with_uploaded_workmux(true);

        ssh.upload_workmux(&script).unwrap();
        let output = ssh
            .exec_captured(&argv(&["workmux", "a\\b", "it's"]))
            .unwrap();

        assert_eq!(stdout(&output), "uploaded a\\b|uploaded it's|");
    }

    #[test]
    fn missing_workmux_names_the_version_to_install() {
        let harness = ssh_harness!();
        local_sshd!(harness).set_shell_startup("export PATH=/usr/bin:/bin");

        let error = open(&harness).err().unwrap().to_string();

        assert!(
            error.contains(&format!(
                "workmux is not installed on {}: install workmux {}",
                harness.host(),
                env!("CARGO_PKG_VERSION")
            )),
            "{error}"
        );
    }

    #[test]
    fn a_workmux_too_old_to_serve_names_the_version_to_install() {
        let harness = ssh_harness!();
        install_remote_workmux(
            &harness,
            "",
            "echo \"error: unrecognized subcommand 'remote'\" >&2; exit 2",
        );

        let error = open(&harness).err().unwrap().to_string();

        assert!(
            error.contains("unrecognized subcommand 'remote'"),
            "{error}"
        );
        assert!(
            error.contains(&format!("install workmux {}", env!("CARGO_PKG_VERSION"))),
            "{error}"
        );
    }

    #[test]
    fn a_protocol_mismatch_names_the_version_to_install() {
        let harness = ssh_harness!();
        install_remote_workmux(
            &harness,
            NOISY_STARTUP,
            &format!(
                "printf '\\n{SENTINEL} {{\"workmux\":\"9.9.9\",\"protocol\":{}}}\\n'; exec cat >/dev/null",
                PROTOCOL_VERSION + 1
            ),
        );

        let error = open(&harness).err().unwrap().to_string();

        assert!(error.contains("runs workmux 9.9.9"), "{error}");
        assert!(
            error.contains(&format!("install workmux {}", env!("CARGO_PKG_VERSION"))),
            "{error}"
        );
    }

    /// Runs `exec` on a local terminal and returns what it printed.
    fn run_on_a_terminal(harness: &SshHarness, exec: &TtyExec) -> String {
        let pty = Pty::open(30, 100);
        let mut command = transport(harness).tty_command(exec).unwrap();
        let _child = Reaped(pty.spawn(&mut command));
        let screen = pty.reader();
        assert!(
            screen.wait_for("done", Duration::from_secs(5)),
            "{}",
            screen.text()
        );
        screen.text()
    }

    #[test]
    fn tty_exec_runs_in_the_workdir_with_the_env_on_a_terminal() {
        let harness = ssh_harness!();
        let workdir = format!("{}/my repo", harness.remote_dir());
        harness.run(&[], &format!("mkdir '{workdir}'"));

        let text = run_on_a_terminal(
            &harness,
            &TtyExec {
                argv: argv(&[
                    "sh",
                    "-c",
                    "echo \"pwd=$(pwd)\"; echo \"env=$WM_PANE\"; tty >/dev/null && echo has-tty; echo done",
                ]),
                workdir: Some(workdir.clone()),
                env: vec![("WM_PANE".to_string(), "it's pane 1".to_string())],
            },
        );

        let expected = stdout(&harness.run(&[], &format!("cd '{workdir}' && pwd")));
        assert!(
            text.contains(&format!("pwd={}\r\n", expected.trim())),
            "{text}"
        );
        assert!(text.contains("env=it's pane 1\r\n"), "{text}");
        assert!(text.contains("has-tty\r\n"), "{text}");
    }

    #[test]
    fn a_tty_workdir_under_home_is_expanded_on_the_remote() {
        let harness = ssh_harness!();
        let home = stdout(&harness.run(&[], "printf %s \"$HOME\""));

        let text = run_on_a_terminal(
            &harness,
            &TtyExec {
                argv: argv(&["sh", "-c", "echo \"pwd=$(pwd)\"; echo done"]),
                workdir: Some("~".to_string()),
                env: vec![],
            },
        );

        assert!(text.contains(&format!("pwd={home}\r\n")), "{text}");
    }

    #[test]
    fn tty_exec_on_a_fish_login_shell_keeps_the_workdir_env_and_arguments() {
        let harness = fish_ssh_harness!();
        let workdir = format!("{}/my \\ repo", harness.remote_dir());
        std::fs::create_dir(&workdir).unwrap();

        let text = run_on_a_terminal(
            &harness,
            &TtyExec {
                argv: argv(&[
                    "sh",
                    "-c",
                    "echo \"pwd=$(pwd -P)\"; echo \"env=$WM_PANE\"; echo \"arg=$1\"; echo done",
                    "sh",
                    "a\\'b",
                ]),
                workdir: Some(workdir.clone()),
                env: vec![("WM_PANE".to_string(), "it's \\ pane".to_string())],
            },
        );

        let expected = std::fs::canonicalize(&workdir).unwrap();
        assert!(
            text.contains(&format!("pwd={}\r\n", expected.display())),
            "{text}"
        );
        assert!(text.contains("env=it's \\ pane\r\n"), "{text}");
        assert!(text.contains("arg=a\\'b\r\n"), "{text}");
    }

    /// The pid of the remote sshd session behind a control channel: the
    /// parent of the remote workmux the channel pings.
    fn session_pid(harness: &SshHarness, connection: &Connection) -> String {
        let serve = connection.ping().unwrap().pid;
        stdout(&harness.run(&[], &format!("ps -o ppid= -p {serve}")))
            .trim()
            .to_string()
    }

    fn wait_closed(connection: &Connection, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !connection.is_closed() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn a_killed_remote_session_closes_the_channel_in_ssh_words() {
        let harness = ssh_harness!();
        install_remote_workmux(&harness, "", &serve_script());
        let connection = open(&harness).unwrap();

        harness.remote_kill("KILL", &session_pid(&harness, &connection));

        assert!(wait_closed(&connection, Duration::from_secs(5)));
        let failure = connection.failure();
        assert!(failure.contains("closed by remote host"), "{failure}");
    }

    #[test]
    fn a_silent_drop_is_noticed_through_keepalives() {
        let harness = ssh_harness!();
        install_remote_workmux(&harness, "", &serve_script());
        let mut transport = transport(&harness);
        transport.keepalive = Keepalive {
            interval: 1,
            count: 2,
        };
        let connection = Connection::open(
            harness.host(),
            &transport,
            NoRequests,
            Duration::from_secs(10),
        )
        .unwrap();
        let session = session_pid(&harness, &connection);

        // A stopped session stands in for a dead network path.
        harness.remote_kill("STOP", &session);
        let closed = wait_closed(&connection, Duration::from_secs(10));
        harness.remote_kill("KILL", &session);

        assert!(closed, "the channel never noticed the drop");
    }

    #[test]
    fn the_channel_survives_the_users_control_master_exiting() {
        let harness = ssh_harness!();
        install_remote_workmux(&harness, "", &serve_script());
        let config = harness.control_dir().join("ssh_config");
        std::fs::write(
            &config,
            format!(
                "Host *\n  ControlMaster auto\n  ControlPath {}/%C\n  ControlPersist 60\n\
                 Include {}\n",
                harness.control_dir().display(),
                harness.ssh_config().unwrap().display()
            ),
        )
        .unwrap();
        let transport = Ssh {
            config: Some(config.clone()),
            ..transport(&harness)
        };
        // Captured exec starts the user's master; the channel must not use it.
        assert!(
            transport
                .exec_captured(&argv(&["true"]))
                .unwrap()
                .status
                .success()
        );
        let connection = Connection::open(
            harness.host(),
            &transport,
            NoRequests,
            Duration::from_secs(10),
        )
        .unwrap();

        let exit = Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .args(["-O", "exit", harness.host()])
            .output()
            .unwrap();
        assert!(exit.status.success(), "{exit:?}");

        assert!(!wait_closed(&connection, Duration::from_millis(500)));
        connection.ping().unwrap();
    }

    #[test]
    fn an_auth_failure_is_reported_in_ssh_words() {
        let harness = ssh_harness!();
        local_sshd!(harness);
        let transport = Ssh {
            target: LOCAL_BAD_KEY_HOST.to_string(),
            ..transport(&harness)
        };

        let error = Connection::open("devbox", &transport, NoRequests, Duration::from_secs(10))
            .err()
            .unwrap()
            .to_string();

        assert!(error.contains("Permission denied (publickey)"), "{error}");
    }
}
