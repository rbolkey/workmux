//! GitHub credentials in an OpenShell sandbox: which attached providers
//! give them, and `gh-login`, the provider workmux keeps from the
//! workstation user's own `gh` login. OpenShell's proxy resolves a
//! provider's placeholder from the gateway's credential store and can't run
//! a command, so the gateway keeps a copy of the login's token in
//! `gh-login`, which workmux updates each time it creates or attaches to a
//! sandbox.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

use super::{OpenShell, agents, failure};

/// The provider workmux keeps from the workstation's `gh` login, one per
/// gateway. Attaching it (`--provider gh-login`) gives a sandbox everything
/// that login can do on GitHub.
pub const GH_LOGIN: &str = "gh-login";

/// The label marking `gh-login` as workmux's own; a `gh-login` without it
/// is someone else's and never touched.
const MANAGED_LABEL: (&str, &str) = ("workmux.managed", GH_LOGIN);

/// The profile workmux ships for `gh-login`: what the `gh` login can do on
/// GitHub, push and pull requests included.
pub const PROFILE: &str = "workmux-github";

/// `gh-login`'s credential, which the sandbox sees in its environment.
pub const CREDENTIAL: &str = "GH_TOKEN";

const PROFILE_YAML: &str = include_str!("workmux-github.yaml");

/// The `gh` login's token for github.com. A missing `gh` or a logged-out
/// one is an error naming `gh auth login`: the sandbox never goes without
/// the credentials its project asked for.
pub fn login_token(gh: &Path) -> Result<String> {
    let output = Command::new(gh)
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .output();
    let unusable = |problem: &str, fix: &str| {
        anyhow!("provider {GH_LOGIN} gives the sandbox your gh login, but {problem}; {fix}")
    };
    let output = match output {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(unusable(
                "gh is not installed on the workstation",
                "install it and run `gh auth login`",
            ));
        }
        result => result.context("failed to run gh")?,
    };
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let problem = if stderr.is_empty() {
            "gh is not logged in to github.com".to_string()
        } else {
            format!("gh is not logged in to github.com ({stderr})")
        };
        return Err(unusable(&problem, "run `gh auth login`"));
    }
    Ok(token)
}

/// Whether provider `name` gives git credentials for github.com:
/// `gh-login`, or one whose profile reaches github.com, or, with no
/// profile to go by, one of OpenShell's stock `github` type.
pub fn gives_github(name: &str, provider: Option<&agents::Provider>) -> bool {
    if name == GH_LOGIN {
        return true;
    }
    match provider {
        Some(agents::Provider {
            profile: Some(profile),
            ..
        }) => profile.hosts.iter().any(|host| host == "github.com"),
        Some(provider) => provider.provider_type == "github",
        None => false,
    }
}

/// Two attached providers that both give GitHub credentials are an error:
/// which one git uses would be ambiguous.
pub fn refuse_two(github_providers: &[String]) -> Result<()> {
    if github_providers.len() > 1 {
        bail!(
            "providers {} all give the sandbox GitHub credentials; attach only one",
            github_providers.join(", ")
        );
    }
    Ok(())
}

/// The shipped profile, for `profile import`.
pub fn profile_yaml() -> &'static str {
    PROFILE_YAML
}

/// The shipped profile as `profile update` takes it: with the installed
/// profile's resource version.
pub fn profile_update_yaml(resource_version: &Value) -> Result<String> {
    let mut profile: serde_yaml::Value = serde_yaml::from_str(PROFILE_YAML)?;
    profile["resource_version"] = serde_yaml::to_value(resource_version)?;
    Ok(serde_yaml::to_string(&profile)?)
}

/// Whether `installed`, as `profile list -o json` reports it, differs from
/// the shipped profile. The gateway fills in empty fields, which don't
/// count.
pub fn profile_differs(installed: &Value) -> bool {
    let shipped: Value = serde_yaml::from_str(PROFILE_YAML).expect("the shipped profile is YAML");
    [
        "display_name",
        "description",
        "credentials",
        "discovery",
        "endpoints",
        "binaries",
    ]
    .into_iter()
    .any(|field| without_empty(&installed[field]) != without_empty(&shipped[field]))
}

