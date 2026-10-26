//! The OpenShell transport: one sandbox per project, reached through the
//! `openshell` CLI's exec. workmux creates the sandbox on first use (its
//! image, policy and providers come from the project's settings), starts it
//! when a command needs it, and stops or deletes it only when asked.
//!
//! Observed behaviour this relies on is pinned by the OpenShell learning
//! tests (`tests/test_openshell_learning.py`): names of at most 19 characters, `--detach` at
//! create, exec with `--no-login-shell`, the workload running as uid 1000
//! with `HOME` and workdir `/sandbox`, and policy changes that apply in
//! place only when they widen a grant.

pub mod agents;
pub mod github;
pub mod image;
pub mod policy;
pub mod repo;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::transport::{Provisioning, RemoteState, StdioProcess, Transport, TtyExec};
use crate::config::OpenshellRemote;

/// Where OpenShell runs every sandbox's workload, and its `HOME`.
pub const SANDBOX_HOME: &str = "/sandbox";

/// Set on every command OpenShell runs in a sandbox, to the sandbox's name:
/// remote workmux there never starts workmux's own sandbox.
pub const SANDBOX_ENV: &str = "WORKMUX_OPENSHELL_SANDBOX";

/// How long a sandbox may take to reach a phase.
const PHASE_TIMEOUT: Duration = Duration::from_secs(300);

/// The annotation recording the image a sandbox was created from.
pub const IMAGE_ANNOTATION: &str = "workmux.image";

/// The annotation recording the project a sandbox was created for: the
/// mark of a sandbox workmux made.
pub const PROJECT_ANNOTATION: &str = "workmux.project";

/// The annotation recording where the project's clone is in the sandbox.
pub const PATH_ANNOTATION: &str = "workmux.path";

/// A sandbox workmux made, as the gateway lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub project: String,
    /// The project's clone in the sandbox.
    pub path: String,
    pub phase: String,
}

impl Listed {
    pub fn state(&self) -> RemoteState {
        state_in(&self.phase)
    }
}

/// The state of an existing sandbox in `phase`: running only once ready.
fn state_in(phase: &str) -> RemoteState {
    if phase == "Ready" {
        RemoteState::Running
    } else {
        RemoteState::Stopped
    }
}

/// The sandboxes on the gateway that workmux made, by its annotation.
/// None when OpenShell is not installed.
pub fn workmux_sandboxes() -> Result<Vec<Listed>> {
    let mut listed = Vec::new();
    let mut token = String::new();
    loop {
        let output = match Command::new("openshell")
            .env("NO_COLOR", "1")
            .args(["sandbox", "list", "-o", "json", "--page-token", &token])
            .stdin(Stdio::null())
            .output()
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(listed),
            output => output.context("Failed to run openshell")?,
        };
        if !output.status.success() {
            bail!("{}", failure(&output));
        }
        let page: Value = serde_json::from_slice(&output.stdout)
            .context("openshell sandbox list printed unreadable JSON")?;
        listed.extend(workmux_sandboxes_in(&page));
        token = page["next_page_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            return Ok(listed);
        }
    }
}

