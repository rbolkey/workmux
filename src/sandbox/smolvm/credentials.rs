//! Variables passed into a smol machine, as placeholders where workmux
//! knows their hosts.
//!
//! smolvm swaps a placeholder for the real value in a host-side proxy, on
//! HTTPS requests to a credential's hosts, so the value never enters the
//! guest. workmux never reads a value either: each credential is a secret
//! ref (`--secret-env` or `--secret-keychain`) that smolvm
//! resolves itself, plus a `--credential` binding. A variable with no known
//! hosts passes its real value, by name, from smolvm's environment.
//!
//! Measured on smolvm 1.36.1:
//! - a ref feeds a binding only when named after the binding's ENV_VAR;
//!   named after anything else, the real value is injected into the guest;
//! - `machine create` refuses a credential host the allowlist doesn't open;
//! - a credential doesn't open its host, nor bypass `--block-private`.

use anyhow::{Result, bail};
use std::collections::BTreeMap;

use std::fmt;

use crate::config::{CredentialEntry, CredentialMode, NetworkPolicy, SandboxConfig};
use crate::sandbox::network_proxy::domain_matches;

/// A keychain item: a macOS Keychain generic password or a Linux Secret
/// Service item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainItem {
    pub service: String,
    pub account: Option<String>,
}

impl KeychainItem {
    /// Parse `SERVICE[/ACCOUNT]`. The service is everything before the first
    /// `/`; neither part may be empty.
    pub fn parse(spec: &str) -> std::result::Result<Self, String> {
        let (service, account) = match spec.split_once('/') {
            Some((service, account)) => (service, Some(account)),
            None => (spec, None),
        };
        if service.is_empty() {
            return Err(format!(
                "from_keychain needs a service, as SERVICE or SERVICE/ACCOUNT: '{spec}'"
            ));
        }
        if account.is_some_and(str::is_empty) {
            return Err(format!(
                "from_keychain account after '/' must not be empty: '{spec}'"
            ));
        }
        Ok(Self {
            service: service.to_string(),
            account: account.map(str::to_string),
        })
    }
}

impl fmt::Display for KeychainItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.account {
            Some(account) => write!(f, "{}/{}", self.service, account),
            None => f.write_str(&self.service),
        }
    }
}

/// Where smolvm reads a credential's value instead of its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSource {
    Keychain(KeychainItem),
}

impl EnvSource {
    pub fn keychain(spec: &str) -> std::result::Result<Self, String> {
        KeychainItem::parse(spec).map(Self::Keychain)
    }
}

/// A value workmux holds. Its `Debug` output is redacted so it can't reach
/// a log by accident; read it with [`SecretValue::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// A variable whose hosts workmux knows, so it becomes a placeholder with
/// no config.
pub struct BuiltinCredential {
    pub var: &'static str,
    pub hosts: &'static [&'static str],
}

const GITHUB_API: &[&str] = &["api.github.com", "uploads.github.com"];
const ANTHROPIC_API: &[&str] = &["api.anthropic.com"];

/// `CLAUDE_CODE_OAUTH_TOKEN` and the GitHub rows are measured against
/// smolvm; the API keys follow their providers' documented API hosts.
pub const BUILTIN: &[BuiltinCredential] = &[
    BuiltinCredential {
        var: "GITHUB_TOKEN",
        hosts: GITHUB_API,
    },
    BuiltinCredential {
        var: "GH_TOKEN",
        hosts: GITHUB_API,
    },
    BuiltinCredential {
        var: "ANTHROPIC_API_KEY",
        hosts: ANTHROPIC_API,
    },
    BuiltinCredential {
        var: "CLAUDE_CODE_OAUTH_TOKEN",
        hosts: ANTHROPIC_API,
    },
    BuiltinCredential {
        var: "OPENAI_API_KEY",
        hosts: &["api.openai.com"],
    },
    BuiltinCredential {
        var: "GEMINI_API_KEY",
        hosts: &["generativelanguage.googleapis.com"],
    },
];