fn without_empty(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, value)| !matches!(value, Value::Null) && *value != "")
                .map(|(key, value)| (key.clone(), without_empty(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_empty).collect()),
        value => value.clone(),
    }
}

/// Whether `provider`, as `provider list -o json` reports it, is
/// workmux's own `gh-login`.
fn is_managed(provider: &Value) -> bool {
    provider["labels"][MANAGED_LABEL.0] == MANAGED_LABEL.1
}

impl OpenShell {
    /// Whether the project attaches `gh-login`.
    pub(super) fn attaches_gh_login(&self) -> bool {
        self.attached_providers()
            .iter()
            .any(|name| name == GH_LOGIN)
    }

    /// The attached providers that give git credentials for github.com.
    /// `gh-login` needs no lookup; the others are looked up only when
    /// there are any.
    pub fn github_providers(&self) -> Result<Vec<String>> {
        let attached = self.attached_providers();
        if attached.iter().all(|name| name == GH_LOGIN) {
            return Ok(attached);
        }
        let providers = self.providers()?;
        Ok(attached
            .into_iter()
            .filter(|name| gives_github(name, providers.get(name)))
            .collect())
    }

    /// For `project add` and sandbox create: at most one attached provider
    /// may give GitHub credentials, and with `gh-login` attached, the `gh`
    /// login must be there and a `gh-login` on the gateway must be
    /// workmux's. A check that can't run is an error.
    pub fn check_github_credentials(&self) -> Result<()> {
        if self.attached_providers().len() > 1 {
            let github = self.github_providers().map_err(|error| {
                anyhow!(
                    "could not check which attached providers give GitHub credentials, \
                     and at most one may: {error:#}"
                )
            })?;
            refuse_two(&github)?;
        }
        if self.attaches_gh_login() {
            login_token(&self.gh)?;
            self.gh_login_provider()?;
        }
        Ok(())
    }

    /// `gh-login` as the gateway lists it, or `None` when it has none. One
    /// workmux didn't create is an error, and is never touched.
    fn gh_login_provider(&self) -> Result<Option<Value>> {
        let provider = self
            .provider_list()?
            .into_iter()
            .find(|provider| provider["name"] == GH_LOGIN);
        if let Some(provider) = &provider
            && !is_managed(provider)
        {
            bail!(
                "the gateway already has a provider named {GH_LOGIN} that workmux did not create \
                 (it has no {}={} label); workmux leaves it alone. Rename or delete it, \
                 or attach your own GitHub provider instead of {GH_LOGIN}",
                MANAGED_LABEL.0,
                MANAGED_LABEL.1
            );
        }
        Ok(provider)
    }

    /// Give `gh-login` the `gh` login's current token, creating it on first
    /// use; at sandbox create (`sync_profile`), workmux's profile is
    /// brought up to date too. The token reaches `openshell` through its
    /// environment only, never its arguments.
    pub(super) fn refresh_gh_login(&self, sync_profile: bool) -> Result<()> {
        if !self.attaches_gh_login() {
            return Ok(());
        }
        let token = login_token(&self.gh)?;
        if sync_profile {
            self.sync_github_profile()?;
        }
        if self.gh_login_provider()?.is_some() {
            return self.run_with_token(
                &[
                    "provider",
                    "update",
                    GH_LOGIN,
                    "--credential",
                    CREDENTIAL,
                    "--wait",
                ],
                &token,
            );
        }
        if !sync_profile {
            self.sync_github_profile()?;
        }
        self.run_with_token(
            &[
                "provider",
                "create",
                "--name",
                GH_LOGIN,
                "--type",
                PROFILE,
                "--global-profile",
                "--label",
                &format!("{}={}", MANAGED_LABEL.0, MANAGED_LABEL.1),
                "--credential",
                CREDENTIAL,
            ],
            &token,
        )
    }