/// The sandboxes workmux made in one page of `sandbox list -o json`.
fn workmux_sandboxes_in(page: &Value) -> Vec<Listed> {
    page["sandboxes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sandbox| {
            let annotations = &sandbox["annotations"];
            let project = annotations[PROJECT_ANNOTATION].as_str()?.to_string();
            Some(Listed {
                name: sandbox["name"].as_str()?.to_string(),
                // A sandbox made before workmux recorded the path has its
                // clone at the default path.
                path: annotations[PATH_ANNOTATION]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("~/{project}")),
                project,
                phase: sandbox["phase"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

pub struct OpenShell {
    sandbox: String,
    /// What the sandbox is created from, for a project's implicit remote.
    origin: Option<OpenshellRemote>,
    openshell: PathBuf,
    docker: PathBuf,
    gh: PathBuf,
    /// Where `workmux sandbox install-dev` put the Linux workmux the
    /// sandbox's image gets.
    linux_workmux: Option<PathBuf>,
    phase_poll: Duration,
    /// The sandbox as the last state check described it, kept for the one
    /// policy read that follows, so a command asks OpenShell about the
    /// sandbox once.
    last_get: Mutex<Option<Value>>,
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
            gh: PathBuf::from("gh"),
            linux_workmux: image::installed_workmux(),
            phase_poll: Duration::from_secs(1),
            last_get: Mutex::new(None),
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
    fn exec(
        &self,
        tty: bool,
        workdir: Option<&str>,
        env: &[(String, String)],
        argv: &[String],
    ) -> Result<Command> {
        if argv.is_empty() {
            return Err(anyhow!("No command to run on the remote"));
        }
        let mut command = self.openshell();
        command.args(["sandbox", "exec", "-n", &self.sandbox, "--no-login-shell"]);
        command.arg(if tty { "--tty" } else { "--no-tty" });
        if let Some(workdir) = workdir {
            command.arg("--workdir").arg(sandbox_path(workdir));
        }
        command
            .arg("--env")
            .arg(format!("{SANDBOX_ENV}={}", self.sandbox));
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
            .context("Failed to run openshell")?;
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
            .context("Failed to run openshell")?;
        if !output.status.success() {
            let error = failure(&output);
            if error.contains("not found") {
                return Ok(None);
            }
            bail!("{error}");
        }
        Ok(Some(serde_json::from_slice(&output.stdout).with_context(
            || {
                format!(
                    "openshell sent an unreadable description of {}",
                    self.sandbox
                )
            },
        )?))
    }

    /// The sandbox's phase, keeping its description for the policy read
    /// that follows.
    fn phase(&self) -> Result<Phase> {
        let sandbox = self.get()?;
        let phase = match &sandbox {
            None => Phase::Missing,
            Some(sandbox) => Phase::Is(sandbox["phase"].as_str().unwrap_or_default().to_string()),
        };
        *self.last_get.lock().unwrap() = sandbox;
        Ok(phase)
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
                    bail!("Sandbox {} is in phase Error", self.sandbox)
                }
                (Phase::Missing, Some(wanted)) => {
                    bail!(
                        "Sandbox {} disappeared while waiting for {wanted}",
                        self.sandbox
                    )
                }
                _ => {}
            }
            if Instant::now() > deadline {
                bail!(
                    "Sandbox {} did not reach {} within {}s; it is {phase:?}",
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
                "Sandbox {} does not exist; workmux creates a sandbox when a command runs in a repository whose config sets sandbox.backend: openshell",
                self.sandbox
            )
        })
    }

    /// The policy the sandbox should have: the base policy, with nix's
    /// grants when the sandbox has nix, and the project's policy file merged
    /// on top.
    pub fn desired_policy(&self) -> Result<serde_yaml::Value> {
        let user =
            match self
                .origin
                .as_ref()
                .and_then(|o| o.settings.policy.as_deref())
            {
                Some(path) => {
                    let path = crate::util::expand_tilde(path);
                    Some(std::fs::read_to_string(&path).with_context(|| {
                        format!("failed to read policy file {}", path.display())
                    })?)
                }
                None => None,
            };
        policy::effective(
            user.as_deref(),
            self.origin.as_ref().is_some_and(|origin| origin.nix),
        )
    }

    /// The sandbox's effective policy, as OpenShell reports it: from the
    /// state check just made, else from a new `sandbox get`.
    pub fn current_policy(&self) -> Result<Option<Value>> {
        let sandbox = match self.last_get.lock().unwrap().take() {
            Some(sandbox) => Some(sandbox),
            None => self.get()?,
        };
        Ok(sandbox.map(|sandbox| sandbox["policy"].clone()))
    }

    fn write_policy(&self, dir: &std::path::Path) -> Result<PathBuf> {
        let path = dir.join("policy.yaml");
        std::fs::write(&path, serde_yaml::to_string(&self.desired_policy()?)?)?;
        Ok(path)
    }

    /// What the user is told about the project's agents: where each is
    /// installed and which attached provider gives it credentials. `None`
    /// for a sandbox that belongs to no project.
    pub fn agent_report(&self) -> Option<agents::Report> {
        let origin = self.origin.as_ref()?;
        let providers = &self.attached_providers();
        let found = if providers.is_empty() {
            Ok(Default::default())
        } else {
            self.providers().map_err(|error| format!("{error:#}"))
        };
        Some(agents::report(
            &origin.agents,
            providers,
            found.as_ref().map_err(String::as_str),
        ))
    }

    /// The providers attached to the sandbox.
    fn attached_providers(&self) -> Vec<String> {
        self.origin
            .as_ref()
            .map(|origin| origin.settings.providers.clone())
            .unwrap_or_default()
    }

    /// Every provider profile on the gateway.
    fn profiles(&self) -> Result<Value> {
        let output = self.run(&["provider", "profile", "list", "-o", "json"])?;
        serde_json::from_slice(&output.stdout).context("openshell sent an unreadable profile list")
    }

    /// Every provider on the gateway, as `provider list` describes it,
    /// read page by page.
    fn provider_list(&self) -> Result<Vec<Value>> {
        let mut providers = Vec::new();
        let mut token = String::new();
        loop {
            let output = self.run(&["provider", "list", "-o", "json", "--page-token", &token])?;
            let mut page: Value = serde_json::from_slice(&output.stdout)
                .context("openshell sent an unreadable provider list")?;
            if let Some(Value::Array(found)) = page.get_mut("providers").map(Value::take) {
                providers.extend(found);
            }
            match page["next_page_token"].as_str() {
                Some(next) if !next.is_empty() && next != token => token = next.to_string(),
                _ => return Ok(providers),
            }
        }
    }

    /// Every provider on the gateway by name, with what its profile
    /// allows.
    fn providers(&self) -> Result<std::collections::BTreeMap<String, agents::Provider>> {
        let profiles = self.profiles()?;
        let profile = |provider_type: &str| {
            profiles
                .as_array()?
                .iter()
                .find(|profile| profile["id"] == provider_type)
        };
        let reach = |provider_type: &str| {
            let profile = profile(provider_type)?;
            let strings = |list: &Value, field: Option<&str>| -> Vec<String> {
                list.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| {
                        Some(
                            field
                                .map_or(item, |field| &item[field])
                                .as_str()?
                                .to_string(),
                        )
                    })
                    .collect()
            };
            Some(agents::ProfileReach {
                binaries: strings(&profile["binaries"], None),
                hosts: strings(&profile["endpoints"], Some("host")),
            })
        };
        let mut providers = std::collections::BTreeMap::new();
        for provider in self.provider_list()? {
            let (Some(name), Some(provider_type)) =
                (provider["name"].as_str(), provider["type"].as_str())
            else {
                continue;
            };
            providers.insert(
                name.to_string(),
                agents::Provider {
                    provider_type: provider_type.to_string(),
                    profile: reach(provider_type),
                },
            );
        }
        Ok(providers)
    }

    /// Whether the image's git gets a credential helper for github.com.
    /// When the providers can't be looked up it does: the helper hands git
    /// only the placeholder already in the sandbox's environment.
    fn needs_git_credential_helper(&self) -> bool {
        self.github_providers().map_or_else(
            |error| {
                eprintln!(
                    "warning: could not check which providers give GitHub credentials ({error:#}); \
                     the sandbox image gets git's credential helper for github.com anyway"
                );
                true
            },
            |providers| !providers.is_empty(),
        )
    }

    /// Build the sandbox's image, returning its tag.
    fn build_image(
        &self,
        origin: &OpenshellRemote,
        workmux: &std::path::Path,
        context: &std::path::Path,
    ) -> Result<String> {
        let checkout = context.join("project");
        // A devshell is baked when the toolchain is a flake, as nix is, and
        // the clone has one. The workstation has SSH: clone the way the
        // user gave the URL.
        let devshell = (origin.nix
            && origin
                .git_url
                .as_ref()
                .is_some_and(|url| self.fetch_flake(url.original(), &checkout)))
        .then_some(checkout);
        let recipe = image::Recipe {
            base: origin.settings.image.clone(),
            workmux: workmux.to_path_buf(),
            agents: origin.agents.clone(),
            devshell,
            git_credential_helper: self.needs_git_credential_helper(),
            nix: origin.nix,
        };
        let tag = image::prepare(&recipe, &self.sandbox, &sandbox_path(&origin.path), context)?;
        // The bake fetches the flake's inputs, private GitHub ones with the
        // workstation's gh login, but only for a project that already gives
        // its sandbox that login: the bake runs the project's own flake.
        let gh_login = recipe.devshell.is_some() && self.attaches_gh_login();
        let github_token = gh_login
            .then(|| github::login_token(&self.gh).ok())
            .flatten();
        let built = image::build(&self.docker, context, &tag, github_token.as_deref());
        if recipe.devshell.is_some() && !gh_login {
            built.map_err(|error| {
                anyhow!(
                    "{error:#}; the devshell bake had no GitHub credentials, and private GitHub \
                     flake inputs can only be fetched with `{}` in sandbox.openshell.providers",
                    github::GH_LOGIN
                )
            })?;
        } else {
            built?;
        }
        Ok(tag)
    }

    /// Shallow-clone the project into `checkout` on the workstation and
    /// report whether it has a flake devshell to bake into the image.
    fn fetch_flake(&self, url: &str, checkout: &std::path::Path) -> bool {
        let cloned = crate::git::unattended_git(None).and_then(|mut git| {
            git.args(["clone", "--quiet", "--depth", "1", url])
                .arg(checkout)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .context("Failed to run git")
        });
        if !cloned.is_ok_and(|status| status.success()) {
            eprintln!(
                "warning: could not clone {url} on the workstation to look for a flake devshell; the sandbox gets none"
            );
            return false;
        }
        checkout.join("flake.nix").is_file()
    }

    /// The arguments that create the sandbox from `image` with the policy
    /// file at `policy`. No bind mounts, ever: the repository is cloned
    /// inside the sandbox.
    fn create_args(&self, origin: &OpenshellRemote, image: &str, policy: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "sandbox",
            "create",
            "--name",
            &self.sandbox,
            "--from",
            image,
            "--policy",
            policy,
        ]
        .map(str::to_string)
        .to_vec();
        for provider in self.attached_providers() {
            args.extend(["--provider".to_string(), provider]);
        }
        args.extend([
            "--annotation".to_string(),
            format!("{PROJECT_ANNOTATION}={}", origin.project),
            "--annotation".to_string(),
            format!("{PATH_ANNOTATION}={}", origin.path),
            "--annotation".to_string(),
            format!("{IMAGE_ANNOTATION}={image}"),
        ]);
        args.extend(["--no-tty", "--no-auto-providers", "--detach"].map(str::to_string));
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
                "the policy of sandbox {} no longer grants {}, which OpenShell cannot take away from a sandbox; run `workmux sandbox rebuild` in the repository to apply it",
                self.sandbox,
                narrowed.join(", "),
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
        let filesystem_changed =
            current_yaml.get("filesystem_policy") != desired.get("filesystem_policy");
        Ok(if running && filesystem_changed {
            vec![format!(
                "updated the policy of sandbox {}; its new filesystem grants apply once `workmux sandbox stop` has stopped it and the next command that needs it has started it",
                self.sandbox
            )]
        } else {
            vec![format!("updated the policy of sandbox {}", self.sandbox)]
        })
    }
}