/// The variable holding git's `Authorization` value for `github.com`. The
/// guest sees only its placeholder; smolvm sets the header on every request
/// to `github.com` (measured on smolvm 1.36.1), so git needs no config.
pub const GIT_AUTH_VAR: &str = "WM_GITHUB_GIT_AUTH";
const GIT_HOST: &str = "github.com";
/// The variables git's header is derived from, in `gh`'s order.
const GIT_TOKEN_VARS: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN"];

/// What a machine gets for its variables.
#[derive(Debug, Default)]
pub struct MachineCredentials {
    /// Secret refs and `--credential` bindings for `machine create`.
    pub create_flags: Vec<String>,
    /// Variables the exec passes by name (`-e NAME`) from smolvm's
    /// environment, with their real values.
    pub exec_names: Vec<String>,
    /// Values set on `machine start`'s environment only: git's derived
    /// `github.com` header, which smolvm resolves at start.
    pub start_env: Vec<(String, SecretValue)>,
    /// Notes for `tracing`.
    pub notes: Vec<String>,
    /// Whether `create_flags` hold a `--credential` binding.
    bound: bool,
}

impl MachineCredentials {
    /// Whether the machine has a credential binding, so smolvm intercepts
    /// its hosts and puts its CA in the guest's bundle.
    pub fn has_binding(&self) -> bool {
        self.bound
    }

    /// Add a `--credential` binding, `spec` as smolvm takes it.
    fn bind(&mut self, spec: String) {
        self.create_flags.extend(["--credential".to_string(), spec]);
        self.bound = true;
    }
}

/// A variable to pass, and where smolvm reads its value: the same-named
/// host variable when `source` is `None`.
struct Candidate {
    name: String,
    source: Option<EnvSource>,
}

/// The variables to consider: every `env_passthrough` variable, then
/// every `credentials` entry with its own source.
fn candidates(sandbox: &SandboxConfig) -> Vec<Candidate> {
    let credentials = sandbox.smolvm.credentials();
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |name: &str, source: Option<EnvSource>| {
        if !out.iter().any(|known| known.name == name) {
            out.push(Candidate {
                name: name.to_string(),
                source,
            });
        }
    };
    for name in sandbox.env_passthrough() {
        push(
            name,
            credentials.get(name).and_then(|entry| entry.source.clone()),
        );
    }
    for (name, entry) in &credentials {
        if let Some(source) = &entry.source {
            push(name, Some(source.clone()));
        }
    }
    out
}

/// The host variable smolvm reads a candidate's value from: its own name,
/// unless it has a source.
fn host_var(candidate: &Candidate) -> Option<&str> {
    candidate
        .source
        .is_none()
        .then_some(candidate.name.as_str())
}

/// The host variables the candidates read that the supervisor's
/// environment sets, by name. workmux checks only that each is set, never
/// its value.
pub fn host_env(sandbox: &SandboxConfig) -> Vec<String> {
    candidates(sandbox)
        .iter()
        .filter_map(host_var)
        .filter(|name| std::env::var_os(name).is_some())
        .map(str::to_string)
        .collect()
}

/// The values git's `github.com` header may be derived from: `GH_TOKEN` and
/// `GITHUB_TOKEN`, when passed, read through `lookup` from the same-named
/// host variable. Keychain sources aren't read. These are the only
/// values workmux holds.
pub fn github_tokens(
    sandbox: &SandboxConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, SecretValue)> {
    candidates(sandbox)
        .iter()
        .filter(|candidate| GIT_TOKEN_VARS.contains(&candidate.name.as_str()))
        .filter_map(|candidate| {
            let value = lookup(host_var(candidate)?)?;
            Some((candidate.name.clone(), SecretValue::new(value)))
        })
        .collect()
}

/// Check that each `credentials` entry has hosts to bind. smolvm checks
/// the hosts themselves at create: their form, and that the allowlist
/// opens them.
pub fn validate(sandbox: &SandboxConfig) -> Result<()> {
    for (name, entry) in sandbox.smolvm.credentials() {
        if entry.hosts.as_ref().is_some_and(Vec::is_empty) {
            bail!(
                "sandbox.smolvm.credentials.{name}: hosts is empty; list at least one host, \
                 or remove hosts"
            );
        }
        if entry.hosts.is_none() && builtin_hosts(&name).is_none() {
            bail!(
                "sandbox.smolvm.credentials.{name}: workmux knows no hosts for {name}; \
                 list its hosts, or remove the entry"
            );
        }
    }
    Ok(())
}

