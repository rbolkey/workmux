//! What the smolvm backend needs from the host: a supported platform and a
//! recent enough `smolvm`. Checked only when the backend is selected.

use anyhow::{Result, bail};
use std::process::Command;

/// The oldest smolvm workmux supports. 1.36.0 changed how an allowlisted
/// machine reaches the host's loopback through the gateway.
pub const MIN_VERSION: (u32, u32, u32) = (1, 36, 1);

/// Where smolvm's install instructions live.
pub const INSTALL_DOCS: &str = "https://github.com/smol-machines/smolvm";

/// The host facts the platform check reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub os: &'static str,
    pub arch: &'static str,
    pub has_kvm: bool,
}

impl Host {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            has_kvm: std::path::Path::new("/dev/kvm").exists(),
        }
    }
}

/// Check the platform and smolvm's `--version` output (`None` when smolvm
/// couldn't be run).
pub fn check(host: &Host, version_output: Option<&str>) -> Result<()> {
    let supported = match host.os {
        "macos" => host.arch == "aarch64",
        "linux" => host.has_kvm,
        _ => false,
    };
    if !supported {
        bail!(
            "The smolvm sandbox backend supports macOS on Apple silicon, or Linux with KVM \
             (/dev/kvm); this host is {} on {}{}.\n  \
             Or disable sandbox: set 'sandbox.enabled: false' or choose another 'sandbox.backend'.",
            host.os,
            host.arch,
            if host.os == "linux" {
                " without /dev/kvm"
            } else {
                ""
            },
        );
    }

    let minimum = format_version(MIN_VERSION);
    let found = version_output.map(|output| (output.trim(), parse_version(output)));
    match found {
        Some((_, Some(version))) if version >= MIN_VERSION => Ok(()),
        _ => {
            let found = match found {
                None => "smolvm is not installed".to_string(),
                Some((_, Some(version))) => format!("found smolvm {}", format_version(version)),
                Some((raw, None)) => format!("could not read smolvm's version from {raw:?}"),
            };
            bail!(
                "The smolvm sandbox backend needs smolvm {minimum} or later ({found}).\n  \
                 Install or upgrade smolvm: {INSTALL_DOCS}\n  \
                 Or disable sandbox: set 'sandbox.enabled: false' or choose another 'sandbox.backend'."
            )
        }
    }
}

/// Check the current host and the `smolvm` on `PATH`.
pub fn ensure_available() -> Result<()> {
    check(&Host::current(), installed_version().as_deref())
}

/// `smolvm --version`'s output, trimmed (`None` when smolvm couldn't be
/// run).
pub fn installed_version() -> Option<String> {
    Command::new("smolvm")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `smolvm 1.36.1` → `(1, 36, 1)`; a suffix such as `-rc1` is ignored.
fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    let word = output
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = word.split('.').map(|part| {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u32>().ok()
    });
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

fn format_version((major, minor, patch): (u32, u32, u32)) -> String {
    format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Host = Host {
        os: "macos",
        arch: "aarch64",
        has_kvm: false,
    };

    fn error(host: &Host, version: Option<&str>) -> String {
        format!("{:#}", check(host, version).unwrap_err())
    }

    #[test]
    fn test_accepts_the_minimum_and_later() {
        for version in [
            "smolvm 1.36.1",
            "smolvm 1.36.2\n",
            "smolvm 1.40.0",
            "smolvm 2.0.0-rc1",
        ] {
            assert!(check(&MAC, Some(version)).is_ok(), "{version}");
        }
        let linux = Host {
            os: "linux",
            arch: "x86_64",
            has_kvm: true,
        };
        assert!(check(&linux, Some("smolvm 1.36.1")).is_ok());
    }

    #[test]
    fn test_an_old_smolvm_names_the_version_found_the_minimum_and_the_install_docs() {
        let message = error(&MAC, Some("smolvm 1.35.4"));
        assert!(message.contains("found smolvm 1.35.4"), "{message}");
        assert!(message.contains("1.36.1 or later"), "{message}");
        assert!(message.contains(INSTALL_DOCS), "{message}");
    }

    #[test]
    fn test_a_missing_smolvm_says_so_with_the_minimum_and_the_install_docs() {
        let message = error(&MAC, None);
        assert!(message.contains("smolvm is not installed"), "{message}");
        assert!(message.contains("1.36.1 or later"), "{message}");
        assert!(message.contains(INSTALL_DOCS), "{message}");
    }

    #[test]
    fn test_an_unreadable_version_is_an_error() {
        let message = error(&MAC, Some("smolvm dev"));
        assert!(message.contains("\"smolvm dev\""), "{message}");
    }

    #[test]
    fn test_unsupported_platforms_name_the_supported_ones() {
        let intel_mac = Host {
            os: "macos",
            arch: "x86_64",
            has_kvm: false,
        };
        let linux_without_kvm = Host {
            os: "linux",
            arch: "x86_64",
            has_kvm: false,
        };
        for host in [intel_mac, linux_without_kvm] {
            let message = error(&host, Some("smolvm 1.36.1"));
            assert!(
                message.contains("macOS on Apple silicon, or Linux with KVM"),
                "{message}"
            );
            assert!(message.contains(host.arch), "{message}");
        }
    }
}