/// The lines `workmux sandbox status` shows for a sandbox's effective
/// policy.
pub fn describe_policy(policy: &Value) -> Vec<String> {
    let paths = |list: &str| -> String {
        policy["filesystem_policy"][list]
            .as_array()
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|paths| !paths.is_empty())
            .unwrap_or_else(|| "none".to_string())
    };
    let rules = policy["network_policies"]
        .as_object()
        .map(|rules| rules.keys().cloned().collect::<Vec<_>>().join(", "))
        .filter(|rules| !rules.is_empty())
        .unwrap_or_else(|| "none".to_string());
    vec![
        format!("read-only:  {}", paths("read_only")),
        format!("read-write: {}", paths("read_write")),
        format!(
            "workdir:    {}",
            if policy["filesystem_policy"]["include_workdir"] == Value::Bool(false) {
                "not included"
            } else {
                "read-write"
            }
        ),
        format!("network:    {rules}"),
    ]
}

impl OpenShell {
    /// The sandbox's effective policy for `workmux sandbox status`, and
    /// what differs from the project's settings.
    pub fn policy_report(&self) -> Result<Option<(Vec<String>, Vec<String>)>> {
        let Some(current) = self.current_policy()? else {
            return Ok(None);
        };
        let mut warnings = Vec::new();
        if self.origin.is_some() {
            let desired = self.desired_policy()?;
            if !same_policy(&current, &serde_json::to_value(&desired)?) {
                let current_yaml: serde_yaml::Value = serde_json::from_value(current.clone())?;
                let narrowed = policy::narrowed(&current_yaml, &desired);
                warnings.push(if narrowed.is_empty() {
                    "the project's policy has changed; the next command that needs the sandbox applies it".to_string()
                } else {
                    format!(
                        "the project's policy no longer grants {}; run `workmux sandbox rebuild` to apply it",
                        narrowed.join(", ")
                    )
                });
            }
        }
        Ok(Some((describe_policy(&current), warnings)))
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

/// A sandbox create or start error, with a hint when it says the gateway's
/// supervisor image is gone from Docker: the gateway resolves that image once
/// at start, so every create fails until the gateway restarts and pulls it.
fn with_supervisor_image_hint(error: anyhow::Error) -> anyhow::Error {
    let message = error.to_string();
    if !(message.contains("ControlSupervisorStartFailed") && message.contains("No such image")) {
        return error;
    }
    anyhow!(
        "{message}\n\nThe OpenShell gateway's supervisor image is no longer in Docker (removed by \
         an image prune or by Kubernetes image garbage collection on a shared Docker engine); \
         restarting the gateway pulls it again, e.g. `launchctl kickstart -k \
         gui/$(id -u)/<gateway label>` for a launchd-managed gateway."
    )
}

impl Transport for OpenShell {
    fn exec_captured(&self, argv: &[String]) -> Result<Output> {
        self.exec(false, None, &[], argv)?
            .stdin(Stdio::null())
            .output()
            .context("Failed to run openshell")
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
            Phase::Is(phase) => state_in(&phase),
        })
    }