fn builtin_hosts(name: &str) -> Option<Vec<String>> {
    BUILTIN
        .iter()
        .find(|builtin| builtin.var == name)
        .map(|builtin| builtin.hosts.iter().map(|h| h.to_string()).collect())
}

/// The flags and names for a machine's variables. `host_env` names the
/// host variables the supervisor's environment sets ([`host_env`]);
/// `github_tokens` are the values git's header may be derived from
/// ([`github_tokens`]).
pub fn machine_credentials(
    sandbox: &SandboxConfig,
    host_env: &[String],
    github_tokens: &[(String, SecretValue)],
) -> Result<MachineCredentials> {
    validate(sandbox)?;
    let credentials: BTreeMap<String, CredentialEntry> = sandbox.smolvm.credentials();
    let deny = sandbox.network.policy() == NetworkPolicy::Deny;
    let rules = sandbox.network.allowed_domain_rules();
    let allowed = |host: &str| rules.iter().any(|rule| domain_matches(host, &rule.host));
    let mut out = MachineCredentials::default();
    // Built-in GitHub tokens passed as placeholders, for git's header.
    let mut git_tokens: Vec<String> = Vec::new();

    for (name, entry) in &credentials {
        if entry.source.is_none() && !sandbox.env_passthrough().contains(&name.as_str()) {
            out.notes.push(format!(
                "{name} isn't passed into the machine: nothing passes it into the sandbox. \
                 Add it to sandbox.env_passthrough, or give it from_keychain"
            ));
        }
    }

    for candidate in candidates(sandbox) {
        let name = candidate.name.clone();
        let source = candidate.source.clone();
        if let Some(var) = host_var(&candidate)
            && !host_env.iter().any(|set| set == var)
        {
            continue;
        }
        let entry = credentials.get(&name);
        let listed_hosts = entry.and_then(|entry| entry.hosts.clone());
        let mut hosts = listed_hosts.clone().or_else(|| builtin_hosts(&name));
        if hosts.is_some() && listed_hosts.is_none() && GIT_TOKEN_VARS.contains(&name.as_str()) {
            git_tokens.push(name.clone());
        }
        if deny
            && listed_hosts.is_none()
            && let Some(builtin) = hosts.take()
        {
            let narrowed: Vec<String> = builtin.into_iter().filter(|host| allowed(host)).collect();
            if narrowed.is_empty() {
                out.notes.push(format!(
                    "{name} isn't passed into the machine: none of its hosts is in \
                     sandbox.network.allowed_domains"
                ));
                continue;
            }
            hosts = Some(narrowed);
        }

        match hosts {
            Some(hosts) => {
                out.create_flags.extend(secret_ref(&name, source.as_ref()));
                let mode = match entry.and_then(|entry| entry.mode.as_ref()) {
                    None | Some(CredentialMode::Header) => String::new(),
                    Some(CredentialMode::SetHeader(header)) => format!(":set-header={header}"),
                };
                out.bind(format!("{name}={name}@{}{mode}", hosts.join(",")));
            }
            None => {
                if source.is_some() {
                    // A ref with no binding: smolvm would inject its real
                    // value into the guest. `validate` refuses this first.
                    bail!(
                        "sandbox.smolvm.credentials.{name}: no hosts to bind its keychain \
                         value to"
                    );
                }
                out.exec_names.push(name.clone());
            }
        }
    }

    if !git_tokens.is_empty() {
        git_credential(
            &mut out,
            &git_tokens,
            github_tokens,
            deny && !allowed(GIT_HOST),
        );
    }
    Ok(out)
}

