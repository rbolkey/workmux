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
    gh: PathBuf,
    /// workmux's state directory (default: the XDG one).
    state: Option<PathBuf>,
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
            state: None,
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
                    bail!("sandbox {} is in phase Error", self.sandbox)
                }
                (Phase::Missing, Some(wanted)) => {
                    bail!(
                        "sandbox {} disappeared while waiting for {wanted}",
                        self.sandbox
                    )
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
        policy::effective(user.as_deref())
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

    /// Whether the sandbox gets the workstation's `gh` login (`--github
    /// host`).
    pub fn github_host(&self) -> bool {
        self.origin.as_ref().is_some_and(|origin| {
            origin.settings.github == Some(crate::config::GithubCredentials::Host)
        })
    }

    /// The providers attached to the sandbox: the project's, and the one
    /// holding the `gh` login for `--github host`.
    fn attached_providers(&self) -> Vec<String> {
        let mut providers = self
            .origin
            .as_ref()
            .map(|origin| origin.settings.providers.clone())
            .unwrap_or_default();
        if self.github_host() {
            providers.push(github::PROVIDER.to_string());
        }
        providers
    }

    /// For `project add --github host`: the `gh` login must be there, and
    /// no attached provider may already give GitHub credentials. Returns
    /// warnings, for a gateway that can't be asked.
    pub fn check_github_host(&self) -> Result<Vec<String>> {
        if !self.github_host() {
            return Ok(Vec::new());
        }
        github::host_token(&self.gh)?;
        match self.github_providers() {
            Ok(providers) => github::refuse_alongside(&providers).map(|()| Vec::new()),
            Err(error) => Ok(vec![format!(
                "could not check whether an attached provider already gives GitHub credentials ({error:#})"
            )]),
        }
    }

    /// The project's own attached providers that give git credentials for
    /// github.com (not the one `--github host` attaches).
    pub fn github_providers(&self) -> Result<Vec<String>> {
        let Some(origin) = &self.origin else {
            return Ok(Vec::new());
        };
        let attached = &origin.settings.providers;
        if attached.is_empty() {
            return Ok(Vec::new());
        }
        let providers = self.providers()?;
        Ok(attached
            .iter()
            .filter(|name| {
                providers
                    .get(*name)
                    .is_some_and(agents::Provider::gives_github)
            })
            .cloned()
            .collect())
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

    /// Every provider on the gateway by name, with the binaries its
    /// profile allows.
    fn providers(&self) -> Result<std::collections::BTreeMap<String, agents::Provider>> {
        let profiles = self.profiles()?;
        let profile = |provider_type: &str| {
            profiles
                .as_array()?
                .iter()
                .find(|profile| profile["id"] == provider_type)
        };
        let binaries = |provider_type: &str| {
            profile(provider_type)?["binaries"]
                .as_array()
                .map(|binaries| {
                    binaries
                        .iter()
                        .filter_map(|binary| Some(binary.as_str()?.to_string()))
                        .collect()
                })
        };
        let hosts = |provider_type: &str| {
            profile(provider_type).map(|profile| {
                profile["endpoints"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|endpoint| Some(endpoint["host"].as_str()?.to_string()))
                    .collect()
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
                    binaries: binaries(provider_type),
                    hosts: hosts(provider_type),
                },
            );
        }
        Ok(providers)
    }

    /// Import workmux's GitHub profile into the gateway, or update it there
    /// when it differs.
    fn sync_github_profile(&self) -> Result<()> {
        let profiles = self.profiles()?;
        let installed = profiles
            .as_array()
            .into_iter()
            .flatten()
            .find(|profile| profile["id"] == github::PROFILE);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("profile.yaml");
        let path_arg = path.to_string_lossy();
        match installed {
            None => {
                std::fs::write(&path, github::profile_yaml())?;
                self.run(&["provider", "profile", "import", "--global", "-f", &path_arg])?;
            }
            Some(installed) if github::profile_differs(installed) => {
                std::fs::write(
                    &path,
                    github::profile_update_yaml(&installed["resource_version"])?,
                )?;
                self.run(&[
                    "provider",
                    "profile",
                    "update",
                    "--global",
                    "-f",
                    &path_arg,
                    github::PROFILE,
                ])?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Give the provider holding the `gh` login the login's current
    /// token, creating the provider on first use; at sandbox create
    /// (`sync_profile`), workmux's profile is brought up to date too. An
    /// unchanged token is not sent again. The token reaches `openshell`
    /// through its environment only, never its arguments.
    fn refresh_github_login(&self, sync_profile: bool) -> Result<()> {
        if !self.github_host() {
            return Ok(());
        }
        let token = github::host_token(&self.gh)?;
        if sync_profile {
            self.sync_github_profile()?;
        }
        let record = self.github_login_record();
        let id = match self.provider_id(github::PROVIDER)? {
            Some(id) => {
                let sent = record
                    .as_ref()
                    .and_then(|record| std::fs::read_to_string(record).ok());
                if sent.as_deref() == Some(github::digest(&id, &token).as_str()) {
                    return Ok(());
                }
                self.run_with_token(
                    &[
                        "provider",
                        "update",
                        github::PROVIDER,
                        "--credential",
                        github::CREDENTIAL,
                        "--wait",
                    ],
                    &token,
                )?;
                id
            }
            None => {
                if !sync_profile {
                    self.sync_github_profile()?;
                }
                self.run_with_token(
                    &[
                        "provider",
                        "create",
                        "--name",
                        github::PROVIDER,
                        "--type",
                        github::PROFILE,
                        "--global-profile",
                        "--credential",
                        github::CREDENTIAL,
                    ],
                    &token,
                )?;
                self.provider_id(github::PROVIDER)?.ok_or_else(|| {
                    anyhow!("provider {} is missing after its create", github::PROVIDER)
                })?
            }
        };
        if let Some(record) = record {
            let written = record
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&record, github::digest(&id, &token)));
            if let Err(error) = written {
                tracing::warn!(error = %error, "recording the gh login given to the gateway");
            }
        }
        Ok(())
    }

    /// The id of provider `name`, or `None` when the gateway has none.
    fn provider_id(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .provider_list()?
            .iter()
            .find(|provider| provider["name"] == name)
            .map(|provider| provider["id"].as_str().unwrap_or_default().to_string()))
    }

    /// Where workmux records which `gh` token it last gave the gateway.
    fn github_login_record(&self) -> Option<PathBuf> {
        let state = match &self.state {
            Some(state) => state.clone(),
            None => crate::xdg::state_dir().ok()?,
        };
        Some(state.join("openshell").join("github-login"))
    }

    /// Run an `openshell` command that reads the credential from its
    /// environment.
    fn run_with_token(&self, args: &[&str], token: &str) -> Result<()> {
        let output = self
            .openshell()
            .args(args)
            .env(github::CREDENTIAL, token)
            .stdin(Stdio::null())
            .output()
            .context("failed to run openshell")?;
        if !output.status.success() {
            bail!("{}", failure(&output));
        }
        Ok(())
    }

    /// Whether the image's git gets a credential helper for github.com.
    /// When the providers can't be looked up it does: the helper hands git
    /// only the placeholder already in the sandbox's environment.
    fn github_credentials(&self) -> bool {
        if self.github_host() {
            return true;
        }
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
    fn build_image(&self, origin: &OpenshellRemote, context: &std::path::Path) -> Result<String> {
        let checkout = context.join("project");
        // The workstation has SSH: clone the way the user gave the URL.
        let devshell = origin
            .ssh_git_url
            .as_deref()
            .or(origin.git_url.as_deref())
            .is_some_and(|url| self.fetch_flake(url, &checkout))
            .then_some(checkout);
        let workmux = image::Workmux::current();
        if let image::Workmux::Source(source) = &workmux {
            eprintln!(
                "This workmux is a dev build: building a Linux workmux from {} into the sandbox image",
                source.display()
            );
        }
        let recipe = image::Recipe {
            base: origin.settings.image.clone(),
            workmux,
            agents: origin.agents.clone(),
            devshell,
            github_credentials: self.github_credentials(),
        };
        let tag = image::prepare(&recipe, &self.sandbox, &sandbox_path(&origin.path), context)?;
        image::build(&self.docker, context, &tag)?;
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
                .context("failed to run git")
        });
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
            format!("workmux.project={}", origin.project),
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
                "the policy of sandbox {} no longer grants {}, which OpenShell cannot take away from a sandbox; run `workmux remote recreate {}` to apply it",
                self.sandbox,
                narrowed.join(", "),
                self.origin
                    .as_ref()
                    .map_or(self.sandbox.as_str(), |o| o.project.as_str())
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
                "updated the policy of sandbox {}; its new filesystem grants apply after `workmux remote stop` and `workmux remote start`",
                self.sandbox
            )]
        } else {
            vec![format!("updated the policy of sandbox {}", self.sandbox)]
        })
    }
}

