//! The launch plan: what a pane launch asks of sbx.
//!
//! [`launch_plan`] is pure. Callers gather sbx's state (see
//! [`super::instance::gather_state`]), compute the plan, run its setup
//! commands, then run its exec in the foreground.

use anyhow::{Result, bail};
use std::path::Path;

use crate::config::SandboxConfig;

use super::instance::{SbxCheck, SbxRule, SbxSandboxInfo};

/// The host sbx runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `std::env::consts::OS`
    pub os: String,
    /// `std::env::consts::ARCH`
    pub arch: String,
    /// Whether `/dev/kvm` exists (Linux only)
    pub kvm: bool,
}

/// What sbx reports about a worktree's sandbox, as parsed data.
#[derive(Debug, Clone)]
pub struct SbxState {
    pub platform: Platform,
    /// Checks from `sbx diagnose --json`.
    pub checks: Vec<SbxCheck>,
    /// The sandbox from `sbx ls --json`, or `None` when it doesn't exist.
    pub sandbox: Option<SbxSandboxInfo>,
    /// Rules from `sbx policy ls <name> --json`; empty when the sandbox is missing.
    pub rules: Vec<SbxRule>,
}

/// The worktree a launch is for, with the host paths the plan mounts.
#[derive(Debug, Clone)]
pub struct LaunchTarget<'a> {
    /// The worktree's sandbox name (see [`super::sandbox_name`]).
    pub sandbox_name: &'a str,
    pub worktree_root: &'a Path,
    /// The pane's working directory, under `worktree_root`.
    pub pane_cwd: &'a Path,
    /// The main repository's git dir, when it lies outside `worktree_root`.
    pub git_common_dir: Option<&'a Path>,
    /// Host-side state directory for the sandbox (shims live under it).
    pub state_dir: &'a Path,
}

/// The command and environment to run in the sandbox.
#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    pub command: String,
    pub env: Vec<(String, String)>,
}

/// One `sbx` invocation. Environment values are passed through the process
/// environment and named with a bare `-e KEY`, so they never appear in argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbxCommand {
    /// Arguments after `sbx`.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The ordered sbx commands for one launch.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub sandbox_name: String,
    /// Template the sandbox is created from, when the plan creates it.
    pub create_template: Option<String>,
    /// Commands to run, in order, before `exec`.
    pub setup: Vec<SbxCommand>,
    /// The interactive exec that runs the command.
    pub exec: SbxCommand,
}

/// Home directory of sbx's `agent` user in the guest.
const GUEST_HOME: &str = "/home/agent";

/// The image's system PATH.
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Policy resource for workmux's RPC server: sbx's name for the workstation,
/// on every port (RPC ports are ephemeral).
const RPC_RULE_RESOURCE: &str = "localhost";

/// Where an agent's config directory is linked under the guest home, or
/// `None` for agents the docker_sbx backend doesn't support yet.
fn agent_home_subpath(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some(".claude"),
        _ => None,
    }
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// The interactive `sbx exec` that runs the command in the pane's working
/// directory. Values travel in the process environment; argv names them only.
fn exec_command(target: &LaunchTarget, exec: &ExecRequest) -> SbxCommand {
    let mut args = words(&["exec", "-it", "-w"]);
    args.push(target.pane_cwd.to_string_lossy().into_owned());
    for (key, _) in &exec.env {
        args.push("-e".to_string());
        args.push(key.clone());
    }
    // Put the shims first. PATH goes by value: in sbx's own environment it
    // would also change where sbx itself is looked up.
    args.push("-e".to_string());
    args.push(format!(
        "PATH={}:{GUEST_HOME}/.local/bin:{GUEST_PATH}",
        target.state_dir.join("shims/bin").display()
    ));
    args.push(target.sandbox_name.to_string());
    // bash, so BASH_ENV brings in the environment sbx keeps in
    // /etc/sandbox-persistent.sh.
    args.extend(words(&["bash", "-c", &exec.command]));
    SbxCommand {
        args,
        env: exec.env.clone(),
    }
}

