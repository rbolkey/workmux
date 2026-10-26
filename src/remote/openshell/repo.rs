//! A project's git URL as an OpenShell sandbox can use it. Its proxy
//! swaps in credentials only over HTTPS and no profile allows SSH, so an
//! SSH URL on github.com is rewritten to HTTPS, visibly, and one on any
//! other host is refused.

use anyhow::{Result, bail};

use super::github::GH_LOGIN;

/// What workmux says about an OpenShell project's SSH origin URL.
const SSH_UNSUPPORTED: &str = "uses SSH, which OpenShell sandboxes don't support";

/// The HTTPS form of `url` when it is an SSH URL on github.com; `None`
/// when it is no SSH URL. An SSH URL on another host is an error: nothing
/// can make it work in a sandbox.
pub fn https_for_ssh(url: &str) -> Result<Option<String>> {
    let Some((host, path)) = ssh_host_and_path(url) else {
        return Ok(None);
    };
    if !host.eq_ignore_ascii_case("github.com") {
        bail!(
            "The repository's origin {url} {SSH_UNSUPPORTED}: they reach git hosts over HTTPS only, \
             and workmux rewrites SSH URLs to HTTPS only on github.com; set origin to an https:// URL"
        );
    }
    Ok(Some(format!(
        "https://github.com/{}",
        path.trim_start_matches('/')
    )))
}

/// The host and path of an SSH URL, in its `ssh://` or scp-like form.
fn ssh_host_and_path(url: &str) -> Option<(&str, &str)> {
    for scheme in ["ssh://", "git+ssh://", "ssh+git://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let host = host.split(':').next().unwrap_or(host);
            return Some((host, path));
        }
    }
    // As git reads it: `[user@]host:path`, with no `/` before the colon.
    if url.contains("://") {
        return None;
    }
    let (authority, path) = url.split_once(':')?;
    if authority.is_empty() || authority.contains('/') {
        return None;
    }
    Some((authority.rsplit('@').next().unwrap_or(authority), path))
}

/// What sandbox creation says about an SSH origin it rewrote, given the
/// attached providers that give GitHub credentials, or why they could not
/// be looked up.
pub fn ssh_notice(github: Result<&[String], &str>) -> String {
    let unsupported = format!("The repository's origin {SSH_UNSUPPORTED}");
    match github {
        Ok([]) => format!(
            "{unsupported}; the sandbox reaches github.com over HTTPS instead, with no GitHub credentials attached: \
             public repos will clone, but private repos and push need `{GH_LOGIN}` (your `gh` login) \
             or another GitHub provider in sandbox.openshell.providers."
        ),
        Ok(providers) => format!(
            "{unsupported}; the sandbox reaches github.com over HTTPS with {} credentials instead.",
            providers
                .iter()
                .map(|provider| if provider == GH_LOGIN {
                    format!("`{provider}` (your `gh` login's)")
                } else {
                    format!("`{provider}`")
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(error) => format!(
            "{unsupported}; the sandbox reaches github.com over HTTPS instead, with the GitHub credentials its providers give \
             (could not check which do: {error})."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_github_urls_are_rewritten_to_https() {
        for (ssh, https) in [
            ("git@github.com:me/api.git", "https://github.com/me/api.git"),
            ("git@github.com:me/api", "https://github.com/me/api"),
            (
                "git@GitHub.com:/me/api.git",
                "https://github.com/me/api.git",
            ),
            (
                "ssh://git@github.com/me/api.git",
                "https://github.com/me/api.git",
            ),
            (
                "ssh://git@github.com:22/me/api.git",
                "https://github.com/me/api.git",
            ),
            (
                "git+ssh://github.com/me/api.git",
                "https://github.com/me/api.git",
            ),
        ] {
            assert_eq!(https_for_ssh(ssh).unwrap().as_deref(), Some(https), "{ssh}");
        }
    }

    #[test]
    fn https_and_local_urls_are_kept() {
        for url in [
            "https://github.com/me/api.git",
            "http://git.example.com/api.git",
            "/sandbox/origin",
            "./origin",
            "file:///srv/api.git",
        ] {
            assert_eq!(https_for_ssh(url).unwrap(), None, "{url}");
        }
    }

    #[test]
    fn ssh_urls_on_other_hosts_are_refused() {
        for url in [
            "git@gitlab.com:me/api.git",
            "ssh://git@git.example.com/api.git",
            "github-work:me/api.git",
        ] {
            let error = https_for_ssh(url).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "The repository's origin {url} {SSH_UNSUPPORTED}: they reach git hosts over HTTPS only, \
                     and workmux rewrites SSH URLs to HTTPS only on github.com; set origin to an https:// URL"
                )
            );
        }
    }

    #[test]
    fn the_rewrite_notice_names_the_providers_giving_github_credentials() {
        assert_eq!(
            ssh_notice(Ok(&["work".to_string()])),
            "The repository's origin uses SSH, which OpenShell sandboxes don't support; \
             the sandbox reaches github.com over HTTPS with `work` credentials instead."
        );
        assert_eq!(
            ssh_notice(Ok(&["work".to_string(), "bot".to_string()])),
            "The repository's origin uses SSH, which OpenShell sandboxes don't support; \
             the sandbox reaches github.com over HTTPS with `work`, `bot` credentials instead."
        );
    }

    #[test]
    fn with_gh_login_the_notice_names_it_as_the_gh_login() {
        assert_eq!(
            ssh_notice(Ok(&["gh-login".to_string()])),
            "The repository's origin uses SSH, which OpenShell sandboxes don't support; \
             the sandbox reaches github.com over HTTPS with `gh-login` (your `gh` login's) credentials instead."
        );
    }

    #[test]
    fn without_github_credentials_the_notice_says_what_will_and_wont_work() {
        assert_eq!(
            ssh_notice(Ok(&[])),
            "The repository's origin uses SSH, which OpenShell sandboxes don't support; \
             the sandbox reaches github.com over HTTPS instead, with no GitHub credentials attached: \
             public repos will clone, but private repos and push need `gh-login` (your `gh` login) \
             or another GitHub provider in sandbox.openshell.providers."
        );
        assert_eq!(
            ssh_notice(Err("gateway down")),
            "The repository's origin uses SSH, which OpenShell sandboxes don't support; \
             the sandbox reaches github.com over HTTPS instead, with the GitHub credentials its providers give \
             (could not check which do: gateway down)."
        );
    }
}
