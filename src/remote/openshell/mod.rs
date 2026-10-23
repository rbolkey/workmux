//! The OpenShell transport: one sandbox per project, reached through the
//! `openshell` CLI's exec. workmux creates the sandbox on first use (its
//! image, policy and providers come from the project's settings), starts it
//! when a command needs it, and stops or deletes it only when asked.
//!
//! Observed behaviour this relies on is pinned by ticket 01's and ticket
//! 21's learning tests: names of at most 19 characters, `--detach` at
//! create, exec with `--no-login-shell`, the workload running as uid 1000
//! with `HOME` and workdir `/sandbox`, and policy changes that apply in
//! place only when they widen a grant.

pub mod image;
pub mod policy;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::transport::{Provisioning, RemoteState, StdioProcess, Transport, TtyExec};
use crate::config::OpenshellRemote;

/// Where OpenShell runs every sandbox's workload, and its `HOME`.
pub const SANDBOX_HOME: &str = "/sandbox";

/// How long a sandbox may take to reach a phase.
const PHASE_TIMEOUT: Duration = Duration::from_secs(300);

/// The annotation recording the image a sandbox was created from.
pub const IMAGE_ANNOTATION: &str = "workmux.image";

pub struct OpenShell {
    sandbox: String,
    /// What the sandbox is created from, for a project's implicit remote.
    origin: Option<OpenshellRemote>,
    openshell: PathBuf,
    docker: PathBuf,
    phase_poll: Duration,
}

/// A sandbox's phase, as `sandbox get` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Missing,
    Is(String),
}

impl OpenShell {
    pub fn new(sandbox: impl Into<String>, origin: Option<OpenshellRemote>) -> Self {
        Self {
            sandbox: sandbox.into(),
            origin,
            openshell: PathBuf::from("openshell"),
            docker: PathBuf::from("docker"),
            phase_poll: Duration::from_secs(1),
        }
    }

    fn openshell(&self) -> Command {
        let mut command = Command::new(&self.openshell);
        // Errors are miette-formatted; keep them free of colour codes.
        command.env("NO_COLOR", "1");
        command
    }

    /// `openshell sandbox exec` for `argv`. `workmux` is remote workmux,
    /// which the sandbox image puts on PATH.
    fn exec(&self, tty: bool, workdir: Option<&str>, env: &[(String, String)], argv: &[String]) -> Result<Command> {
        if argv.is_empty() {
            return Err(anyhow!("no command to run on the remote"));
        }
        let mut command = self.openshell();
        command.args(["sandbox", "exec", "-n", &self.sandbox, "--no-login-shell"]);
        command.arg(if tty { "--tty" } else { "--no-tty" });
        if let Some(workdir) = workdir {
            command.arg("--workdir").arg(sandbox_path(workdir));
        }
        for (name, value) in env {
            command.arg("--env").arg(format!("{name}={value}"));
        }
        command.arg("--").args(argv);
        Ok(command)
    }

    /// Run an `openshell` command to completion; its stderr is the error.
    fn run(&self, args: &[&str]) -> Result<Output> {
        let output = self
            .openshell()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("failed to run openshell")?;
        if !output.status.success() {
            bail!("{}", failure(&output));
        }
        Ok(output)
    }

    /// The sandbox as `sandbox get` reports it, or `None` when it is missing.
    fn get(&self) -> Result<Option<Value>> {
        let output = self
            .openshell()
            .args(["sandbox", "get", &self.sandbox, "-o", "json"])
            .stdin(Stdio::null())
            .output()
            .context("failed to run openshell")?;
        if !output.status.success() {
            let error = failure(&output);
            if error.contains("not found") {
                return Ok(None);
            }
            bail!("{error}");
        }
        Ok(Some(serde_json::from_slice(&output.stdout).with_context(
            || format!("openshell sent an unreadable description of {}", self.sandbox),
        )?))
    }

    fn phase(&self) -> Result<Phase> {
        Ok(match self.get()? {
            None => Phase::Missing,
            Some(sandbox) => Phase::Is(
                sandbox["phase"].as_str().unwrap_or_default().to_string(),
            ),
        })
    }