    /// Import workmux's GitHub profile into the gateway, or update it there
    /// when it differs.
    fn sync_github_profile(&self) -> Result<()> {
        let profiles = self.profiles()?;
        let installed = profiles
            .as_array()
            .into_iter()
            .flatten()
            .find(|profile| profile["id"] == PROFILE);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("profile.yaml");
        let path_arg = path.to_string_lossy();
        match installed {
            None => {
                std::fs::write(&path, profile_yaml())?;
                self.run(&["provider", "profile", "import", "--global", "-f", &path_arg])?;
            }
            Some(installed) if profile_differs(installed) => {
                std::fs::write(&path, profile_update_yaml(&installed["resource_version"])?)?;
                self.run(&[
                    "provider", "profile", "update", "--global", "-f", &path_arg, PROFILE,
                ])?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Run an `openshell` command that reads the credential from its
    /// environment.
    fn run_with_token(&self, args: &[&str], token: &str) -> Result<()> {
        let output = self
            .openshell()
            .args(args)
            .env(CREDENTIAL, token)
            .stdin(Stdio::null())
            .output()
            .context("failed to run openshell")?;
        if !output.status.success() {
            bail!("{}", failure(&output));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpenshellRemote;
    use crate::remote::openshell::tests::{FakeOpenShell, origin};
    use crate::remote::transport::Provisioning;
    use std::os::unix::fs::PermissionsExt;

    fn fake_gh(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_login_token_is_the_gh_logins_token_for_github_com() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            "[ \"$*\" = 'auth token --hostname github.com' ] && echo gho_secret",
        );

        assert_eq!(login_token(&gh).unwrap(), "gho_secret");
    }

    #[test]
    fn a_missing_or_logged_out_gh_is_an_error_naming_gh_auth_login() {
        let dir = tempfile::tempdir().unwrap();
        let logged_out = fake_gh(
            dir.path(),
            "echo 'no oauth token found for github.com' >&2; exit 1",
        );

        let error = login_token(&logged_out).unwrap_err().to_string();
        assert_eq!(
            error,
            "provider gh-login gives the sandbox your gh login, but gh is not logged in \
             to github.com (no oauth token found for github.com); run `gh auth login`"
        );
        let error = login_token(&dir.path().join("missing"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "provider gh-login gives the sandbox your gh login, but gh is not installed on the workstation; \
             install it and run `gh auth login`"
        );
    }

    #[test]
    fn the_installed_profile_differs_only_when_what_it_allows_differs() {
        // As the gateway reports the imported profile: fields reordered,
        // empty ones filled in, its own bookkeeping added.
        let mut installed: Value = serde_yaml::from_str(PROFILE_YAML).unwrap();
        installed["credentials"][0]["query_param"] = "".into();
        installed["resource_version"] = 3.into();
        installed["scope"] = "platform".into();
        assert!(!profile_differs(&installed));

        installed["binaries"].as_array_mut().unwrap().pop();
        assert!(profile_differs(&installed));
        assert!(profile_differs(&Value::Null));
    }

    #[test]
    fn the_shipped_profile_lets_git_and_gh_push_and_write_through_the_api() {
        let profile: Value = serde_yaml::from_str(PROFILE_YAML).unwrap();

        assert_eq!(profile["id"], PROFILE);
        let binaries: Vec<&str> = profile["binaries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for binary in ["/usr/bin/git", "/usr/bin/gh", "/nix/store/*-git-*/bin/git"] {
            assert!(binaries.contains(&binary), "{binaries:?}");
        }
        let endpoints = profile["endpoints"].as_array().unwrap();
        assert!(
            endpoints
                .iter()
                .filter(|e| e["host"] == "api.github.com")
                .all(|e| e["access"] == "read-write")
        );
        let github = endpoints
            .iter()
            .find(|e| e["host"] == "github.com")
            .unwrap();
        assert!(
            github["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["allow"]["method"] == "POST"
                    && rule["allow"]["path"] == "/**/git-receive-pack")
        );
    }

    #[test]
    fn the_profile_update_carries_the_installed_resource_version() {
        let update: Value = serde_yaml::from_str(&profile_update_yaml(&4.into()).unwrap()).unwrap();

        assert_eq!(update["resource_version"], 4);
        assert_eq!(update["id"], PROFILE);
    }

    #[test]
    fn two_providers_giving_github_credentials_are_refused() {
        assert!(refuse_two(&[]).is_ok());
        assert!(refuse_two(&["work".to_string()]).is_ok());

        let error = refuse_two(&["gh-login".to_string(), "work".to_string()]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "providers gh-login, work all give the sandbox GitHub credentials; attach only one"
        );
    }

    #[test]
    fn github_credentials_come_from_gh_login_and_providers_whose_profile_reaches_github() {
        let fake = FakeOpenShell::new();
        fake.providers(
            r#"[{"name":"work","type":"github-dev"},
                {"name":"stock","type":"github"},
                {"name":"anthropic","type":"claude-code"},
                {"name":"unused","type":"github-dev"}]"#,
        );
        fake.profiles(
            r#"[{"id":"github-dev","binaries":["/usr/bin/git"],
                 "endpoints":[{"host":"api.github.com","port":443},{"host":"github.com","port":443}]},
                {"id":"claude-code","binaries":["/usr/local/bin/claude"],
                 "endpoints":[{"host":"api.anthropic.com","port":443}]}]"#,
        );
        // `stock`'s profile is not installed: its stock type is enough.
        // `gh-login` doesn't exist until first use.
        let github = fake
            .transport(Some(attaching(&["anthropic", "work", "stock", "gh-login"])))
            .github_providers()
            .unwrap();

        assert_eq!(github, ["work", "stock", "gh-login"]);
        assert!(
            fake.transport(Some(attaching(&[])))
                .github_providers()
                .unwrap()
                .is_empty()
        );
    }

    fn attaching(providers: &[&str]) -> OpenshellRemote {
        let mut origin = origin();
        origin.settings.providers = providers.iter().map(|p| p.to_string()).collect();
        origin
    }

    fn gh_login_origin() -> OpenshellRemote {
        attaching(&["anthropic", "gh-login"])
    }

    #[test]
    fn gh_login_is_created_on_first_use_labelled_as_workmuxs_and_attached_to_the_sandbox() {
        let fake = FakeOpenShell::new();
        fake.providers(r#"[{"name":"anthropic","type":"claude-code"}]"#);
        fake.log_in("gho_first");
        let transport = fake.transport(Some(gh_login_origin()));

        transport.create().unwrap();

        let log = fake.log();
        assert!(
            log.iter()
                .any(|line| line.starts_with("provider profile import --global -f ")),
            "{log:?}"
        );
        let imported: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(fake.path("imported")).unwrap()).unwrap();
        assert_eq!(imported["id"], "workmux-github");
        assert!(
            log.contains(
                &"provider create --name gh-login --type workmux-github --global-profile \
              --label workmux.managed=gh-login --credential GH_TOKEN"
                    .to_string()
            )
        );
        assert_eq!(fake.sent(), ["create gho_first"]);
        let create = log
            .iter()
            .find(|line| line.starts_with("sandbox create"))
            .unwrap();
        assert!(
            create.contains("--provider anthropic --provider gh-login"),
            "{create}"
        );
        assert!(
            log.iter().all(|line| !line.contains("gho_first")),
            "{log:?}"
        );
    }

    #[test]
    fn gh_login_gets_the_current_token_at_every_command_and_attach() {
        let fake = FakeOpenShell::new();
        fake.log_in("gho_first");
        let transport = fake.transport(Some(attaching(&["gh-login"])));
        transport.create().unwrap();

        transport.reconcile().unwrap();
        fake.log_in("gho_second");
        transport.refresh_credentials().unwrap();

        assert_eq!(
            fake.sent(),
            ["create gho_first", "update gho_first", "update gho_second"]
        );
        assert!(
            fake.log()
                .contains(&"provider update gh-login --credential GH_TOKEN --wait".to_string())
        );
        assert!(fake.log().iter().all(|line| !line.contains("gho_")));
    }

    #[test]
    fn a_gh_login_provider_workmux_did_not_create_is_an_error_and_never_touched() {
        let fake = FakeOpenShell::new();
        fake.providers(r#"[{"name":"gh-login","type":"github","labels":{"team":"me"}}]"#);
        fake.log_in("gho_first");
        let transport = fake.transport(Some(attaching(&["gh-login"])));

        let error = transport
            .check_github_credentials()
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "the gateway already has a provider named gh-login that workmux did not create"
            ),
            "{error}"
        );
        assert!(transport.create().is_err());
        assert!(transport.refresh_credentials().is_err());

        assert!(fake.sent().is_empty());
        assert!(
            fake.log()
                .iter()
                .all(|line| !line.starts_with("sandbox create")
                    && !line.starts_with("provider profile import")),
            "{:?}",
            fake.log()
        );
    }

    #[test]
    fn a_logged_out_gh_fails_create_and_attach_closed() {
        let fake = FakeOpenShell::new();
        let transport = fake.transport(Some(gh_login_origin()));

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
        fake.providers(
            r#"[{"name":"gh-login","type":"workmux-github","labels":{"workmux.managed":"gh-login"}}]"#,
        );
        fake.profiles(
            r#"[{"id":"workmux-github","resource_version":2,"binaries":["/usr/bin/git"],
                 "endpoints":[{"host":"github.com","port":443}]}]"#,
        );
        fake.log_in("gho_first");

