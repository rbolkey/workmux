//! Provisioning a remote for a project: what remote workmux does once it
//! has cloned the project there. It installs nix when the project's
//! toolchain needs it, as the Lima backend does, and installs agent hooks
//! with a non-interactive `workmux setup`, so agent status reaches the
//! sidebar.

use std::path::Path;

use crate::agent_setup::{AgentCheck, StatusCheck};
use crate::sandbox::toolchain::DetectedToolchain;

/// Where the nix installer puts nix; a non-interactive shell may not have
/// it on PATH.
const NIX_PROFILE_BIN: &str = "/nix/var/nix/profiles/default/bin";

/// Installs nix with the Determinate Systems installer, which asks for
/// root itself.
const NIX_INSTALL: &str = "curl --proto '=https' --tlsv1.2 -sSf -L \
     https://install.determinate.systems/nix | sh -s -- install --no-confirm";

/// Installs devbox, which runs on top of nix.
const DEVBOX_INSTALL: &str = "curl -fsSL https://get.jetify.com/devbox | bash -s -- -f";

/// One provisioning step: a command to run on the remote. `workmux` means
/// remote workmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What the step does, as the user is told it, e.g. `installing nix`.
    pub what: &'static str,
    pub argv: Vec<String>,
}

/// What provisioning does for one project.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Problems provisioning cannot fix, shown as warnings.
    pub warnings: Vec<String>,
    pub steps: Vec<Step>,
}

fn shell(what: &'static str, script: &str) -> Step {
    Step {
        what,
        argv: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
    }
}

/// Provisioning for a project whose toolchain is `toolchain`, on a remote
/// where `has` tells whether a command is installed.
pub fn plan(toolchain: &DetectedToolchain, has: impl Fn(&str) -> bool) -> Plan {
    let mut plan = Plan::default();
    if let Some(warning) = holder_warning(&has) {
        plan.warnings.push(warning);
    }
    let needs_nix = *toolchain != DetectedToolchain::None;
    if needs_nix && !has("nix") {
        plan.steps.push(shell("installing nix", NIX_INSTALL));
    }
    if *toolchain == DetectedToolchain::Devbox && !has("devbox") {
        plan.steps.push(shell("installing devbox", DEVBOX_INSTALL));
    }
    plan.steps.push(Step {
        what: "installing agent hooks",
        argv: ["workmux", "setup", "--hooks", "--yes"]
            .map(str::to_string)
            .to_vec(),
    });
    plan
}

fn holder_warning(has: &impl Fn(&str) -> bool) -> Option<String> {
    (!has("dtach")).then(|| {
        "the session holder dtach is not installed on this remote; remote panes need it".to_string()
    })
}

/// Whether `name` is installed on this machine, the remote.
pub fn has_command(name: &str) -> bool {
    which::which(name).is_ok() || Path::new(NIX_PROFILE_BIN).join(name).is_file()
}

/// Problems `workmux remote status` warns about: no session holder, and
/// agents whose status would never reach the sidebar.
pub fn warnings(has: impl Fn(&str) -> bool, agents: &[AgentCheck]) -> Vec<String> {
    let mut warnings: Vec<String> = holder_warning(&has).into_iter().collect();
    if agents.is_empty() {
        warnings.push(
            "no agent with status tracking was found on this remote; once one is \
             installed, run `workmux setup --hooks` there"
                .to_string(),
        );
    }
    for check in agents {
        match &check.status {
            StatusCheck::Installed | StatusCheck::UpdateAvailable => {}
            StatusCheck::NotInstalled => warnings.push(format!(
                "{} has no status hooks on this remote, so its agents show no status; \
                 run `workmux setup --hooks` there",
                check.agent.name()
            )),
            StatusCheck::Error(error) => warnings.push(format!(
                "{}'s status hooks on this remote could not be checked: {error}",
                check.agent.name()
            )),
        }
    }
    warnings
}

/// [`warnings`] for this machine, the remote.
pub fn current_warnings() -> Vec<String> {
    warnings(has_command, &crate::agent_setup::check_all())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_setup::Agent;

    fn has(installed: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| installed.contains(&name)
    }

    fn steps(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|step| step.what).collect()
    }

    const SETUP: &[&str] = &["workmux", "setup", "--hooks", "--yes"];

    #[test]
    fn a_project_without_a_toolchain_only_gets_agent_hooks() {
        let plan = plan(&DetectedToolchain::None, has(&["dtach"]));

        assert_eq!(steps(&plan), ["installing agent hooks"]);
        assert_eq!(plan.steps[0].argv, SETUP);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn a_flake_project_installs_nix_when_it_is_missing() {
        let plan = plan(&DetectedToolchain::Flake, has(&["dtach"]));

        assert_eq!(steps(&plan), ["installing nix", "installing agent hooks"]);
        let script = &plan.steps[0].argv;
        assert_eq!(script[..2], ["sh", "-c"]);
        assert!(script[2].contains("https://install.determinate.systems/nix"));
        assert!(script[2].contains("--no-confirm"));
    }

    #[test]
    fn nix_already_on_the_remote_is_not_reinstalled() {
        let plan = plan(&DetectedToolchain::Flake, has(&["dtach", "nix"]));

        assert_eq!(steps(&plan), ["installing agent hooks"]);
    }

    #[test]
    fn a_devbox_project_installs_nix_and_devbox_when_missing() {
        assert_eq!(
            steps(&plan(&DetectedToolchain::Devbox, has(&["dtach"]))),
            [
                "installing nix",
                "installing devbox",
                "installing agent hooks"
            ]
        );
        assert_eq!(
            steps(&plan(&DetectedToolchain::Devbox, has(&["dtach", "nix"]))),
            ["installing devbox", "installing agent hooks"]
        );
    }

    #[test]
    fn a_remote_without_dtach_is_warned_about() {
        let plan = plan(&DetectedToolchain::None, has(&[]));

        assert_eq!(
            plan.warnings,
            ["the session holder dtach is not installed on this remote; remote panes need it"]
        );
    }

    fn check(agent: Agent, status: StatusCheck) -> AgentCheck {
        AgentCheck {
            agent,
            reason: "test",
            status,
        }
    }

    #[test]
    fn status_warns_about_agents_without_hooks() {
        let agents = [
            check(Agent::Claude, StatusCheck::NotInstalled),
            check(Agent::Codex, StatusCheck::Installed),
            check(Agent::Gemini, StatusCheck::UpdateAvailable),
            check(Agent::Pi, StatusCheck::Error("bad JSON".to_string())),
        ];

        assert_eq!(
            warnings(has(&["dtach"]), &agents),
            [
                "Claude Code has no status hooks on this remote, so its agents show no \
                 status; run `workmux setup --hooks` there",
                "pi's status hooks on this remote could not be checked: bad JSON",
            ]
        );
    }

    #[test]
    fn status_warns_without_dtach_or_any_agent() {
        assert_eq!(
            warnings(has(&[]), &[]),
            [
                "the session holder dtach is not installed on this remote; remote panes \
                 need it",
                "no agent with status tracking was found on this remote; once one is \
                 installed, run `workmux setup --hooks` there",
            ]
        );
    }

    #[test]
    fn a_remote_with_dtach_and_hooked_agents_has_no_warnings() {
        let agents = [check(Agent::Claude, StatusCheck::Installed)];

        assert!(warnings(has(&["dtach"]), &agents).is_empty());
    }
}