/// Whether sbx's diagnostics report `name` as failed.
fn check_failed(state: &SbxState, name: &str) -> bool {
    state
        .checks
        .iter()
        .any(|check| check.name == name && check.status == "fail")
}

/// Fail closed unless sbx can run sandboxes. workmux never runs `sbx login`
/// or `sbx policy init` itself.
fn check_prerequisites(state: &SbxState) -> Result<()> {
    let platform = &state.platform;
    let supported = match platform.os.as_str() {
        "macos" => platform.arch == "aarch64",
        "linux" => platform.kvm,
        "windows" => true,
        _ => false,
    };
    if !supported {
        bail!(
            "Docker Sandboxes doesn't support this host ({} on {}). sbx runs on macOS 14+ \
             on Apple silicon, Windows 11, and Ubuntu 24.04+ with KVM.\n\
             Use the container or lima sandbox backend instead.",
            platform.os,
            platform.arch
        );
    }
    if check_failed(state, "Authentication") {
        bail!(
            "sbx is not logged in. Sign in to Docker with `sbx login`, then try again.\n\
             Or disable sandbox: set 'sandbox.enabled: false' in config."
        );
    }
    if check_failed(state, "Global network policy") {
        bail!(
            "sbx's global network policy has not been initialized. Choose one with \
             `sbx policy init`, then try again; workmux never picks it for you."
        );
    }
    Ok(())
}

