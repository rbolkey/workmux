//! Installing a workmux that matches this workstation on an SSH remote.
//!
//! When a remote has no workmux, or one that speaks another control
//! protocol, workmux offers to install a matching build in a per-user cache
//! directory on the remote ([`uploaded_workmux_dir`]) and to run remote
//! workmux from there. A workmux the user installed on the remote's PATH is
//! never touched. The user is asked the first time; their answer is kept in
//! the remote's `upload_workmux` setting.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::channel::Handler;
use super::connection::{Connection, HELLO_TIMEOUT};
use super::ssh::{Ssh, uploaded_workmux_dir};
use super::transport::Transport;
use crate::command::update;

/// Set when building a release, so a build can tell whether a released
/// artifact of its version is the same program.
const RELEASE_BUILD: bool = option_env!("WORKMUX_RELEASE_BUILD").is_some();

/// Where a build downloads released builds from: its release source, when
/// it is a release build. There is no default source (ticket 54).
fn releases_for(release_build: bool, source: Option<String>) -> Option<String> {
    source.filter(|_| release_build)
}

/// Where a workmux matching this workstation comes from.
pub struct Builds {
    /// This workstation's own binary and the platform it runs on.
    pub own: Option<(String, PathBuf)>,
    /// Base URL of released builds, `<base>/v<version>/workmux-<platform>.tar.gz`
    /// with its `.sha256`: the build's release source (ticket 54). `None`
    /// for a development build, which no release matches, or a build with
    /// no release source.
    pub releases: Option<String>,
}

impl Builds {
    /// The builds available to this binary.
    pub fn current() -> Self {
        let own = crate::command::update::platform_suffix()
            .ok()
            .zip(std::env::current_exe().ok())
            .map(|(platform, exe)| (platform.to_string(), exe));
        Self {
            own,
            releases: releases_for(RELEASE_BUILD, update::release_source()),
        }
    }

    /// A workmux binary for `platform`, downloading into `dir` if need be.
    pub fn binary_for(&self, remote: &str, platform: &str, dir: &Path) -> Result<PathBuf> {
        if let Some((own, binary)) = &self.own
            && own == platform
        {
            return Ok(binary.clone());
        }
        let version = env!("CARGO_PKG_VERSION");
        let Some(base) = &self.releases else {
            bail!(
                "No release of workmux {version} can be downloaded for {remote}'s platform \
                 ({platform}): this is a development build or has no release source. Build \
                 workmux for {platform} and run `workmux remote upload {remote} --binary <path>`"
            );
        };
        let artifact = format!("workmux-{platform}");
        let url = format!("{base}/v{version}/{artifact}");
        let archive = dir.join(format!("{artifact}.tar.gz"));
        let checksum = dir.join(format!("{artifact}.sha256"));
        let fetch = || -> Result<()> {
            update::download(&format!("{url}.tar.gz"), &archive)?;
            update::download(&format!("{url}.sha256"), &checksum)?;
            let expected = std::fs::read_to_string(&checksum)
                .context("Failed to read the release checksum")?;
            update::verify_checksum(&archive, &expected)?;
            update::extract_tar(&archive, dir)
        };
        fetch().with_context(|| {
            format!("failed to get the workmux {version} release for {platform}")
        })?;
        let binary = dir.join("workmux");
        if !binary.exists() {
            bail!("The workmux {version} release for {platform} holds no workmux binary");
        }
        Ok(binary)
    }
}

/// The release artifact suffix for a remote whose `uname -sm` printed
/// `uname`, such as `linux-amd64`.
pub fn platform_of(uname: &str) -> Result<String> {
    let uname = uname.trim();
    let mut words = uname.split_whitespace();
    let os = match words.next() {
        Some("Linux") => "linux",
        Some("Darwin") => "darwin",
        _ => bail!("workmux has no release for a remote running {uname}"),
    };
    let arch = match words.next() {
        Some("x86_64" | "amd64") => "amd64",
        Some("aarch64" | "arm64") => "arm64",
        _ => bail!("workmux has no release for a remote running {uname}"),
    };
    Ok(format!("{os}-{arch}"))
}

