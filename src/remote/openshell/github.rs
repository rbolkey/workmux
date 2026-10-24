//! `--github host`: a sandbox gets the workstation user's own GitHub access
//! from their `gh` login. OpenShell's proxy resolves a provider's
//! placeholder from the gateway's credential store and can't run a command,
//! so the gateway keeps a copy of the login's token in one provider, which
//! workmux updates each time it creates or attaches to a sandbox.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

/// The profile workmux ships for the provider: what the `gh` login can do
/// on GitHub, push and pull requests included.
pub const PROFILE: &str = "workmux-github";

/// The provider holding the `gh` login's token, one per gateway.
pub const PROVIDER: &str = "workmux-gh-host";

/// The provider's credential, which the sandbox sees in its environment.
pub const CREDENTIAL: &str = "GH_TOKEN";

const PROFILE_YAML: &str = include_str!("workmux-github.yaml");

/// The `gh` login's token for github.com. A missing `gh` or a logged-out
/// one is an error naming `gh auth login`: the sandbox never goes without
/// the credentials its project asked for.
pub fn host_token(gh: &Path) -> Result<String> {
    let output = Command::new(gh)
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "the project's GitHub credentials come from your gh login (--github host), \
             but gh is not installed on this machine; install it and run `gh auth login`"
        ),
        result => result.context("failed to run gh")?,
    };
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "the project's GitHub credentials come from your gh login (--github host), \
             but gh is not logged in to github.com{}; run `gh auth login`",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(" ({stderr})")
            }
        );
    }
    Ok(token)
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

/// What workmux records of the token it last gave provider `provider_id`,
/// so an unchanged token isn't sent again. Never the token itself.
pub fn digest(provider_id: &str, token: &str) -> String {
    super::image::sha256_hex(format!("{provider_id}\n{token}").as_bytes())
}

/// `--github host` beside an attached provider that already gives GitHub
/// credentials is an error: two sources would be ambiguous.
pub fn refuse_alongside(github_providers: &[String]) -> Result<()> {
    if !github_providers.is_empty() {
        bail!(
            "--github host gives the sandbox your gh login, but provider {} already gives it GitHub credentials; \
             use one or the other",
            github_providers.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_gh(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_host_token_is_the_gh_logins_token_for_github_com() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            "[ \"$*\" = 'auth token --hostname github.com' ] && echo gho_secret",
        );

        assert_eq!(host_token(&gh).unwrap(), "gho_secret");
    }

    #[test]
    fn a_missing_or_logged_out_gh_is_an_error_naming_gh_auth_login() {
        let dir = tempfile::tempdir().unwrap();
        let logged_out = fake_gh(
            dir.path(),
            "echo 'no oauth token found for github.com' >&2; exit 1",
        );

        let error = host_token(&logged_out).unwrap_err().to_string();
        assert_eq!(
            error,
            "the project's GitHub credentials come from your gh login (--github host), but gh is not logged in \
             to github.com (no oauth token found for github.com); run `gh auth login`"
        );
        let error = host_token(&dir.path().join("missing"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("gh is not installed"), "{error}");
        assert!(error.ends_with("run `gh auth login`"), "{error}");
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
    fn host_mode_is_refused_beside_a_github_provider() {
        assert!(refuse_alongside(&[]).is_ok());

        let error = refuse_alongside(&["work".to_string()]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "--github host gives the sandbox your gh login, but provider work already gives it GitHub credentials; \
             use one or the other"
        );
    }
}