    /// Wait until the sandbox's phase is `wanted` (`None`: until it is gone).
    fn wait_for(&self, wanted: Option<&str>) -> Result<()> {
        let deadline = Instant::now() + PHASE_TIMEOUT;
        loop {
            let phase = self.phase()?;
            match (&phase, wanted) {
                (Phase::Missing, None) => return Ok(()),
                (Phase::Is(phase), Some(wanted)) if phase == wanted => return Ok(()),
                (Phase::Is(phase), _) if phase == "Error" => {
                    bail!("sandbox {} is in phase Error", self.sandbox)
                }
                (Phase::Missing, Some(wanted)) => {
                    bail!("sandbox {} disappeared while waiting for {wanted}", self.sandbox)
                }
                _ => {}
            }
            if Instant::now() > deadline {
                bail!(
                    "sandbox {} did not reach {} within {}s; it is {phase:?}",
                    self.sandbox,
                    wanted.unwrap_or("deletion"),
                    PHASE_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(self.phase_poll);
        }
    }

    fn origin(&self) -> Result<&OpenshellRemote> {
        self.origin.as_ref().ok_or_else(|| {
            anyhow!(
                "sandbox {} does not exist; workmux creates sandboxes only for projects added with `workmux project add --openshell`",
                self.sandbox
            )
        })
    }

    /// The policy the sandbox should have: the base policy with the
    /// project's policy file merged on top.
    pub fn desired_policy(&self) -> Result<serde_yaml::Value> {
        let user = match self.origin.as_ref().and_then(|o| o.settings.policy.as_deref()) {
            Some(path) => {
                let path = crate::util::expand_tilde(path);
                Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("failed to read policy file {}", path.display()))?,
                )
            }
            None => None,
        };
        policy::effective(user.as_deref())
    }

    /// The sandbox's effective policy, as OpenShell reports it.
    pub fn current_policy(&self) -> Result<Option<Value>> {
        Ok(self.get()?.map(|sandbox| sandbox["policy"].clone()))
    }

    fn write_policy(&self, dir: &std::path::Path) -> Result<PathBuf> {
        let path = dir.join("policy.yaml");
        std::fs::write(&path, serde_yaml::to_string(&self.desired_policy()?)?)?;
        Ok(path)
    }

    /// Build the sandbox's image, returning its tag.
    fn build_image(&self, origin: &OpenshellRemote, context: &std::path::Path) -> Result<String> {
        let workmux = std::env::var_os(image::BINARY_ENV).map(PathBuf::from);
        let checkout = context.join("project");
        let devshell = origin
            .git_url
            .as_deref()
            .is_some_and(|url| self.fetch_flake(url, &checkout))
            .then_some(checkout);
        let recipe = image::Recipe {
            base: origin.settings.image.clone(),
            workmux,
            devshell,
        };
        let tag = image::prepare(&recipe, &self.sandbox, context)?;
        image::build(&self.docker, context, &tag)?;
        Ok(tag)
    }

    /// Shallow-clone the project into `checkout` on the workstation and
    /// report whether it has a flake devshell to bake into the image.
    fn fetch_flake(&self, url: &str, checkout: &std::path::Path) -> bool {
        let cloned = Command::new("git")
            .args(["clone", "--quiet", "--depth", "1", url])
            .arg(checkout)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status();
        if !cloned.is_ok_and(|status| status.success()) {
            eprintln!(
                "warning: could not clone {url} on this machine to look for a flake devshell; the sandbox gets none"
            );
            return false;
        }
        let mode = crate::config::Config::load_with_location_from(checkout, None)
            .map(|(config, _)| config.sandbox.toolchain())
            .unwrap_or_default();
        crate::sandbox::toolchain::resolve_toolchain(&mode, checkout)
            == crate::sandbox::toolchain::DetectedToolchain::Flake
    }