/// The lines `workmux remote status` shows for a sandbox's effective
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
    /// The sandbox's effective policy for `workmux remote status`, and
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
                        "the project's policy no longer grants {}; run `workmux remote recreate` to apply it",
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
        self.refresh_github_login(true)?;
        let context = tempfile::tempdir()?;
        let image = self.build_image(origin, context.path())?;
        for warning in self.agent_report().map(|r| r.warnings).unwrap_or_default() {
            eprintln!("warning: {warning}");
        }
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
        self.refresh_github_login(false)?;
        self.sync_policy(true)
    }

    fn refresh_credentials(&self) -> Result<()> {
        self.refresh_github_login(false)
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
            ssh_git_url: None,
            path: "~/api".to_string(),
            settings: OpenshellProject {
                providers: vec!["github".to_string(), "anthropic".to_string()],
                ..OpenshellProject::default()
            },
            agents: Vec::new(),
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
                 'provider list') token=; while [ $# -gt 0 ]; do [ \"$1\" = --page-token ] && token=$2; shift; done;\n\
                   cat \"$dir/providers$token\" ;;\n\
                 'provider profile') case $3 in\n\
                   list) cat \"$dir/profiles\" ;;\n\
                   *) while [ \"$1\" != -f ]; do shift; done; cp \"$2\" \"$dir/imported\" ;;\n\
                   esac ;;\n\
                 'provider create') echo \"create $GH_TOKEN\" >> \"$dir/sent\";\n\
                   echo '{{\"next_page_token\":\"\",\"providers\":[{{\"name\":\"workmux-gh-host\",\"type\":\"workmux-github\",\"id\":\"id-1\"}}]}}' > \"$dir/providers\" ;;\n\
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
            for (name, body) in [
                ("openshell", script.as_str()),
                ("docker", "#!/bin/sh\nexit 0\n"),
                ("gh", gh.as_str()),
            ] {
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
                gh: self.dir.path().join("gh"),
                state: Some(self.dir.path().join("state")),
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
                "sandbox",
                "exec",
                "-n",
                "api",
                "--no-login-shell",
                "--no-tty",
                "--",
                "workmux",
                "remote",
                "serve"
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

    #[test]
    fn github_credentials_come_from_providers_whose_profile_reaches_github() {
        let fake = FakeOpenShell::new();
        let dir = fake.dir.path();
        std::fs::write(
            dir.join("providers"),
            r#"{"next_page_token":"","providers":[
                {"name":"work","type":"github-dev"},
                {"name":"stock","type":"github"},
                {"name":"anthropic","type":"claude-code"},
                {"name":"unused","type":"github-dev"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles"),
            r#"[{"id":"github-dev","binaries":["/usr/bin/git"],
                 "endpoints":[{"host":"api.github.com","port":443},{"host":"github.com","port":443}]},
                {"id":"claude-code","binaries":["/usr/local/bin/claude"],
                 "endpoints":[{"host":"api.anthropic.com","port":443}]}]"#,
        )
        .unwrap();
        let mut origin = origin();
        // `stock`'s profile is not installed: its stock type is enough.
        origin.settings.providers = ["anthropic", "work", "stock"].map(String::from).to_vec();

        let github = fake.transport(Some(origin)).github_providers().unwrap();

        assert_eq!(github, ["work", "stock"]);
        let mut origin = self::origin();
        origin.settings.providers.clear();
        assert!(
            fake.transport(Some(origin))
                .github_providers()
                .unwrap()
                .is_empty()
        );
    }

    fn host_login_origin() -> OpenshellRemote {
        let mut origin = origin();
        origin.settings.providers = vec!["anthropic".to_string()];
        origin.settings.github = Some(crate::config::GithubCredentials::Host);
        origin
    }

    impl FakeOpenShell {
        /// The tokens `openshell provider create/update` got in their
        /// environment.
        fn sent(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("sent"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn log_in(&self, token: &str) {
            std::fs::write(self.dir.path().join("gh-token"), format!("{token}\n")).unwrap();
        }
    }

    #[test]
    fn the_gh_login_is_kept_in_a_provider_created_on_first_use_and_attached_to_the_sandbox() {
        let fake = FakeOpenShell::new();
        let dir = fake.dir.path();
        std::fs::write(
            dir.join("providers"),
            r#"{"next_page_token":"","providers":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("profiles"), "[]").unwrap();
        fake.log_in("gho_first");
        let transport = fake.transport(Some(host_login_origin()));

        transport.create().unwrap();

        let log = fake.log();
        assert!(
            log.iter()
                .any(|line| line.starts_with("provider profile import --global -f ")),
            "{log:?}"
        );
        let imported: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("imported")).unwrap()).unwrap();
        assert_eq!(imported["id"], "workmux-github");
        assert!(log.contains(
            &"provider create --name workmux-gh-host --type workmux-github --global-profile --credential GH_TOKEN"
                .to_string()
        ));
        assert_eq!(fake.sent(), ["create gho_first"]);
        let create = log
            .iter()
            .find(|line| line.starts_with("sandbox create"))
            .unwrap();
        assert!(
            create.contains("--provider anthropic --provider workmux-gh-host"),
            "{create}"
        );
        assert!(
            log.iter().all(|line| !line.contains("gho_first")),
            "{log:?}"
        );
    }

    #[test]
    fn a_new_gh_login_updates_the_provider_and_an_unchanged_one_is_not_sent_again() {
        let fake = FakeOpenShell::new();
        let dir = fake.dir.path();
        std::fs::write(
            dir.join("providers"),
            r#"{"next_page_token":"","providers":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("profiles"), "[]").unwrap();
        fake.log_in("gho_first");
        let transport = fake.transport(Some(host_login_origin()));
        transport.create().unwrap();

        transport.reconcile().unwrap();
        assert_eq!(fake.sent(), ["create gho_first"]);

        fake.log_in("gho_second");
        transport.refresh_credentials().unwrap();
        transport.refresh_credentials().unwrap();

        assert_eq!(fake.sent(), ["create gho_first", "update gho_second"]);
        assert!(
            fake.log().contains(
                &"provider update workmux-gh-host --credential GH_TOKEN --wait".to_string()
            )
        );
        assert!(fake.log().iter().all(|line| !line.contains("gho_")));
    }

    #[test]
    fn a_logged_out_gh_fails_create_and_attach_closed() {
        let fake = FakeOpenShell::new();
        std::fs::write(
            fake.dir.path().join("providers"),
            r#"{"next_page_token":"","providers":[]}"#,
        )
        .unwrap();
        std::fs::write(fake.dir.path().join("profiles"), "[]").unwrap();
        let transport = fake.transport(Some(host_login_origin()));

        let error = transport.create().unwrap_err().to_string();

        assert!(error.ends_with("run `gh auth login`"), "{error}");
        assert!(
            fake.log()
                .iter()
                .all(|line| !line.starts_with("sandbox create"))
        );
        let error = transport.refresh_credentials().unwrap_err().to_string();
        assert!(error.contains("gh is not logged in"), "{error}");
    }

    #[test]
    fn an_outdated_shipped_profile_is_updated_at_create() {
        let fake = FakeOpenShell::new();
        let dir = fake.dir.path();
        std::fs::write(
            dir.join("providers"),
            r#"{"next_page_token":"","providers":[{"name":"workmux-gh-host","type":"workmux-github","id":"id-1"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles"),
            r#"[{"id":"workmux-github","resource_version":2,"binaries":["/usr/bin/git"],
                 "endpoints":[{"host":"github.com","port":443}]}]"#,
        )
        .unwrap();
        fake.log_in("gho_first");

        fake.transport(Some(host_login_origin())).create().unwrap();

        let log = fake.log();
        assert!(
            log.iter().any(
                |line| line.starts_with("provider profile update --global -f ")
                    && line.ends_with(" workmux-github")
            ),
            "{log:?}"
        );
        let imported: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("imported")).unwrap()).unwrap();
        assert_eq!(imported["resource_version"], 2);
        // The provider exists: its token is updated, not created.
        assert_eq!(fake.sent(), ["update gho_first"]);
    }

    #[test]
    fn project_add_checks_the_gh_login_and_refuses_it_beside_a_github_provider() {
        let fake = FakeOpenShell::new();
        let dir = fake.dir.path();
        std::fs::write(
            dir.join("providers"),
            r#"{"next_page_token":"","providers":[{"name":"anthropic","type":"claude-code"},{"name":"work","type":"github"}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("profiles"), "[]").unwrap();

        let error = fake
            .transport(Some(host_login_origin()))
            .check_github_host()
            .unwrap_err();
        assert!(
            error.to_string().ends_with("run `gh auth login`"),
            "{error}"
        );

        fake.log_in("gho_first");
        assert!(
            fake.transport(Some(host_login_origin()))
                .check_github_host()
                .unwrap()
                .is_empty()
        );
        let mut origin = host_login_origin();
        origin.settings.providers.push("work".to_string());
        let error = fake
            .transport(Some(origin))
            .check_github_host()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("provider work already gives it GitHub credentials"),
            "{error}"
        );
        // Nothing is created before first use.
        assert!(fake.sent().is_empty());
    }

    #[test]
    fn a_registered_sandbox_without_a_project_is_never_created() {
        let fake = FakeOpenShell::new();

        let error = fake.transport(None).create().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("workmux creates sandboxes only for projects"),
            "{error}"
        );
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
    fn status_shows_the_effective_policy() {
        let policy = serde_json::to_value(
            policy::effective(Some("network_policies:\n  github: {endpoints: []}\n")).unwrap(),
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
    /// (git, dtach, python3), to which workmux adds a Linux workmux built
    /// from this checkout, or the one `WORKMUX_OPENSHELL_WORKMUX` names.
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
            ssh_git_url: None,
            path: "~/api".to_string(),
            settings: crate::config::OpenshellProject {
                image: Some(fixture_image()),
                ..Default::default()
            },
            agents: Vec::new(),
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
    fn a_flake_projects_devshell_is_baked_into_the_image_and_runs_in_the_sandbox() {
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
            origin.git_url = Some(repo.path().display().to_string());
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
        assert_eq!(
            source.flake_lock_sha256,
            image::flake_lock_digest(repo.path())
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
