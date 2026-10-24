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

/// The baked devshell's wrapper in an OpenShell image: it loads the
/// environment the workstation recorded and execs its arguments.
pub const BAKED_DEVSHELL: &str = "/usr/local/bin/workmux-devshell";

/// Where a remote's devshell for a nix toolchain comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Devshell {
    /// nix is (or gets) installed, and agents start in `nix develop`.
    Nix,
    /// The image carries the project's devshell closure, built on the
    /// workstation;
    /// nothing is installed or built on the remote.
    Baked,
    /// An OpenShell sandbox without a baked devshell: nix can neither be
    /// installed nor build there, so the toolchain comes from the image.
    Unavailable,
}

/// Where this machine, the remote, gets its devshell.
pub fn devshell_here() -> Devshell {
    if Path::new(BAKED_DEVSHELL).is_file() {
        Devshell::Baked
    } else if std::env::var_os("OPENSHELL_SANDBOX").is_some_and(|value| value == "1") {
        Devshell::Unavailable
    } else {
        Devshell::Nix
    }
}

/// `command` wrapped to start in the project's devshell.
pub fn wrap(command: &str, toolchain: &DetectedToolchain, devshell: Devshell) -> String {
    match (devshell, toolchain) {
        (Devshell::Nix, _) => crate::sandbox::toolchain::wrap_command(command, toolchain),
        (Devshell::Baked, DetectedToolchain::Flake) => format!(
            "{BAKED_DEVSHELL} bash -c '{}'",
            crate::shell::shell_escape(command)
        ),
        _ => command.to_string(),
    }
}

/// Provisioning for a project whose toolchain is `toolchain`, on a remote
/// where `has` tells whether a command is installed.
pub fn plan(toolchain: &DetectedToolchain, has: impl Fn(&str) -> bool) -> Plan {
    plan_for(toolchain, devshell_here(), has)
}

fn plan_for(toolchain: &DetectedToolchain, devshell: Devshell, has: impl Fn(&str) -> bool) -> Plan {
    let mut plan = Plan::default();
    if let Some(warning) = holder_warning(&has) {
        plan.warnings.push(warning);
    }
    let needs_nix = devshell == Devshell::Nix && *toolchain != DetectedToolchain::None;
    if needs_nix && !has("nix") {
        plan.steps.push(shell("installing nix", NIX_INSTALL));
    }
    if needs_nix && *toolchain == DetectedToolchain::Devbox && !has("devbox") {
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
    let mut warnings = warnings(has_command, &crate::agent_setup::check_all());
    let source = Path::new(crate::remote::openshell::image::DEVSHELL_SOURCE);
    warnings.extend(baked_devshell_warning(source));
    warnings
}

/// A warning when the devshell baked into this sandbox was built from
/// another `flake.lock` than the project's clone has now.
fn baked_devshell_warning(source: &Path) -> Option<String> {
    use crate::remote::openshell::image::{DevshellSource, flake_lock_digest};
    let source: DevshellSource = serde_json::from_slice(&std::fs::read(source).ok()?).ok()?;
    let clone = Path::new(&source.clone);
    (clone.is_dir() && flake_lock_digest(clone) != source.flake_lock_sha256).then(|| {
        format!(
            "the devshell baked into this sandbox was built from another flake.lock than {}'s; \
             run `workmux remote recreate` for the project to rebuild it",
            source.clone
        )
    })
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
    fn a_baked_devshell_or_an_openshell_sandbox_never_gets_nix_installed() {
        for devshell in [Devshell::Baked, Devshell::Unavailable] {
            for toolchain in [DetectedToolchain::Flake, DetectedToolchain::Devbox] {
                let plan = plan_for(&toolchain, devshell, has(&["dtach"]));

                assert_eq!(steps(&plan), ["installing agent hooks"]);
            }
        }
    }

    #[test]
    fn agents_start_in_the_baked_devshell_instead_of_nix_develop() {
        assert_eq!(
            wrap("claude --help", &DetectedToolchain::Flake, Devshell::Baked),
            "/usr/local/bin/workmux-devshell bash -c 'claude --help'"
        );
        assert_eq!(
            wrap("claude", &DetectedToolchain::Flake, Devshell::Nix),
            "nix develop --command bash -c 'claude'"
        );
        assert_eq!(
            wrap("claude", &DetectedToolchain::Flake, Devshell::Unavailable),
            "claude"
        );
        assert_eq!(
            wrap("claude", &DetectedToolchain::None, Devshell::Baked),
            "claude"
        );
    }

    #[test]
    fn a_baked_devshell_older_than_the_clones_flake_lock_is_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("api");
        std::fs::create_dir(&clone).unwrap();
        std::fs::write(clone.join("flake.lock"), "v1").unwrap();
        let source = dir.path().join("devshell-source.json");
        std::fs::write(
            &source,
            format!(
                r#"{{"clone":"{}","flake_lock_sha256":"{}"}}"#,
                clone.display(),
                crate::remote::openshell::image::sha256_hex(b"v1")
            ),
        )
        .unwrap();
        assert_eq!(baked_devshell_warning(&source), None);

        std::fs::write(clone.join("flake.lock"), "v2").unwrap();

        let warning = baked_devshell_warning(&source).expect("a warning");
        assert!(warning.contains("workmux remote recreate"), "{warning}");
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
