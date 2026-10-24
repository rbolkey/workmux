//! The agents workmux can install in an OpenShell sandbox's image, each
//! where OpenShell's example provider profile for it expects the binary:
//! a profile lets only the binaries it names reach its endpoints, so an
//! agent's credentials come from a provider whose profile allows its binary.

use std::collections::BTreeMap;

/// An agent workmux installs in a sandbox image.
#[derive(Debug, PartialEq, Eq)]
pub struct SandboxAgent {
    /// The agent's plain CLI, as remote workmux runs it.
    pub name: &'static str,
    /// Where the image has it.
    pub path: &'static str,
    /// The stock OpenShell provider type whose profile allows it.
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

/// A provider on the gateway, as the agent check sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// Its type: the id of the profile it was created from.
    pub provider_type: String,
    /// The binaries its profile lets reach the profile's endpoints, or
    /// `None` when the profile could not be found.
    pub binaries: Option<Vec<String>>,
}

impl Provider {
    /// Whether the provider gives `agent` its credentials: its profile
    /// allows the agent's binary, or, with no profile to go by, it is of
    /// the agent's stock type.
    fn gives(&self, agent: &SandboxAgent) -> bool {
        match &self.binaries {
            Some(binaries) => binaries.iter().any(|binary| binary == agent.path),
            None => self.provider_type == agent.provider_type,
        }
    }
}

/// The report for a sandbox running `agents` with the providers
/// `attached`, given every provider on the gateway by name, or why they
/// could not be looked up.
pub fn report(
    agents: &[String],
    attached: &[String],
    providers: Result<&BTreeMap<String, Provider>, &str>,
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
        let path = agent.path;
        let giving: Result<Vec<&str>, &str> = if attached.is_empty() {
            Ok(Vec::new())
        } else {
            providers.map(|providers| {
                attached
                    .iter()
                    .filter(|name| providers.get(*name).is_some_and(|p| p.gives(agent)))
                    .map(String::as_str)
                    .collect()
            })
        };
        let note = format!(
            "{name}: installed at {path}; its credentials come from a provider whose profile allows it, \
             such as one of type {wanted}"
        );
        match giving {
            Ok(giving) if giving.is_empty() => {
                report.notes.push(note);
                report.warnings.push(format!(
                    "no provider attached to the sandbox has a profile that allows {path}, so {name} has no credentials there; \
                     create one with `openshell provider create --type {wanted}` and list it in the project's `openshell: providers:`"
                ));
            }
            Ok(giving) => report
                .notes
                .push(format!("{note} (attached: {})", giving.join(", "))),
            Err(error) => {
                report.notes.push(note);
                report.warnings.push(format!(
                    "could not check which providers give {name} its credentials ({error}); \
                     it needs one whose profile allows {path}, such as one of type {wanted}"
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

    fn provider(provider_type: &str, binaries: Option<&[&str]>) -> Provider {
        Provider {
            provider_type: provider_type.to_string(),
            binaries: binaries.map(names),
        }
    }

    #[test]
    fn a_stock_provider_gives_its_agent_credentials_with_or_without_its_profile() {
        let stock = BTreeMap::from([
            ("gh".to_string(), provider("github", None)),
            (
                "anthropic".to_string(),
                provider("claude-code", Some(&["/usr/local/bin/claude"])),
            ),
            ("openai".to_string(), provider("codex", None)),
        ]);

        let report = report(
            &names(&["claude", "codex"]),
            &names(&["gh", "anthropic", "openai"]),
            Ok(&stock),
        );

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
    fn a_provider_whose_profile_allows_the_agents_binary_gives_it_credentials_whatever_its_type() {
        let providers = BTreeMap::from([
            (
                "gh".to_string(),
                provider("github-dev", Some(&["/usr/bin/gh"])),
            ),
            (
                "claude".to_string(),
                provider(
                    "claude-code-oauth",
                    Some(&["/usr/local/bin/claude", "/usr/bin/claude"]),
                ),
            ),
        ]);

        let report = report(
            &names(&["claude", "codex"]),
            &names(&["gh", "claude"]),
            Ok(&providers),
        );

        assert_eq!(
            report.notes,
            [
                "claude: installed at /usr/local/bin/claude; its credentials come from a provider whose profile allows it, such as one of type claude-code (attached: claude)",
                "codex: installed at /usr/local/bin/codex; its credentials come from a provider whose profile allows it, such as one of type codex",
            ]
        );
        assert_eq!(
            report.warnings,
            [
                "no provider attached to the sandbox has a profile that allows /usr/local/bin/codex, so codex has no credentials there; \
                 create one with `openshell provider create --type codex` and list it in the project's `openshell: providers:`"
            ]
        );
    }

    #[test]
    fn with_no_providers_attached_there_is_nothing_to_look_up() {
        let report = report(&names(&["claude"]), &[], Err("unreachable"));

        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].starts_with("no provider attached to the sandbox"),
            "{:?}",
            report.warnings
        );
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
                 it needs one whose profile allows /usr/local/bin/claude, such as one of type claude-code",
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