/// Compute the sbx commands a launch needs.
///
/// When the sandbox is missing: create it from the template with sbx's
/// generic `shell` agent (agent kits would write into the mounted config
/// directory), mounting the worktree, the main repository's git dir, the
/// agent's config directory and the shims, all at their host paths.
pub fn launch_plan(
    config: &SandboxConfig,
    target: &LaunchTarget,
    agent: &str,
    state: &SbxState,
    exec: &ExecRequest,
) -> Result<LaunchPlan> {
    check_prerequisites(state)?;
    if agent_home_subpath(agent).is_none() {
        bail!(
            "The docker_sbx sandbox backend doesn't support agent '{agent}' yet; \
             it supports claude. Use the container or lima backend for {agent}."
        );
    }

    let name = target.sandbox_name;
    let mut setup = Vec::new();
    let mut create_template = None;

    if state.sandbox.is_none() {
        let template = super::template::template_tag(&config.resolved_image(agent));
        let mut args = words(&[
            "create",
            "--name",
            name,
            "--no-progress",
            "--template",
            &template,
            "--pull",
            "never",
            "shell",
        ]);
        args.push(target.worktree_root.to_string_lossy().into_owned());
        if let Some(git_dir) = target.git_common_dir {
            args.push(git_dir.to_string_lossy().into_owned());
        }
        let config_dir = config.resolved_agent_config_dir(agent);
        if let Some(config_dir) = &config_dir {
            args.push(config_dir.to_string_lossy().into_owned());
        }
        args.push(format!("{}:ro", target.state_dir.display()));
        setup.push(SbxCommand {
            args,
            env: Vec::new(),
        });
        create_template = Some(template);

        // sbx mounts workspaces only at their host paths: link the agent's
        // config directory to where the agent looks for it.
        if let (Some(config_dir), Some(subpath)) = (config_dir, agent_home_subpath(agent)) {
            let mut args = words(&["exec", "-u", "root", name, "ln", "-sfnT"]);
            args.push(config_dir.to_string_lossy().into_owned());
            args.push(format!("{GUEST_HOME}/{subpath}"));
            setup.push(SbxCommand {
                args,
                env: Vec::new(),
            });
        }
    }

    // One rule scoped to the sandbox lets the guest reach workmux's RPC
    // server through host.docker.internal; sbx names the host `localhost`.
    let sandbox_scope = format!("sandbox:{name}");
    let has_rpc_rule = state.rules.iter().any(|rule| {
        rule.scope == sandbox_scope
            && rule.decision == "allow"
            && rule.resources.iter().any(|r| r == RPC_RULE_RESOURCE)
    });
    if !has_rpc_rule {
        setup.push(SbxCommand {
            args: words(&[
                "policy",
                "allow",
                "network",
                "--sandbox",
                name,
                RPC_RULE_RESOURCE,
            ]),
            env: Vec::new(),
        });
    }

    Ok(LaunchPlan {
        sandbox_name: name.to_string(),
        create_template,
        setup,
        exec: exec_command(target, exec),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn config() -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            backend: Some(crate::config::SandboxBackend::DockerSbx),
            image: Some("test-image:latest".to_string()),
            agent_config_dir: Some("/home/me/.{agent}".to_string()),
            ..Default::default()
        }
    }

    fn passing_checks() -> Vec<SbxCheck> {
        [
            ("Authentication", "pass"),
            ("Global network policy", "pass"),
        ]
        .into_iter()
        .map(|(name, status)| SbxCheck {
            name: name.to_string(),
            status: status.to_string(),
        })
        .collect()
    }

    fn missing() -> SbxState {
        SbxState {
            platform: Platform {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                kvm: false,
            },
            checks: passing_checks(),
            sandbox: None,
            rules: Vec::new(),
        }
    }

    struct Paths {
        worktree: PathBuf,
        cwd: PathBuf,
        git: PathBuf,
        state: PathBuf,
    }

    fn paths() -> Paths {
        Paths {
            worktree: PathBuf::from("/code/proj__worktrees/feat"),
            cwd: PathBuf::from("/code/proj__worktrees/feat/sub"),
            git: PathBuf::from("/code/proj/.git"),
            state: PathBuf::from("/state/docker-sbx/wm-proj-12345678"),
        }
    }

    fn target(p: &Paths) -> LaunchTarget<'_> {
        LaunchTarget {
            sandbox_name: "wm-proj-12345678",
            worktree_root: &p.worktree,
            pane_cwd: &p.cwd,
            git_common_dir: Some(&p.git),
            state_dir: &p.state,
        }
    }

    fn exec() -> ExecRequest {
        ExecRequest {
            command: "claude --dangerously-skip-permissions".to_string(),
            env: vec![
                ("WM_RPC_PORT".to_string(), "4242".to_string()),
                ("WM_RPC_TOKEN".to_string(), "secret".to_string()),
            ],
        }
    }

    #[test]
    fn missing_sandbox_is_created_from_the_template_with_the_shell_agent() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let tag = super::super::template::template_tag("test-image:latest");
        assert_eq!(plan.create_template.as_deref(), Some(tag.as_str()));
        assert_eq!(
            plan.setup[0].args,
            args(&[
                "create",
                "--name",
                "wm-proj-12345678",
                "--no-progress",
                "--template",
                &tag,
                "--pull",
                "never",
                "shell",
                "/code/proj__worktrees/feat",
                "/code/proj/.git",
                "/home/me/.claude",
                "/state/docker-sbx/wm-proj-12345678:ro",
            ])
        );
    }

    #[test]
    fn created_sandbox_links_the_agent_config_dir_into_the_agent_home_as_root() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.setup[1].args,
            args(&[
                "exec",
                "-u",
                "root",
                "wm-proj-12345678",
                "ln",
                "-sfnT",
                "/home/me/.claude",
                "/home/agent/.claude",
            ])
        );
    }

    #[test]
    fn existing_sandbox_is_neither_created_nor_set_up_again() {
        let p = paths();
        let state = SbxState {
            sandbox: Some(SbxSandboxInfo {
                name: "wm-proj-12345678".to_string(),
                status: "stopped".to_string(),
            }),
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(plan.create_template, None);
        assert!(
            plan.setup
                .iter()
                .all(|c| c.args[0] != "create" && !c.args.contains(&"root".to_string())),
            "{:?}",
            plan.setup
        );
    }

    fn rule(scope: &str, resource: &str) -> SbxRule {
        SbxRule {
            scope: scope.to_string(),
            decision: "allow".to_string(),
            resources: vec![resource.to_string()],
        }
    }

    #[test]
    fn localhost_rule_scoped_to_the_sandbox_lets_the_guest_reach_rpc() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.setup.last().unwrap().args,
            args(&[
                "policy",
                "allow",
                "network",
                "--sandbox",
                "wm-proj-12345678",
                "localhost",
            ])
        );
    }

    #[test]
    fn existing_localhost_rule_is_not_added_again() {
        let p = paths();
        let existing = SbxState {
            sandbox: Some(SbxSandboxInfo {
                name: "wm-proj-12345678".to_string(),
                status: "running".to_string(),
            }),
            rules: vec![
                rule("global", "api.anthropic.com"),
                rule("sandbox:wm-proj-12345678", "localhost"),
            ],
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &existing, &exec()).unwrap();
        assert!(plan.setup.is_empty(), "{:?}", plan.setup);

        // A global localhost rule is the user's, not this sandbox's.
        let global_only = SbxState {
            rules: vec![rule("global", "localhost")],
            ..existing
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &global_only, &exec()).unwrap();
        assert_eq!(plan.setup.len(), 1);
        assert_eq!(plan.setup[0].args[0], "policy");
    }

    #[test]
    fn exec_runs_the_command_in_the_pane_cwd_with_env_named_not_valued() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.exec.args,
            args(&[
                "exec",
                "-it",
                "-w",
                "/code/proj__worktrees/feat/sub",
                "-e",
                "WM_RPC_PORT",
                "-e",
                "WM_RPC_TOKEN",
                "-e",
                "PATH=/state/docker-sbx/wm-proj-12345678/shims/bin:/home/agent/.local/bin:\
                 /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "wm-proj-12345678",
                "bash",
                "-c",
                "claude --dangerously-skip-permissions",
            ])
        );
        assert!(!plan.exec.args.iter().any(|a| a.contains("secret")));
        assert_eq!(
            plan.exec.env,
            vec![
                ("WM_RPC_PORT".to_string(), "4242".to_string()),
                ("WM_RPC_TOKEN".to_string(), "secret".to_string()),
            ]
        );
    }

    fn failing(check: &str) -> SbxState {
        let mut state = missing();
        for c in &mut state.checks {
            if c.name == check {
                c.status = "fail".to_string();
            }
        }
        state
    }

    fn plan_error(state: &SbxState, agent: &str) -> String {
        let p = paths();
        launch_plan(&config(), &target(&p), agent, state, &exec())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn logged_out_sbx_fails_naming_sbx_login() {
        let message = plan_error(&failing("Authentication"), "claude");
        assert!(message.contains("sbx login"), "{message}");
    }

    #[test]
    fn uninitialised_global_policy_fails_naming_sbx_policy_init() {
        let message = plan_error(&failing("Global network policy"), "claude");
        assert!(message.contains("sbx policy init"), "{message}");
    }

    #[test]
    fn unsupported_hosts_fail_naming_supported_platforms() {
        for (os, arch, kvm) in [
            ("macos", "x86_64", false),
            ("linux", "x86_64", false),
            ("freebsd", "x86_64", true),
        ] {
            let state = SbxState {
                platform: Platform {
                    os: os.to_string(),
                    arch: arch.to_string(),
                    kvm,
                },
                ..missing()
            };
            let message = plan_error(&state, "claude");
            assert!(message.contains("Apple silicon"), "{os}/{arch}: {message}");
        }

        let linux_with_kvm = SbxState {
            platform: Platform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                kvm: true,
            },
            ..missing()
        };
        let p = paths();
        assert!(launch_plan(&config(), &target(&p), "claude", &linux_with_kvm, &exec()).is_ok());
    }

    #[test]
    fn agents_without_sbx_support_fail_closed() {
        let message = plan_error(&missing(), "codex");
        assert!(message.contains("codex"), "{message}");
        assert!(message.contains("claude"), "{message}");
    }

    #[test]
    fn main_worktree_mounts_no_separate_git_dir() {
        let p = paths();
        let target = LaunchTarget {
            git_common_dir: None,
            ..target(&p)
        };
        let plan = launch_plan(&config(), &target, "claude", &missing(), &exec()).unwrap();
        assert_eq!(
            plan.setup[0].args[9..],
            args(&[
                "/code/proj__worktrees/feat",
                "/home/me/.claude",
                "/state/docker-sbx/wm-proj-12345678:ro",
            ])
        );
    }
}