        fake.transport(Some(attaching(&["gh-login"])))
            .create()
            .unwrap();

        let log = fake.log();
        assert!(
            log.iter().any(
                |line| line.starts_with("provider profile update --global -f ")
                    && line.ends_with(" workmux-github")
            ),
            "{log:?}"
        );
        let imported: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(fake.path("imported")).unwrap()).unwrap();
        assert_eq!(imported["resource_version"], 2);
        // The provider exists: its token is updated, not created.
        assert_eq!(fake.sent(), ["update gho_first"]);
    }

    #[test]
    fn project_add_and_create_refuse_two_github_providers() {
        let fake = FakeOpenShell::new();
        fake.providers(
            r#"[{"name":"anthropic","type":"claude-code"},{"name":"work","type":"github"}]"#,
        );
        fake.log_in("gho_first");

        assert!(
            fake.transport(Some(gh_login_origin()))
                .check_github_credentials()
                .is_ok()
        );
        let transport = fake.transport(Some(attaching(&["anthropic", "work", "gh-login"])));
        let error = transport.check_github_credentials().unwrap_err();
        assert_eq!(
            error.to_string(),
            "providers work, gh-login all give the sandbox GitHub credentials; attach only one"
        );
        let error = transport.create().unwrap_err();
        assert!(
            error.to_string().starts_with("providers work, gh-login"),
            "{error}"
        );

        // Nothing is created before first use, nor for a refused sandbox.
        assert!(fake.sent().is_empty());
        assert!(
            fake.log()
                .iter()
                .all(|line| !line.starts_with("sandbox create"))
        );
    }

    #[test]
    fn a_check_that_cannot_ask_the_gateway_is_an_error() {
        let fake = FakeOpenShell::new();
        fake.log_in("gho_first");
        // No provider list: `openshell` fails.
        std::fs::remove_file(fake.path("providers")).unwrap();
        let transport = fake.transport(Some(attaching(&["work", "gh-login"])));

        let error = transport.check_github_credentials().unwrap_err();

        assert!(
            error.to_string().starts_with(
                "could not check which attached providers give GitHub credentials, and at most one may: "
            ),
            "{error}"
        );
        // One provider has nothing to compare against.
        assert!(
            fake.transport(Some(attaching(&["work"])))
                .check_github_credentials()
                .is_ok()
        );
    }

    #[test]
    fn project_add_checks_the_gh_login() {
        let fake = FakeOpenShell::new();

        let error = fake
            .transport(Some(gh_login_origin()))
            .check_github_credentials()
            .unwrap_err();

        assert!(
            error.to_string().ends_with("run `gh auth login`"),
            "{error}"
        );
    }
}