    /// The arguments that create the sandbox from `image` with the policy
    /// file at `policy`. No bind mounts, ever: the repository is cloned
    /// inside the sandbox.
    fn create_args(&self, origin: &OpenshellRemote, image: &str, policy: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "sandbox", "create", "--name", &self.sandbox, "--from", image, "--policy", policy,
        ]
        .map(str::to_string)
        .to_vec();
        for provider in &origin.settings.providers {
            args.extend(["--provider".to_string(), provider.clone()]);
        }
        args.extend([
            "--annotation".to_string(),
            format!("workmux.project={}", origin.project),
            "--annotation".to_string(),
            format!("{IMAGE_ANNOTATION}={image}"),
        ]);
        args.extend(
            ["--no-tty", "--no-auto-providers", "--detach"].map(str::to_string),
        );
        args
    }

    /// Bring a running or stopped sandbox's policy in line with the
    /// project's settings. Returns what the user should be told. A
    /// narrowed filesystem grant cannot be applied in place.
    fn sync_policy(&self, running: bool) -> Result<Vec<String>> {
        let desired = self.desired_policy()?;
        let Some(current) = self.current_policy()? else {
            return Ok(Vec::new());
        };
        let desired_json = serde_json::to_value(&desired)?;
        if same_policy(&current, &desired_json) {
            return Ok(Vec::new());
        }
        let current_yaml: serde_yaml::Value = serde_json::from_value(current)?;
        let narrowed = policy::narrowed(&current_yaml, &desired);
        if !narrowed.is_empty() {
            return Ok(vec![format!(
                "the policy of sandbox {} no longer grants {}, which OpenShell cannot take away from a sandbox; run `workmux remote recreate {}` to apply it",
                self.sandbox,
                narrowed.join(", "),
                self.origin.as_ref().map_or(self.sandbox.as_str(), |o| o.project.as_str())
            )]);
        }
        let dir = tempfile::tempdir()?;
        let path = self.write_policy(dir.path())?;
        let path = path.to_string_lossy();
        let mut args = vec!["policy", "set", &self.sandbox, "--policy", &path];
        if running {
            args.push("--wait");
        }
        self.run(&args)?;
        let filesystem_changed = current_yaml.get("filesystem_policy") != desired.get("filesystem_policy");
        Ok(if running && filesystem_changed {
            vec![format!(
                "updated the policy of sandbox {}; its new filesystem grants apply after `workmux remote stop` and `workmux remote start`",
                self.sandbox
            )]
        } else {
            vec![format!("updated the policy of sandbox {}", self.sandbox)]
        })
    }
}

/// Whether two policies agree, treating an absent section as empty.
fn same_policy(current: &Value, desired: &Value) -> bool {
    let empty = |value: Option<&Value>| match value {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        _ => false,
    };
    let (Some(current), Some(desired)) = (current.as_object(), desired.as_object()) else {
        return current == desired;
    };
    current
        .keys()
        .chain(desired.keys())
        .all(|key| match (current.get(key), desired.get(key)) {
            (a, b) if empty(a) && empty(b) => true,
            (a, b) => a == b,
        })
}

/// A path in the sandbox: `~` is its `HOME`, and a relative path is taken
/// from there.
pub fn sandbox_path(path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => SANDBOX_HOME.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{SANDBOX_HOME}{rest}"),
        _ if path.starts_with('/') => path.to_string(),
        _ => format!("{SANDBOX_HOME}/{path}"),
    }
}

/// OpenShell's own words for a failed command: its stderr, else its exit
/// status.
fn failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("openshell exited with {}", output.status)
    } else {
        stderr
    }
}

impl Transport for OpenShell {
    fn exec_captured(&self, argv: &[String]) -> Result<Output> {
        self.exec(false, None, &[], argv)?
            .stdin(Stdio::null())
            .output()
            .context("failed to run openshell")
    }

    fn exec_stdio(&self, argv: &[String]) -> Result<StdioProcess> {
        StdioProcess::spawn(self.exec(false, None, &[], argv)?)
    }

    fn tty_command(&self, exec: &TtyExec) -> Result<Command> {
        self.exec(true, exec.workdir.as_deref(), &exec.env, &exec.argv)
    }

    fn provisioning(&self) -> Option<&dyn Provisioning> {
        Some(self)
    }
}

impl Provisioning for OpenShell {
    fn status(&self) -> Result<RemoteState> {
        Ok(match self.phase()? {
            Phase::Missing => RemoteState::Missing,
            Phase::Is(phase) if phase == "Ready" => RemoteState::Running,
            Phase::Is(_) => RemoteState::Stopped,
        })
    }