/// git over HTTPS to `github.com`: a `set-header` binding whose value,
/// `Basic base64(x-access-token:<token>)`, workmux derives from the first
/// of `tokens` it holds a value for. git sends the token inside base64, so
/// a placeholder can't be swapped there.
fn git_credential(
    out: &mut MachineCredentials,
    tokens: &[String],
    values: &[(String, SecretValue)],
    denied: bool,
) {
    use base64::Engine;
    if denied {
        out.notes.push(format!(
            "git over HTTPS gets no credential: {GIT_HOST} isn't in \
             sandbox.network.allowed_domains"
        ));
        return;
    }
    let token = GIT_TOKEN_VARS
        .iter()
        .filter(|var| tokens.iter().any(|token| token == *var))
        .find_map(|var| values.iter().find(|(name, _)| name == var));
    let Some((name, token)) = token else {
        out.notes.push(format!(
            "git over HTTPS to {GIT_HOST} gets no credential: workmux derives git's header \
             only from env_passthrough, not from {}'s keychain",
            tokens.join(" or ")
        ));
        return;
    };
    if token.expose().contains(['\n', '\r']) {
        out.notes.push(format!(
            "git over HTTPS to {GIT_HOST} gets no credential: {name} contains a line break"
        ));
        return;
    }
    // Unwrapped: a line break leaves the credential unavailable (502).
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("x-access-token:{}", token.expose()));
    out.create_flags.extend([
        "--secret-env".to_string(),
        format!("{GIT_AUTH_VAR}={GIT_AUTH_VAR}"),
    ]);
    out.bind(format!(
        "{GIT_AUTH_VAR}={GIT_AUTH_VAR}@{GIT_HOST}:set-header=Authorization"
    ));
    out.start_env.push((
        GIT_AUTH_VAR.to_string(),
        SecretValue::new(format!("Basic {encoded}")),
    ));
}

/// The secret ref for `name`, named after it: smolvm feeds a binding only
/// from a ref named after its ENV_VAR, and injects any other ref's real
/// value into the guest.
fn secret_ref(name: &str, source: Option<&EnvSource>) -> [String; 2] {
    match source {
        None => ["--secret-env".into(), format!("{name}={name}")],
        Some(EnvSource::Keychain(item)) => ["--secret-keychain".into(), format!("{name}={item}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(yaml: &str) -> SandboxConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_an_entry_without_hosts_for_a_variable_with_none_built_in_is_an_error() {
        for entry in ["{ mode: header }", "{ from_keychain: linear }"] {
            let listed = sandbox(&format!(
                "env_passthrough: [LINEAR_API_KEY]\nsmolvm:\n  credentials:\n    LINEAR_API_KEY: {entry}\n"
            ));
            let error = machine_credentials(&listed, &["LINEAR_API_KEY".into()], &[])
                .unwrap_err()
                .to_string();
            assert!(error.contains("LINEAR_API_KEY"), "{error}");
            assert!(error.contains("hosts"), "{error}");
        }
        // A built-in variable keeps its hosts.
        let builtin = sandbox(
            "env_passthrough: [GITHUB_TOKEN]\nsmolvm:\n  credentials:\n    GITHUB_TOKEN: { mode: header }\n",
        );
        machine_credentials(&builtin, &["GITHUB_TOKEN".into()], &[]).unwrap();
    }

    #[test]
    fn test_a_secret_values_debug_form_hides_it() {
        let shown = format!("{:?}", SecretValue::new("ghp-throwaway".into()));
        assert!(!shown.contains("ghp-throwaway"), "{shown}");
    }

    #[test]
    fn test_github_tokens_are_read_only_from_variables_that_pass_them() {
        let lookup = |var: &str| match var {
            "GH_TOKEN" => Some("from-gh".to_string()),
            "GITHUB_TOKEN" => Some("from-github".to_string()),
            _ => None,
        };
        let names = |tokens: Vec<(String, SecretValue)>| -> Vec<(String, String)> {
            tokens
                .into_iter()
                .map(|(name, value)| (name, value.expose().to_string()))
                .collect()
        };
        // Not passed: not read.
        assert!(github_tokens(&sandbox("{}\n"), lookup).is_empty());
        let passthrough = sandbox("env_passthrough: [GITHUB_TOKEN]\n");
        assert_eq!(
            names(github_tokens(&passthrough, lookup)),
            [("GITHUB_TOKEN".to_string(), "from-github".to_string())]
        );
        let keychain = sandbox(
            "env_passthrough: [GH_TOKEN]\nsmolvm:\n  credentials:\n    GH_TOKEN: { from_keychain: gh-work }\n",
        );
        assert!(github_tokens(&keychain, lookup).is_empty());
    }
}