/// Answers nothing: a probe's channel carries no requests.
struct NoRequests;

impl Handler for NoRequests {
    fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
        Err(format!("unknown method: {method}"))
    }
}

/// What came of offering to install workmux on a remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The remote's workmux works as it is.
    NotNeeded,
    /// A matching workmux was installed.
    Installed,
    /// The user said no, now or before.
    Declined,
}

/// What a remote's settings and its user decide about an upload.
pub struct Offer<'a> {
    pub remote: &'a str,
    /// The transport, as the remote's settings configure it.
    pub ssh: &'a Ssh,
    /// The remote's `upload_workmux` setting.
    pub answer: Option<bool>,
    pub builds: &'a Builds,
}

impl Offer<'_> {
    /// Check whether the remote can serve a control channel and, when it
    /// can't for want of a compatible workmux, install one: right away if
    /// the user agreed before, after asking them with `ask` if they were
    /// never asked. `ask` gets the problem and the question, and returns
    /// `None` when no one can answer.
    pub fn run(&self, ask: &mut dyn FnMut(&str, &str) -> Result<Option<bool>>) -> Result<Outcome> {
        let Some(problem) = self.problem() else {
            return Ok(Outcome::NotNeeded);
        };
        let agreed = match self.answer {
            Some(answer) => answer,
            None => {
                let question = format!(
                    "Install workmux {} on {} in its cache directory ({}), \
                     leaving any workmux on its PATH alone?",
                    env!("CARGO_PKG_VERSION"),
                    self.remote,
                    uploaded_workmux_dir()
                );
                ask(&problem, &question)?.unwrap_or(false)
            }
        };
        if !agreed {
            return Ok(Outcome::Declined);
        }
        install(self.remote, self.ssh, self.builds)?;
        Ok(Outcome::Installed)
    }

    /// Why the remote cannot serve a control channel for want of a
    /// compatible workmux, or `None` when it can (or fails for another
    /// reason, such as being unreachable).
    fn problem(&self) -> Option<String> {
        let ssh = self
            .ssh
            .clone()
            .with_uploaded_workmux(self.answer == Some(true));
        match Connection::open(self.remote, &ssh, NoRequests, HELLO_TIMEOUT) {
            Ok(_) => None,
            Err(error) => {
                let error = format!("{error:#}");
                needs_workmux(&error).then_some(error)
            }
        }
    }
}

/// Whether a control channel failed to open for want of a compatible
/// workmux on the remote: every such failure names the version to install.
pub fn needs_workmux(error: &str) -> bool {
    error.contains(&format!("install workmux {}", env!("CARGO_PKG_VERSION")))
}

/// Install a workmux matching this workstation on `remote`, in its cache
/// directory.
pub fn install(remote: &str, ssh: &Ssh, builds: &Builds) -> Result<()> {
    let platform = remote_platform(remote, ssh)?;
    let dir = tempfile::tempdir().context("Failed to create a temp directory")?;
    let binary = builds.binary_for(remote, &platform, dir.path())?;
    eprintln!(
        "Installing workmux {} on {remote}...",
        env!("CARGO_PKG_VERSION")
    );
    ssh.upload_workmux(&binary)
}