    fn create(&self) -> Result<()> {
        let origin = self.origin()?;
        let context = tempfile::tempdir()?;
        let image = self.build_image(origin, context.path())?;
        let policy = self.write_policy(context.path())?;
        let args = self.create_args(origin, &image, &policy.to_string_lossy());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args)?;
        self.wait_for(Some("Ready"))
    }

    fn start(&self) -> Result<()> {
        if let Phase::Is(phase) = self.phase()?
            && phase == "Stopped"
        {
            for notice in self.sync_policy(false)? {
                eprintln!("{notice}");
            }
            self.run(&["sandbox", "start", &self.sandbox])?;
        }
        self.wait_for(Some("Ready"))
    }

    fn stop(&self) -> Result<()> {
        self.run(&["sandbox", "stop", &self.sandbox])?;
        self.wait_for(Some("Stopped"))
    }

    fn delete(&self) -> Result<()> {
        self.run(&["sandbox", "delete", &self.sandbox])?;
        self.wait_for(None)
    }

    fn reconcile(&self) -> Result<Vec<String>> {
        if self.origin.is_none() {
            return Ok(Vec::new());
        }
        self.sync_policy(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpenshellProject;
    use std::os::unix::fs::PermissionsExt;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn origin() -> OpenshellRemote {
        OpenshellRemote {
            project: "api".to_string(),
            git_url: None,
            settings: OpenshellProject {
                providers: vec!["github".to_string(), "anthropic".to_string()],
                ..OpenshellProject::default()
            },
        }
    }

    /// A stand-in `openshell` that logs its arguments and answers `sandbox
    /// get` with the phase in the file `phase` (no file: not found).
    struct FakeOpenShell {
        dir: tempfile::TempDir,
    }

    impl FakeOpenShell {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = format!(
                "#!/bin/sh\n\
                 dir={dir}\n\
                 printf '%s\\n' \"$*\" >> \"$dir/log\"\n\
                 case \"$1 $2\" in\n\
                 'sandbox get')\n\
                   if [ -f \"$dir/phase\" ]; then printf '{{\"phase\":\"%s\",\"policy\":{{}}}}' \"$(cat \"$dir/phase\")\";\n\
                   else echo \"Error:   × code: 'Some requested entity was not found', message: \\\"sandbox not found\\\"\" >&2; exit 1; fi ;;\n\
                 'sandbox create'|'sandbox start') echo Ready > \"$dir/phase\" ;;\n\
                 'sandbox stop') echo Stopped > \"$dir/phase\" ;;\n\
                 'sandbox delete') rm -f \"$dir/phase\" ;;\n\
                 'sandbox exec') shift 2; while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\" ;;\n\
                 esac\n",
                dir = dir.path().display()
            );
            for (name, body) in [("openshell", script.as_str()), ("docker", "#!/bin/sh\nexit 0\n")] {
                let path = dir.path().join(name);
                std::fs::write(&path, body).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir }
        }

        fn transport(&self, origin: Option<OpenshellRemote>) -> OpenShell {
            OpenShell {
                openshell: self.dir.path().join("openshell"),
                docker: self.dir.path().join("docker"),
                phase_poll: Duration::from_millis(10),
                ..OpenShell::new("api", origin)
            }
        }

        fn log(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    #[test]
    fn exec_runs_without_a_login_shell_in_the_named_sandbox() {
        let transport = OpenShell::new("api", None);

        let command = transport
            .exec(false, None, &[], &argv(&["workmux", "remote", "serve"]))
            .unwrap();

        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), "openshell");
        assert_eq!(
            args,
            argv(&[
                "sandbox", "exec", "-n", "api", "--no-login-shell", "--no-tty", "--", "workmux",
                "remote", "serve"
            ])
        );
    }

    #[test]
    fn a_tty_command_carries_its_workdir_and_env() {
        let transport = OpenShell::new("api", None);

        let command = transport
            .tty_command(&TtyExec {
                argv: argv(&["workmux", "remote", "pane-attach", "api/feature/0"]),
                workdir: Some("~/api__worktrees/feature".to_string()),
                env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            })
            .unwrap();

        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            argv(&[
                "sandbox",
                "exec",
                "-n",
                "api",
                "--no-login-shell",
                "--tty",
                "--workdir",
                "/sandbox/api__worktrees/feature",
                "--env",
                "TERM=xterm-256color",
                "--",
                "workmux",
                "remote",
                "pane-attach",
                "api/feature/0"
            ])
        );
    }

    #[test]
    fn sandbox_paths_are_taken_from_its_home() {
        assert_eq!(sandbox_path("~"), "/sandbox");
        assert_eq!(sandbox_path("~/api"), "/sandbox/api");
        assert_eq!(sandbox_path("api"), "/sandbox/api");
        assert_eq!(sandbox_path("/work/api"), "/work/api");
    }

    #[test]
    fn the_sandbox_is_created_from_its_image_policy_and_providers_and_never_with_mounts() {
        let fake = FakeOpenShell::new();
        let transport = fake.transport(Some(origin()));
        assert_eq!(transport.status().unwrap(), RemoteState::Missing);

        transport.create().unwrap();

        assert_eq!(transport.status().unwrap(), RemoteState::Running);
        let create = fake
            .log()
            .into_iter()
            .find(|line| line.starts_with("sandbox create"))
            .expect("a create");
        let words: Vec<&str> = create.split(' ').collect();
        let policy = words[words.iter().position(|w| *w == "--policy").unwrap() + 1];
        let image = words[words.iter().position(|w| *w == "--from").unwrap() + 1];
        assert!(image.starts_with("workmux-openshell-api:"), "{create}");
        assert_eq!(
            create,
            format!(
                "sandbox create --name api --from {image} --policy {policy} \
                 --provider github --provider anthropic \
                 --annotation workmux.project=api --annotation workmux.image={image} \
                 --no-tty --no-auto-providers --detach"
            )
        );
        for flag in ["--upload", "--mount", "--driver-config-json", "-v"] {
            assert!(!words.contains(&flag), "{create}");
        }
    }

    #[test]
    fn a_registered_sandbox_without_a_project_is_never_created() {
        let fake = FakeOpenShell::new();

        let error = fake.transport(None).create().unwrap_err();

        assert!(
            error.to_string().contains("workmux creates sandboxes only for projects"),
            "{error}"
        );
        assert!(fake.log().iter().all(|line| !line.starts_with("sandbox create")));
    }

    #[test]
    fn stop_start_and_delete_follow_the_sandboxs_phase() {
        let fake = FakeOpenShell::new();
        let transport = fake.transport(Some(origin()));
        transport.create().unwrap();

        transport.stop().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Stopped);
        transport.start().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Running);
        transport.delete().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Missing);
    }

    #[test]
    fn a_policy_that_only_widens_is_applied_in_place() {
        let current = serde_json::to_value(policy::effective(None).unwrap()).unwrap();
        let widened = serde_json::to_value(
            policy::effective(Some("network_policies:\n  github: {endpoints: []}\n")).unwrap(),
        )
        .unwrap();

        assert!(same_policy(&current, &current));
        assert!(!same_policy(&current, &widened));
        let mut with_empty = current.clone();
        with_empty["network_policies"] = serde_json::json!({});
        assert!(same_policy(&current, &with_empty));
    }
}