    fn create(&self) -> Result<()> {
        let origin = self.origin()?;
        let workmux = image::linux_workmux(self.linux_workmux.as_deref())?;
        self.check_github_credentials()?;
        if self.attaches_gh_login() {
            eprintln!(
                "Note: provider {} gives the sandbox your gh login's GitHub access, which usually \
                 reaches every repo you can (push and pull requests included); workmux keeps its token in \
                 that provider on the gateway and updates it whenever it creates or attaches to the sandbox",
                github::GH_LOGIN
            );
        }
        if origin.git_url.as_ref().is_some_and(|url| url.ssh.is_some()) {
            let github = self
                .github_providers()
                .map_err(|error| format!("{error:#}"));
            eprintln!(
                "Note: {}",
                repo::ssh_notice(github.as_deref().map_err(String::as_str))
            );
        }
        self.refresh_gh_login(true)?;
        let context = tempfile::tempdir()?;
        let image = self.build_image(origin, workmux, context.path())?;
        for warning in self.agent_report().map(|r| r.warnings).unwrap_or_default() {
            eprintln!("warning: {warning}");
        }
        let policy = self.write_policy(context.path())?;
        let args = self.create_args(origin, &image, &policy.to_string_lossy());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args).map_err(with_supervisor_image_hint)?;
        self.wait_for(Some("Ready"))
    }

    fn start(&self) -> Result<()> {
        if let Phase::Is(phase) = self.phase()?
            && phase == "Stopped"
        {
            // Without its project's settings the policy it should have is
            // unknown: started as it is.
            if self.origin.is_some() {
                for notice in self.sync_policy(false)? {
                    eprintln!("{notice}");
                }
            }
            self.run(&["sandbox", "start", &self.sandbox])
                .map_err(with_supervisor_image_hint)?;
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
        self.refresh_gh_login(false)?;
        self.sync_policy(true)
    }

    fn refresh_credentials(&self) -> Result<()> {
        self.refresh_gh_login(false)
    }

    fn notes(&self) -> Vec<String> {
        self.origin
            .as_ref()
            .filter(|origin| origin.extra_mounts)
            .map(|_| {
                format!(
                    "Note: extra_mounts is ignored for OpenShell sandbox {}: OpenShell does not mount workstation directories",
                    self.sandbox
                )
            })
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::config::OpenshellProject;

    #[test]
    fn the_gateway_lists_the_sandboxes_workmux_made_by_their_annotation() {
        let page = serde_json::json!({
            "next_page_token": "",
            "sandboxes": [
                {"name": "api-1a2b3c4d", "phase": "Ready",
                 "annotations": {"workmux.project": "api", "workmux.path": "src/api",
                                 "workmux.image": "i:1"}},
                {"name": "theirs", "phase": "Ready", "annotations": {}},
                {"name": "web-5e6f7a8b", "phase": "Stopped",
                 "annotations": {"workmux.project": "web"}},
            ],
        });

        assert_eq!(
            workmux_sandboxes_in(&page),
            [
                Listed {
                    name: "api-1a2b3c4d".to_string(),
                    project: "api".to_string(),
                    path: "src/api".to_string(),
                    phase: "Ready".to_string(),
                },
                Listed {
                    name: "web-5e6f7a8b".to_string(),
                    project: "web".to_string(),
                    // Made before workmux recorded the path: the default.
                    path: "~/web".to_string(),
                    phase: "Stopped".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_listed_sandbox_runs_only_in_phase_ready() {
        let listed = |phase: &str| Listed {
            name: "wm-api".to_string(),
            project: "api".to_string(),
            path: "~/api".to_string(),
            phase: phase.to_string(),
        };

        assert_eq!(listed("Ready").state(), RemoteState::Running);
        assert_eq!(listed("Stopped").state(), RemoteState::Stopped);
        assert_eq!(listed("Provisioning").state(), RemoteState::Stopped);
    }
    use std::os::unix::fs::PermissionsExt;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    pub(in crate::remote::openshell) fn origin() -> OpenshellRemote {
        OpenshellRemote {
            project: "api".to_string(),
            git_url: None,
            path: "~/api".to_string(),
            settings: OpenshellProject {
                providers: vec!["github".to_string(), "anthropic".to_string()],
                ..OpenshellProject::default()
            },
            agents: Vec::new(),
            nix: false,
            extra_mounts: false,
        }
    }

    /// A stand-in `openshell` that logs its arguments and answers `sandbox
    /// get` with the phase in the file `phase` (no file: not found), and the
    /// provider commands from an empty gateway.
    pub(in crate::remote::openshell) struct FakeOpenShell {
        dir: tempfile::TempDir,
    }

    impl FakeOpenShell {
        pub(in crate::remote::openshell) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = format!(
                "#!/bin/sh\n\
                 dir={dir}\n\
                 printf '%s\\n' \"$*\" >> \"$dir/log\"\n\
                 case \"$1 $2\" in\n\
                 'sandbox get')\n\
                   if [ -f \"$dir/phase\" ]; then printf '{{\"phase\":\"%s\",\"policy\":{{}}}}' \"$(cat \"$dir/phase\")\";\n\
                   else echo \"Error:   × code: 'Some requested entity was not found', message: \\\"sandbox not found\\\"\" >&2; exit 1; fi ;;\n\
                 'sandbox create'|'sandbox start') prev=; for arg; do [ \"$prev\" = --policy ] && cp \"$arg\" \"$dir/created-policy\"; prev=$arg; done;\n\
                   if [ -f \"$dir/fail\" ]; then cat \"$dir/fail\" >&2; exit 1; fi;\n\
                   echo Ready > \"$dir/phase\" ;;\n\
                 'sandbox stop') echo Stopped > \"$dir/phase\" ;;\n\
                 'sandbox delete') rm -f \"$dir/phase\" ;;\n\
                 'provider list') token=; while [ $# -gt 0 ]; do [ \"$1\" = --page-token ] && token=$2; shift; done;\n\
                   cat \"$dir/providers$token\" ;;\n\
                 'provider profile') case $3 in\n\
                   list) cat \"$dir/profiles\" ;;\n\
                   *) while [ \"$1\" != -f ]; do shift; done; cp \"$2\" \"$dir/imported\" ;;\n\
                   esac ;;\n\
                 'provider create') echo \"create $GH_TOKEN\" >> \"$dir/sent\"; labels=;\n\
                   case \"$*\" in *'--label workmux.managed=gh-login'*) labels=',\"labels\":{{\"workmux.managed\":\"gh-login\"}}' ;; esac;\n\
                   echo \"{{\\\"next_page_token\\\":\\\"\\\",\\\"providers\\\":[{{\\\"name\\\":\\\"gh-login\\\",\\\"type\\\":\\\"workmux-github\\\"$labels}}]}}\" > \"$dir/providers\" ;;\n\
                 'provider update') echo \"update $GH_TOKEN\" >> \"$dir/sent\" ;;\n\
                 'sandbox exec') shift 2; while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\" ;;\n\
                 esac\n",
                dir = dir.path().display()
            );
            // A stand-in `gh`, logged in with the token in `gh-token`.
            let gh = format!(
                "#!/bin/sh\n\
                 [ -f {dir}/gh-token ] || {{ echo 'no oauth token found for github.com' >&2; exit 1; }}\n\
                 cat {dir}/gh-token\n",
                dir = dir.path().display()
            );
            // A stand-in `docker` that logs its arguments and the build
            // secret it was handed, and keeps the Dockerfile.
            let docker = format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> {dir}/docker-log\n\
                 [ ! -f Dockerfile ] || cp Dockerfile {dir}/Dockerfile\n\
                 printf 'secret %s\\n' \"$WORKMUX_GITHUB_TOKEN\" >> {dir}/docker-log\n\
                 [ ! -f {dir}/docker-fail ]\n",
                dir = dir.path().display()
            );
            for (name, body) in [
                ("openshell", script.as_str()),
                ("docker", docker.as_str()),
                ("gh", gh.as_str()),
            ] {
                let path = dir.path().join(name);
                std::fs::write(&path, body).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            // The Linux workmux `sandbox install-dev` would have installed.
            std::fs::write(dir.path().join("linux-workmux"), "linux workmux").unwrap();
            let fake = Self { dir };
            fake.providers("[]");
            fake.profiles("[]");
            fake
        }

        pub(in crate::remote::openshell) fn transport(
            &self,
            origin: Option<OpenshellRemote>,
        ) -> OpenShell {
            OpenShell {
                openshell: self.dir.path().join("openshell"),
                docker: self.dir.path().join("docker"),
                gh: self.dir.path().join("gh"),
                linux_workmux: Some(self.dir.path().join("linux-workmux")),
                phase_poll: Duration::from_millis(10),
                ..OpenShell::new("api", origin)
            }
        }

        pub(in crate::remote::openshell) fn log(&self) -> Vec<String> {
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
                "sandbox",
                "exec",
                "-n",
                "api",
                "--no-login-shell",
                "--no-tty",
                "--env",
                "WORKMUX_OPENSHELL_SANDBOX=api",
                "--",
                "workmux",
                "remote",
                "serve"
            ])
        );
    }

    #[test]
    fn a_sandbox_notes_on_create_or_start_that_it_ignores_extra_mounts() {
        let openshell = |extra_mounts| {
            OpenShell::new(
                "api-1a2b3c4d",
                Some(OpenshellRemote {
                    extra_mounts,
                    ..origin()
                }),
            )
        };

        assert_eq!(
            openshell(true).notes(),
            [
                "Note: extra_mounts is ignored for OpenShell sandbox api-1a2b3c4d: OpenShell does not mount workstation directories"
            ]
        );
        assert!(openshell(false).notes().is_empty());
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
                "WORKMUX_OPENSHELL_SANDBOX=api",
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
                 --annotation workmux.project=api --annotation workmux.path=~/api \
                 --annotation workmux.image={image} --no-tty --no-auto-providers --detach"
            )
        );
        for flag in ["--upload", "--mount", "--driver-config-json", "-v"] {
            assert!(!words.contains(&flag), "{create}");
        }
    }

    /// `origin()` with a git URL to a local repository holding a flake, its
    /// toolchain.
    fn flake_origin(repo: &std::path::Path) -> OpenshellRemote {
        std::fs::write(repo.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
        crate::test_support::init_repo(repo);
        crate::test_support::run_git(repo, &["add", "flake.nix"]);
        crate::test_support::run_git(repo, &["commit", "-q", "-m", "flake"]);
        OpenshellRemote {
            git_url: Some(crate::remote::git_url::GitUrl::new(
                repo.display().to_string(),
            )),
            nix: true,
            ..origin()
        }
    }

    /// `flake_origin` attaching `providers`.
    fn flake_origin_with(repo: &std::path::Path, providers: &[&str]) -> OpenshellRemote {
        let mut origin = flake_origin(repo);
        origin.settings.providers = providers.iter().map(|p| p.to_string()).collect();
        origin
    }

    /// The Dockerfile and the policy the fake built and created the
    /// sandbox with.
    fn built_and_created(fake: &FakeOpenShell) -> (String, serde_yaml::Value) {
        let dockerfile = std::fs::read_to_string(fake.dir.path().join("Dockerfile")).unwrap();
        let policy = std::fs::read_to_string(fake.dir.path().join("created-policy")).unwrap();
        (dockerfile, serde_yaml::from_str(&policy).unwrap())
    }

    fn paths(policy: &serde_yaml::Value, list: &str) -> Vec<String> {
        serde_yaml::from_value(policy["filesystem_policy"][list].clone()).unwrap()
    }

    #[test]
    fn a_flake_toolchains_sandbox_gets_nix_the_baked_devshell_and_a_writable_store() {
        let fake = FakeOpenShell::new();
        let repo = tempfile::tempdir().unwrap();
        let transport = fake.transport(Some(flake_origin(repo.path())));

        transport.create().unwrap();

        let (dockerfile, policy) = built_and_created(&fake);
        assert!(dockerfile.contains(" AS devshell\n"), "{dockerfile}");
        assert!(dockerfile.contains(" AS nix\n"), "{dockerfile}");
        assert!(paths(&policy, "read_write").contains(&"/nix".to_string()));
        assert!(!policy["network_policies"]["workmux-nix"].is_null());
    }

    #[test]
    fn without_a_flake_toolchain_a_flake_gets_neither_nix_nor_a_baked_devshell() {
        let fake = FakeOpenShell::new();
        let repo = tempfile::tempdir().unwrap();
        // The repository has a flake, but its toolchain is off.
        let origin = OpenshellRemote {
            nix: false,
            ..flake_origin(repo.path())
        };
        let transport = fake.transport(Some(origin));

        transport.create().unwrap();

        let (dockerfile, policy) = built_and_created(&fake);
        assert!(!dockerfile.contains("nixos/nix"), "{dockerfile}");
        assert!(paths(&policy, "read_only").contains(&"/nix".to_string()));
        assert!(!paths(&policy, "read_write").contains(&"/nix".to_string()));
        assert!(policy["network_policies"].is_null(), "{policy:?}");
    }

    #[test]
    fn with_gh_login_attached_the_devshell_bake_gets_the_gh_login_as_a_build_secret_never_on_argv()
    {
        let fake = FakeOpenShell::new();
        std::fs::write(fake.dir.path().join("gh-token"), "gho_secret\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        let transport = fake.transport(Some(flake_origin_with(
            repo.path(),
            &["gh-login", "anthropic"],
        )));

        transport.create().unwrap();

        let docker = std::fs::read_to_string(fake.dir.path().join("docker-log")).unwrap();
        let (build, secret) = docker.split_once('\n').unwrap();
        assert!(
            build.starts_with("build --secret id=github-token,env=WORKMUX_GITHUB_TOKEN -t "),
            "{build}"
        );
        assert_eq!(secret, "secret gho_secret\n");
        assert!(!fake.log().join("\n").contains("gho_secret"));
    }

    #[test]
    fn without_gh_login_attached_the_devshell_bake_goes_without_the_secret() {
        let fake = FakeOpenShell::new();
        // Logged in, but the project didn't give the sandbox the login.
        std::fs::write(fake.dir.path().join("gh-token"), "gho_secret\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        let transport = fake.transport(Some(flake_origin(repo.path())));

        transport.create().unwrap();

        let docker = std::fs::read_to_string(fake.dir.path().join("docker-log")).unwrap();
        let (build, secret) = docker.split_once('\n').unwrap();
        assert!(build.starts_with("build -t "), "{build}");
        assert_eq!(secret, "secret \n");
    }

    #[test]
    fn a_failed_devshell_bake_without_gh_login_says_private_inputs_need_it() {
        let fake = FakeOpenShell::new();
        std::fs::write(fake.dir.path().join("docker-fail"), "").unwrap();
        let repo = tempfile::tempdir().unwrap();
        let origin = flake_origin(repo.path());
        let without = fake.transport(Some(origin.clone()));
        std::fs::write(fake.dir.path().join("gh-token"), "gho_secret\n").unwrap();
        let mut origin = origin;
        origin.settings.providers = vec!["gh-login".to_string()];
        let with = fake.transport(Some(origin));

        let without = format!("{:#}", without.create().unwrap_err());
        let with = format!("{:#}", with.create().unwrap_err());

        assert!(
            without.contains(
                "private GitHub flake inputs can only be fetched with `gh-login` in sandbox.openshell.providers"
            ),
            "{without}"
        );
        assert!(!with.contains("can only be fetched with"), "{with}");
    }

    #[test]
    fn the_agent_report_matches_attached_providers_by_what_their_profiles_allow() {
        let fake = FakeOpenShell::new();
        let dir = fake.dir.path();
        std::fs::write(
            dir.join("providers"),
            r#"{"next_page_token":"page2","providers":[
                {"name":"github","type":"github"},
                {"name":"other","type":"claude-code"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("providerspage2"),
            r#"{"next_page_token":"","providers":[
                {"name":"anthropic","type":"claude-code-oauth"},
                {"name":"openai","type":"codex"}]}"#,
        )
        .unwrap();
        // A stock profile and a custom one; the stock codex profile is
        // not installed.
        std::fs::write(
            dir.join("profiles"),
            r#"[{"id":"claude-code","binaries":["/usr/local/bin/claude"]},
                {"id":"claude-code-oauth","binaries":["/usr/local/bin/claude","/usr/bin/claude"],
                 "endpoints":[{"host":"api.anthropic.com","port":443}]},
                {"id":"github","binaries":["/usr/bin/gh"]}]"#,
        )
        .unwrap();
        let mut origin = origin();
        origin.settings.providers = ["github", "anthropic", "openai"].map(String::from).to_vec();
        origin.agents = vec!["claude".to_string(), "codex".to_string()];

        let report = fake.transport(Some(origin)).agent_report().unwrap();

        assert_eq!(
            report.notes,
            [
                "claude: installed at /usr/local/bin/claude; its credentials come from a provider whose profile allows it, such as one of type claude-code (attached: anthropic)",
                "codex: installed at /usr/local/bin/codex; its credentials come from a provider whose profile allows it, such as one of type codex (attached: openai)",
            ]
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    impl FakeOpenShell {
        pub(in crate::remote::openshell) fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// The gateway's providers, as one page of `provider list`.
        pub(in crate::remote::openshell) fn providers(&self, providers: &str) {
            std::fs::write(
                self.path("providers"),
                format!(r#"{{"next_page_token":"","providers":{providers}}}"#),
            )
            .unwrap();
        }

        /// The gateway's profiles, as `provider profile list` prints them.
        pub(in crate::remote::openshell) fn profiles(&self, profiles: &str) {
            std::fs::write(self.path("profiles"), profiles).unwrap();
        }

        /// The tokens `openshell provider create/update` got in their
        /// environment.
        pub(in crate::remote::openshell) fn sent(&self) -> Vec<String> {
            std::fs::read_to_string(self.path("sent"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// Log the stand-in `gh` in with `token`.
        pub(in crate::remote::openshell) fn log_in(&self, token: &str) {
            std::fs::write(self.path("gh-token"), format!("{token}\n")).unwrap();
        }
    }

    #[test]
    fn a_sandbox_without_a_project_is_never_created() {
        let fake = FakeOpenShell::new();

        let error = fake.transport(None).create().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("whose config sets sandbox.backend: openshell"),
            "{error}"
        );
        assert!(
            fake.log()
                .iter()
                .all(|line| !line.starts_with("sandbox create"))
        );
    }

    #[test]
    fn without_a_linux_workmux_create_fails_before_any_docker_build() {
        let fake = FakeOpenShell::new();
        std::fs::remove_file(fake.path("linux-workmux")).unwrap();

        let error = fake.transport(Some(origin())).create().unwrap_err();

        assert_eq!(
            error.to_string(),
            "No workmux release for this build can serve OpenShell sandboxes yet; \
             run `workmux sandbox install-dev`"
        );
        assert!(!fake.path("docker-log").exists());
        assert!(
            fake.log()
                .iter()
                .all(|line| !line.starts_with("sandbox create"))
        );
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
    fn a_sandbox_started_by_name_alone_keeps_its_policy() {
        let fake = FakeOpenShell::new();
        let project = fake.transport(Some(origin()));
        project.create().unwrap();
        project.stop().unwrap();
        let before = fake.log().len();

        // As `sandbox prune` starts another repository's sandbox to check
        // its work: the policy its project gives it is unknown here.
        fake.transport(None).start().unwrap();

        assert_eq!(project.status().unwrap(), RemoteState::Running);
        let calls: Vec<String> = fake.log()[before..]
            .iter()
            .map(|line| line.split(' ').take(2).collect::<Vec<_>>().join(" "))
            .collect();
        assert!(
            calls
                .iter()
                .all(|call| call == "sandbox get" || call == "sandbox start"),
            "{calls:?}"
        );
    }

    /// What the gateway prints when its supervisor image is gone from
    /// Docker, as seen after an image prune removed that untagged image.
    const NO_SUPERVISOR_IMAGE: &str = "ControlSupervisorStartFailed create Docker supervisor staging container: Docker responded with status code 404: No such image: sha256:4efdb659add55a99e1bb6e5773d7b35a51770106a3d819c000fd078edfc098b3";

    #[test]
    fn a_missing_supervisor_image_fails_create_and_start_with_a_restart_hint() {
        let fake = FakeOpenShell::new();
        let transport = fake.transport(Some(origin()));
        transport.create().unwrap();
        transport.stop().unwrap();
        std::fs::write(fake.path("fail"), NO_SUPERVISOR_IMAGE).unwrap();

        let start = transport.start().unwrap_err();
        transport.delete().unwrap();
        let create = transport.create().unwrap_err();

        for error in [start, create] {
            let error = format!("{error:#}");
            assert!(error.contains(NO_SUPERVISOR_IMAGE), "{error}");
            assert!(
                error.contains("supervisor image is no longer in Docker"),
                "{error}"
            );
            assert!(error.contains("restarting the gateway"), "{error}");
            assert!(error.contains("launchctl kickstart"), "{error}");
        }
    }

    #[test]
    fn other_create_errors_pass_through_unchanged() {
        let fake = FakeOpenShell::new();
        std::fs::write(fake.path("fail"), "Error:   × sandbox name already taken").unwrap();

        let error = fake.transport(Some(origin())).create().unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Error:   × sandbox name already taken"
        );
    }

    #[test]
    fn status_shows_the_effective_policy() {
        let policy = serde_json::to_value(
            policy::effective(
                Some("network_policies:\n  github: {endpoints: []}\n"),
                false,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            describe_policy(&policy),
            [
                "read-only:  /bin, /usr, /lib, /proc, /dev/urandom, /etc, /var/log, /nix",
                "read-write: /tmp, /dev/null, /dev/pts, /dev/ptmx",
                "workdir:    read-write",
                "network:    github",
            ]
        );
    }

    #[test]
    fn a_policy_that_only_widens_is_applied_in_place() {
        let current = serde_json::to_value(policy::effective(None, false).unwrap()).unwrap();
        let widened = serde_json::to_value(
            policy::effective(
                Some("network_policies:\n  github: {endpoints: []}\n"),
                false,
            )
            .unwrap(),
        )
        .unwrap();

        assert!(same_policy(&current, &current));
        assert!(!same_policy(&current, &widened));
        let mut with_empty = current.clone();
        with_empty["network_policies"] = serde_json::json!({});
        assert!(same_policy(&current, &with_empty));
    }
}

/// Contract tests against a real OpenShell gateway, env-gated like the
/// OpenShell learning tests: `WORKMUX_TEST_OPENSHELL=1`. Each test owns one
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
    /// (git, dtach, python3), to which workmux adds the Linux workmux
    /// `workmux sandbox install-dev` installed.
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
                .args([
                    "image",
                    "ls",
                    "-q",
                    &format!("workmux-openshell-{}", self.0.sandbox),
                ])
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
            path: "~/api".to_string(),
            settings: crate::config::OpenshellProject {
                image: Some(fixture_image()),
                ..Default::default()
            },
            agents: Vec::new(),
            nix: false,
            extra_mounts: false,
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "held\n",
            "{output:?}"
        );

        transport.delete().unwrap();
        assert_eq!(transport.status().unwrap(), RemoteState::Missing);
    }

    #[test]
    fn a_flake_projects_sandbox_has_its_baked_devshell_and_a_working_nix() {
        if !enabled() {
            return;
        }
        // The project, as the workstation clones it from its git URL.
        let repo = tempfile::tempdir().unwrap();
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openshell/devshell"
        );
        for file in ["flake.nix", "flake.lock"] {
            std::fs::copy(format!("{fixture}/{file}"), repo.path().join(file)).unwrap();
        }
        crate::test_support::init_repo(repo.path());
        crate::test_support::run_git(repo.path(), &["add", "flake.nix", "flake.lock"]);
        crate::test_support::run_git(repo.path(), &["commit", "-q", "-m", "flake"]);
        let mut owned = sandbox("nix");
        if let Some(origin) = owned.0.origin.as_mut() {
            origin.git_url = Some(crate::remote::git_url::GitUrl::new(
                repo.path().display().to_string(),
            ));
            origin.nix = true;
        }
        let transport = &owned.0;

        transport.create().unwrap();

        let output = transport
            .exec_captured(&argv(&[
                crate::remote::provision::BAKED_DEVSHELL,
                "sh",
                "-c",
                "jq --version && rg --version | head -1 && echo devshell=$WM_LEARN_DEVSHELL && git --version",
            ]))
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert!(stdout.contains("jq-"), "{stdout}");
        assert!(stdout.contains("ripgrep"), "{stdout}");
        assert!(stdout.contains("devshell=1"), "{stdout}");
        assert!(stdout.contains("git version"), "{stdout}");
        let source = transport
            .exec_captured(&argv(&["cat", image::DEVSHELL_SOURCE]))
            .unwrap();
        let source: image::DevshellSource = serde_json::from_slice(&source.stdout).unwrap();
        assert_eq!(source.clone, "/sandbox/api");
        assert_eq!(source.devshell_sha256, image::devshell_digest(repo.path()));

        // nix works on the flake: the devshell and the flake's inputs are
        // in the store already, so `nix develop` needs no network.
        let sh = |script: &str| {
            let output = transport
                .exec_captured(&argv(&["sh", "-c", script]))
                .unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.status.success(), "{script}: {text}");
            text
        };
        assert!(sh("nix --version").contains("2.24"));
        for file in ["flake.nix", "flake.lock"] {
            sh(&format!(
                "mkdir -p /tmp/flake && cat > /tmp/flake/{file} <<'EOF'\n{}\nEOF",
                std::fs::read_to_string(repo.path().join(file)).unwrap()
            ));
        }
        let develop = sh(
            "nix develop --offline /tmp/flake -c sh -c 'jq --version; echo devshell=$WM_LEARN_DEVSHELL'",
        );
        assert!(develop.contains("devshell=1"), "{develop}");
        sh("nix develop /tmp/flake -c true");
        // A check that builds, from the same nixpkgs and the binary cache.
        sh("mkdir -p /tmp/check && cp /tmp/flake/flake.lock /tmp/check/ && cat > /tmp/check/flake.nix <<'EOF'
{
  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/nixos-25.05\";
  outputs = { nixpkgs, ... }: {
    checks = nixpkgs.lib.genAttrs [ \"x86_64-linux\" \"aarch64-linux\" ] (system: {
      ok = nixpkgs.legacyPackages.${system}.runCommand \"ok\" { } \"touch $out\";
    });
  };
}
EOF");
        sh("nix flake check /tmp/check");
        let hello = sh("nix profile install nixpkgs#hello && ~/.nix-profile/bin/hello");
        assert!(hello.contains("Hello, world!"), "{hello}");
        // The devshell and the flake's inputs are garbage collection roots.
        sh("nix store gc && nix-collect-garbage && nix develop --offline /tmp/flake -c true");
    }

    #[test]
    fn a_sandbox_whose_toolchain_is_off_has_no_nix() {
        if !enabled() {
            return;
        }
        let owned = sandbox("nonix");
        let transport = &owned.0;

        transport.create().unwrap();

        let nix = transport
            .exec_captured(&argv(&["sh", "-c", "command -v nix"]))
            .unwrap();
        assert!(!nix.status.success(), "{nix:?}");
        let policy = transport.current_policy().unwrap().unwrap();
        let read_only: Vec<String> =
            serde_json::from_value(policy["filesystem_policy"]["read_only"].clone()).unwrap();
        let read_write: Vec<String> =
            serde_json::from_value(policy["filesystem_policy"]["read_write"].clone()).unwrap();
        assert!(read_only.contains(&"/nix".to_string()), "{policy}");
        assert!(!read_write.contains(&"/nix".to_string()), "{policy}");
        assert!(
            policy["network_policies"]
                .as_object()
                .is_none_or(|rules| rules.is_empty()),
            "{policy}"
        );
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
        if !enabled() {
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
