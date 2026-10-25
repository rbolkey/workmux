//! A remote project's git URL: the one its remote clones, with the SSH URL
//! it was rewritten from when the remote can't speak SSH (an OpenShell
//! sandbox). Config and the control channel keep the two as the flat
//! fields `git_url` and `ssh_git_url`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitUrl {
    /// The URL the remote clones.
    #[serde(rename = "git_url")]
    pub url: String,
    /// The SSH URL `url` was rewritten from, as the user gave it.
    #[serde(
        rename = "ssh_git_url",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ssh: Option<String>,
}

impl GitUrl {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ssh: None,
        }
    }

    /// The URL as the user gave it, for the workstation, which has SSH.
    pub fn original(&self) -> &str {
        self.ssh.as_deref().unwrap_or(&self.url)
    }

    /// The URL as workmux shows it: with the SSH URL it was rewritten
    /// from, if any.
    pub fn describe(&self) -> String {
        match &self.ssh {
            Some(ssh) => format!("{} (rewritten from SSH: {ssh})", self.url),
            None => self.url.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten() -> GitUrl {
        GitUrl {
            url: "https://github.com/me/api.git".to_string(),
            ssh: Some("git@github.com:me/api.git".to_string()),
        }
    }

    #[test]
    fn a_rewritten_url_is_shown_with_its_original() {
        assert_eq!(
            rewritten().describe(),
            "https://github.com/me/api.git (rewritten from SSH: git@github.com:me/api.git)"
        );
        assert_eq!(rewritten().original(), "git@github.com:me/api.git");
        let plain = GitUrl::new("https://github.com/me/api.git");
        assert_eq!(plain.describe(), "https://github.com/me/api.git");
        assert_eq!(plain.original(), "https://github.com/me/api.git");
    }

    /// As config and the control channel carry it.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Carrier {
        name: String,
        #[serde(flatten)]
        git_url: Option<GitUrl>,
    }

    #[test]
    fn it_travels_as_the_flat_git_url_and_ssh_git_url_fields() {
        let carrier = Carrier {
            name: "api".to_string(),
            git_url: Some(rewritten()),
        };

        let json = serde_json::to_value(&carrier).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "name": "api",
                "git_url": "https://github.com/me/api.git",
                "ssh_git_url": "git@github.com:me/api.git",
            })
        );
        assert_eq!(serde_json::from_value::<Carrier>(json).unwrap(), carrier);
        let plain: Carrier =
            serde_yaml::from_str("name: api\ngit_url: git@github.com:me/api.git\n").unwrap();
        assert_eq!(
            plain.git_url,
            Some(GitUrl::new("git@github.com:me/api.git"))
        );
    }

    #[test]
    fn no_url_is_written_as_none_and_read_from_none_or_null() {
        let none = Carrier {
            name: "api".to_string(),
            git_url: None,
        };

        assert_eq!(
            serde_json::to_value(&none).unwrap(),
            serde_json::json!({ "name": "api" })
        );
        for json in [r#"{"name":"api"}"#, r#"{"name":"api","git_url":null}"#] {
            assert_eq!(
                serde_json::from_str::<Carrier>(json).unwrap(),
                none,
                "{json}"
            );
        }
    }
}
