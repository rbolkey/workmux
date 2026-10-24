//! The agents workmux can install in an OpenShell sandbox's image, each
//! where OpenShell's example provider profile for it expects the binary:
//! a profile lets only the binaries it names reach its endpoints, and its
//! credentials reach the agent only through a provider of that type.

use std::collections::BTreeMap;

/// An agent workmux installs in a sandbox image.
#[derive(Debug, PartialEq, Eq)]
pub struct SandboxAgent {
    /// The agent's plain CLI, as remote workmux runs it.
    pub name: &'static str,
    /// Where the image has it.
    pub path: &'static str,
    /// The OpenShell provider type whose credentials it uses.
    pub provider_type: &'static str,
    /// The commands, run as root in an Ubuntu stage with curl, that put
    /// the agent at `path`.
    install: &'static str,
}

/// Every agent workmux can install. Each is installed in a stage of its
/// own and copied into the final one, so a project image needs no curl.
static AGENTS: &[SandboxAgent] = &[
    SandboxAgent {
        name: "claude",
        path: "/usr/local/bin/claude",
        provider_type: "claude-code",
        // The installer leaves a symlink into ~/.local/share; copy the
        // binary it points at.
        install: "curl -fsSL https://claude.ai/install.sh | bash \\\n    \
                  && cp -L /root/.local/bin/claude /usr/local/bin/claude",
    },
    SandboxAgent {
        name: "codex",
        path: "/usr/local/bin/codex",
        provider_type: "codex",
        // The static musl release, so it runs on any base image.
        install: "curl -fsSL https://github.com/openai/codex/releases/latest/download/codex-$(uname -m)-unknown-linux-musl.tar.gz \\\n    \
                  | tar -xz -C /tmp \\\n    \
                  && install -m 0755 /tmp/codex-$(uname -m)-unknown-linux-musl /usr/local/bin/codex",
    },
];

impl SandboxAgent {
    /// The agent whose plain CLI is `name`, if workmux can install it.
    pub fn named(name: &str) -> Option<&'static SandboxAgent> {
        AGENTS.iter().find(|agent| agent.name == name)
    }

    /// The image build stage that installs the agent, and its name.
    pub fn stage(&self, base: &str) -> (String, String) {
        let stage = format!("agent-{}", self.name);
        (
            format!(
                "FROM {base} AS {stage}\n\
                 RUN apt-get update \\\n    \
                 && apt-get install -y --no-install-recommends ca-certificates curl \\\n    \
                 && {}\n",
                self.install
            ),
            stage,
        )
    }
}

/// What the user is told about a sandbox's agents.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

/// The report for a sandbox running `agents` with the providers
/// `attached`, whose types are `types` (provider name to type), or why
/// they could not be looked up.
pub fn report(
    agents: &[String],
    attached: &[String],
    types: Result<&BTreeMap<String, String>, &str>,
) -> Report {
    let mut report = Report::default();
    if agents.is_empty() {
        report.warnings.push(
            "the default agent is none workmux can install, so the sandbox has no agent; \
             name the sandbox's agents under the project's `openshell: agents:`"
                .to_string(),
        );
    }
    for name in agents {
        let Some(agent) = SandboxAgent::named(name) else {
            report.warnings.push(format!(
                "workmux cannot install {name} in the sandbox image; the project's image (--image) must have it"
            ));
            continue;
        };
        let wanted = agent.provider_type;
        let giving: Result<Vec<&str>, &str> = if attached.is_empty() {
            Ok(Vec::new())
        } else {
            types.map(|types| {
                attached
                    .iter()
                    .filter(|provider| types.get(*provider).is_some_and(|t| t == wanted))
                    .map(String::as_str)
                    .collect()
            })
        };
        match giving {
            Ok(giving) if giving.is_empty() => {
                report.notes.push(format!(
                    "{name}: installed at {}; its credentials come from a provider of type {wanted}",
                    agent.path
                ));
                report.warnings.push(format!(
                    "no provider of type {wanted} is attached to the sandbox, so {name} has no credentials there; \
                     create one with `openshell provider create --type {wanted}` and list it in the project's `openshell: providers:`"
                ));
            }
            Ok(giving) => report.notes.push(format!(
                "{name}: installed at {}; its credentials come from a provider of type {wanted} (attached: {})",
                agent.path,
                giving.join(", ")
            )),
            Err(error) => {
                report.notes.push(format!(
                    "{name}: installed at {}; its credentials come from a provider of type {wanted}",
                    agent.path
                ));
                report.warnings.push(format!(
                    "could not check which providers give {name} its credentials ({error}); \
                     it needs one of type {wanted}"
                ));
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn each_agent_is_told_the_provider_type_it_needs() {
        let types = BTreeMap::from([
            ("gh".to_string(), "github".to_string()),
            ("anthropic".to_string(), "claude-code".to_string()),
        ]);

        let report = report(
            &names(&["claude"]),
            &names(&["gh", "anthropic"]),
            Ok(&types),
        );

        assert_eq!(
            report.notes,
            [
                "claude: installed at /usr/local/bin/claude; its credentials come from a provider of type claude-code (attached: anthropic)"
            ]
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn an_agent_without_an_attached_provider_of_its_type_is_warned_about() {
        let types = BTreeMap::from([("gh".to_string(), "github".to_string())]);

        let report = report(&names(&["codex"]), &names(&["gh"]), Ok(&types));

        assert_eq!(
            report.warnings,
            [
                "no provider of type codex is attached to the sandbox, so codex has no credentials there; \
              create one with `openshell provider create --type codex` and list it in the project's `openshell: providers:`"
            ]
        );
        // With no providers at all, there is nothing to look up.
        let report = super::report(&names(&["claude"]), &[], Err("unreachable"));
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].starts_with("no provider of type claude-code"));
    }

    #[test]
    fn unknown_provider_types_and_agents_workmux_cannot_install_are_said() {
        let report = report(
            &names(&["claude", "gemini"]),
            &names(&["anthropic"]),
            Err("gateway down"),
        );

        assert_eq!(
            report.warnings,
            [
                "could not check which providers give claude its credentials (gateway down); \
                 it needs one of type claude-code",
                "workmux cannot install gemini in the sandbox image; the project's image (--image) must have it",
            ]
        );
        let report = super::report(&[], &[], Err("unused"));
        assert_eq!(
            report.warnings,
            [
                "the default agent is none workmux can install, so the sandbox has no agent; \
              name the sandbox's agents under the project's `openshell: agents:`"
            ]
        );
    }
}