/// Contract tests against a real OpenShell gateway, env-gated like ticket
/// 01's learning tests: `WORKMUX_TEST_OPENSHELL=1`. Each test owns one
/// `wm-c20-…` sandbox and deletes it.
#[cfg(test)]
mod contract {
    use super::*;
    use crate::remote::channel::Handler;
    use crate::remote::connection::Connection;
    use crate::ssh_learning::harness::{Pty, Reaped};
    use std::io::{BufRead, BufReader, Write};

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn enabled() -> bool {
        std::env::var("WORKMUX_TEST_OPENSHELL").as_deref() == Ok("1")
    }

    /// The image the contract tests run: the learning tests' fixture image
    /// (git, dtach, python3), plus the Linux workmux named by
    /// `WORKMUX_OPENSHELL_WORKMUX` when it is set.
    fn fixture_image() -> String {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/openshell");
        let status = Command::new("docker")
            .args(["build", "-q", "-t", "wm-learn-openshell:latest", fixture])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        "wm-learn-openshell:latest".to_string()
    }

    struct Owned(OpenShell);

    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.run(&["sandbox", "delete", &self.0.sandbox]);
            let _ = self.0.wait_for(None);
            // The images workmux built for the sandbox.
            let images = Command::new("docker")
                .args(["image", "ls", "-q", &format!("workmux-openshell-{}", self.0.sandbox)])
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            // The container may still be going away after the delete.
            for image in images.split_whitespace() {
                for _ in 0..30 {
                    let removed = Command::new("docker")
                        .args(["rmi", "-f", image])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    if removed.is_ok_and(|status| status.success()) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    fn sandbox(label: &str) -> Owned {
        let name = format!("wm-c20-{label}-{:04x}", std::process::id() % 0x10000);
        assert!(name.len() <= 19, "{name}");
        let origin = OpenshellRemote {
            project: name.clone(),
            git_url: None,
            settings: crate::config::OpenshellProject {
                image: Some(fixture_image()),
                ..Default::default()
            },
        };
        Owned(OpenShell::new(name, Some(origin)))
    }

    struct Quiet;
    impl Handler for Quiet {
        fn request(&self, method: &str, _: Value) -> Result<Value, String> {
            Err(format!("unknown method: {method}"))
        }
        fn notification(&self, _: &str, _: Value) {}
    }

    #[test]
    fn lifecycle_exec_and_stdio_against_a_real_gateway() {
        if !enabled() {
            return;
        }
        let owned = sandbox("life");
        let transport = &owned.0;
        assert_eq!(transport.status().unwrap(), RemoteState::Missing);
        let error = transport
            .exec_captured(&argv(&["true"]))
            .map(|output| failure(&output))
            .unwrap();
        assert!(error.contains("not found"), "{error}");

        transport.create().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Running);
        let policy = transport.current_policy().unwrap().unwrap();
        assert!(same_policy(
            &policy,
            &serde_json::to_value(transport.desired_policy().unwrap()).unwrap()
        ));

        // Captured exec: exit codes, stdout and stderr in the transport's words.
        let output = transport
            .exec_captured(&argv(&["sh", "-c", "echo out; echo err >&2; pwd; exit 3"]))
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n/sandbox\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");

        // Long-lived stdio: the first line arrives, and lines go both ways.
        let mut process = transport
            .exec_stdio(&argv(&[
                "sh",
                "-c",
                "echo WORKMUX-REMOTE-HELLO; while IFS= read -r line; do echo \"echo:$line\"; done",
            ]))
            .unwrap();
        let (mut stdin, stdout) = process.take_stdio();
        let mut lines = BufReader::new(stdout).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "WORKMUX-REMOTE-HELLO");
        writeln!(stdin, "{{\"id\":1}}").unwrap();
        assert_eq!(lines.next().unwrap().unwrap(), "echo:{\"id\":1}");
        drop(stdin);
        drop(process);

        // The workspace and a PTY session holder survive stop and start.
        transport
            .exec_captured(&argv(&["sh", "-c", "echo kept > /sandbox/marker"]))
            .unwrap();
        transport.stop().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Stopped);
        let refused = transport.exec_captured(&argv(&["true"])).unwrap();
        assert!(failure(&refused).contains("is not ready (phase: Stopped)"));
        transport.start().unwrap();
        let output = transport
            .exec_captured(&argv(&["cat", "/sandbox/marker"]))
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "kept\n");
        let output = transport
            .exec_captured(&argv(&[
                "sh",
                "-c",
                "dtach -n /sandbox/s.sock -E sh -c 'sleep 30' && sleep 0.5 && test -S /sandbox/s.sock && echo held",
            ]))
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "held\n", "{output:?}");

        transport.delete().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Missing);
    }

    #[test]
    fn a_tty_exec_has_its_workdir_and_env() {
        if !enabled() {
            return;
        }
        let owned = sandbox("tty");
        let transport = &owned.0;
        transport.create().unwrap();
        transport
            .exec_captured(&argv(&["mkdir", "-p", "/sandbox/api"]))
            .unwrap();

        let mut command = transport
            .tty_command(&TtyExec {
                argv: argv(&["sh", "-c", "pwd; echo \"env=$WM_TTY\"; tty"]),
                workdir: Some("~/api".to_string()),
                env: vec![("WM_TTY".to_string(), "from-workstation".to_string())],
            })
            .unwrap();
        let pty = Pty::open(33, 111);
        let reader = pty.reader();
        let _child = Reaped(pty.spawn(&mut command));
        assert!(
            reader.wait_until(Duration::from_secs(60), |text| text.contains("/dev/pts/")),
            "{}",
            reader.text()
        );

        let output = reader.text();
        assert!(output.contains("/sandbox/api"), "{output}");
        assert!(output.contains("env=from-workstation"), "{output}");
        assert!(output.contains("/dev/pts/"), "{output}");
    }

    #[test]
    fn remote_workmux_serves_a_control_channel_through_exec() {
        if !enabled() || std::env::var_os(image::BINARY_ENV).is_none() {
            return;
        }
        let owned = sandbox("serve");
        let transport = &owned.0;
        transport.create().unwrap();

        let connection = Connection::open(
            &transport.sandbox,
            transport,
            Quiet,
            Duration::from_secs(60),
        )
        .unwrap();

        let ping = connection.ping().unwrap();
        assert_eq!(ping.workmux, env!("CARGO_PKG_VERSION"));
        assert!(
            !ping.warnings.iter().any(|w| w.contains("dtach")),
            "{:?}",
            ping.warnings
        );
    }
}