/// The release platform of `remote`, from its `uname`.
fn remote_platform(remote: &str, ssh: &Ssh) -> Result<String> {
    let output = ssh.exec_captured(&["uname".to_string(), "-sm".to_string()])?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to find {remote}'s platform: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    platform_of(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_build_downloads_only_from_its_own_release_source() {
        assert_eq!(releases_for(true, None), None);
        assert_eq!(
            releases_for(true, Some("file:///releases".to_string())).as_deref(),
            Some("file:///releases")
        );
        assert_eq!(
            releases_for(false, Some("file:///releases".to_string())),
            None
        );
    }
    use crate::remote::handshake::{Hello, PROTOCOL_VERSION, SENTINEL};
    use crate::remote::ssh::tests::{install_remote_workmux, serve_script, transport};
    use crate::ssh_learning::harness::{SshHarness, local_sshd, ssh_harness};
    use std::time::Duration;

    /// Gives the remote a home of its own, so uploads land in the harness's
    /// directory, and a PATH with no workmux on it.
    fn isolate_remote(harness: &SshHarness, extra: &str) -> String {
        let home = format!("{}/home", harness.remote_dir());
        std::fs::create_dir_all(&home).unwrap();
        let startup = format!(
            "export HOME={home}; unset XDG_CACHE_HOME; export PATH=/usr/bin:/bin\n{extra}\n"
        );
        harness
            .local()
            .expect("needs the local sshd harness")
            .set_shell_startup(&startup);
        home
    }

    /// A stand-in for a workmux build: a script serving a real control
    /// channel.
    fn serving_build(dir: &Path) -> PathBuf {
        let path = dir.join("workmux");
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", serve_script())).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        path
    }

    fn this_platform() -> String {
        crate::command::update::platform_suffix()
            .unwrap()
            .to_string()
    }

    fn never_asked(_problem: &str, question: &str) -> Result<Option<bool>> {
        panic!("asked: {question}")
    }

    fn connect(harness: &SshHarness, ssh: &Ssh) -> Result<Connection> {
        Connection::open(harness.host(), ssh, NoRequests, Duration::from_secs(10))
    }

    #[test]
    fn a_remote_without_workmux_is_offered_this_workstations_build_and_then_serves() {
        let harness = ssh_harness!();
        let home = isolate_remote(&harness, "");
        let local = tempfile::tempdir().unwrap();
        let builds = Builds {
            own: Some((this_platform(), serving_build(local.path()))),
            releases: None,
        };
        let ssh = transport(&harness);
        let mut asked = Vec::new();

        let outcome = Offer {
            remote: harness.host(),
            ssh: &ssh,
            answer: None,
            builds: &builds,
        }
        .run(&mut |_, question| {
            asked.push(question.to_string());
            Ok(Some(true))
        })
        .unwrap();

        assert_eq!(outcome, Outcome::Installed);
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(
            asked[0].contains(&format!("Install workmux {}", env!("CARGO_PKG_VERSION"))),
            "{asked:?}"
        );
        assert!(
            Path::new(&format!(
                "{home}/.cache/workmux/bin/{}/workmux",
                env!("CARGO_PKG_VERSION")
            ))
            .exists()
        );
        let connection = connect(&harness, &ssh.clone().with_uploaded_workmux(true)).unwrap();
        assert_eq!(connection.hello, Hello::current());
    }

    #[test]
    fn a_remote_of_another_platform_gets_the_release_for_its_platform() {
        let harness = ssh_harness!();
        isolate_remote(&harness, "");
        let releases = tempfile::tempdir().unwrap();
        let version_dir = releases
            .path()
            .join(format!("v{}", env!("CARGO_PKG_VERSION")));
        std::fs::create_dir_all(&version_dir).unwrap();
        let staged = tempfile::tempdir().unwrap();
        serving_build(staged.path());
        let artifact = format!("workmux-{}", this_platform());
        let archive = version_dir.join(format!("{artifact}.tar.gz"));
        let tar = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(staged.path())
            .arg("workmux")
            .status()
            .unwrap();
        assert!(tar.success());
        let sum = std::process::Command::new("shasum")
            .args(["-a", "256"])
            .arg(&archive)
            .output()
            .unwrap();
        std::fs::write(version_dir.join(format!("{artifact}.sha256")), sum.stdout).unwrap();
        // The workstation's own build is for a platform the remote is not.
        let builds = Builds {
            own: Some(("plan9-mips".to_string(), PathBuf::from("/nonexistent"))),
            releases: Some(format!("file://{}", releases.path().display())),
        };
        let ssh = transport(&harness);

        let outcome = Offer {
            remote: harness.host(),
            ssh: &ssh,
            answer: Some(true),
            builds: &builds,
        }
        .run(&mut never_asked)
        .unwrap();

        assert_eq!(outcome, Outcome::Installed);
        let connection = connect(&harness, &ssh.clone().with_uploaded_workmux(true)).unwrap();
        assert_eq!(connection.hello, Hello::current());
    }

    #[test]
    fn a_protocol_mismatch_installs_beside_the_users_workmux_and_leaves_it_alone() {
        let harness = ssh_harness!();
        let mismatched = format!(
            "printf '\\n{SENTINEL} {{\"workmux\":\"9.9.9\",\"protocol\":{}}}\\n'; exec cat >/dev/null",
            PROTOCOL_VERSION + 1
        );
        install_remote_workmux(&harness, "", &mismatched);
        let users = format!("{}/bin/workmux", harness.remote_dir());
        let before = std::fs::read_to_string(&users).unwrap();
        let home = format!("{}/home", harness.remote_dir());
        std::fs::create_dir_all(&home).unwrap();
        local_sshd!(harness).set_shell_startup(&format!(
            "export HOME={home}; unset XDG_CACHE_HOME; export PATH={}/bin:/usr/bin:/bin\n",
            harness.remote_dir()
        ));
        let local = tempfile::tempdir().unwrap();
        let builds = Builds {
            own: Some((this_platform(), serving_build(local.path()))),
            releases: None,
        };
        let ssh = transport(&harness);

        let outcome = Offer {
            remote: harness.host(),
            ssh: &ssh,
            answer: None,
            builds: &builds,
        }
        .run(&mut |_, _| Ok(Some(true)))
        .unwrap();

        assert_eq!(outcome, Outcome::Installed);
        assert_eq!(std::fs::read_to_string(&users).unwrap(), before);
        let connection = connect(&harness, &ssh.clone().with_uploaded_workmux(true)).unwrap();
        assert_eq!(connection.hello, Hello::current());
    }

    #[test]
    fn a_remote_whose_user_said_no_is_not_asked_again() {
        let harness = ssh_harness!();
        let home = isolate_remote(&harness, "");
        let local = tempfile::tempdir().unwrap();
        let builds = Builds {
            own: Some((this_platform(), serving_build(local.path()))),
            releases: None,
        };

        let outcome = Offer {
            remote: harness.host(),
            ssh: &transport(&harness),
            answer: Some(false),
            builds: &builds,
        }
        .run(&mut never_asked)
        .unwrap();

        assert_eq!(outcome, Outcome::Declined);
        assert!(!Path::new(&format!("{home}/.cache/workmux")).exists());
    }

    #[test]
    fn a_remote_that_serves_needs_nothing() {
        let harness = ssh_harness!();
        install_remote_workmux(&harness, "", &serve_script());
        let builds = Builds {
            own: None,
            releases: None,
        };

        let outcome = Offer {
            remote: harness.host(),
            ssh: &transport(&harness),
            answer: None,
            builds: &builds,
        }
        .run(&mut never_asked)
        .unwrap();

        assert_eq!(outcome, Outcome::NotNeeded);
    }

    #[test]
    fn a_development_build_for_another_platform_says_how_to_supply_one() {
        let builds = Builds {
            own: Some(("darwin-arm64".to_string(), PathBuf::from("/bin/workmux"))),
            releases: None,
        };

        let error = builds
            .binary_for("devbox", "linux-amd64", Path::new("/nonexistent"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("development build"), "{error}");
        assert!(error.contains("linux-amd64"), "{error}");
        assert!(
            error.contains("workmux remote upload devbox --binary"),
            "{error}"
        );
    }

    #[test]
    fn a_remote_platform_is_named_as_releases_name_it() {
        assert_eq!(platform_of("Linux x86_64\n").unwrap(), "linux-amd64");
        assert_eq!(platform_of("Linux aarch64").unwrap(), "linux-arm64");
        assert_eq!(platform_of("Darwin arm64").unwrap(), "darwin-arm64");
        assert_eq!(platform_of("Darwin x86_64").unwrap(), "darwin-amd64");
        let error = platform_of("FreeBSD amd64").unwrap_err().to_string();
        assert!(error.contains("FreeBSD amd64"), "{error}");
    }
}
