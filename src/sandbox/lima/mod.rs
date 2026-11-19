//! Lima VM backend for sandbox.
//!
//! Provides VM-based sandboxing using Lima (Linux Machines) with configurable isolation levels.

mod config;
mod instance;
pub(crate) mod log_format;
pub(crate) mod mounts;
mod wrap;

pub use config::generate_lima_config;
pub use instance::{LimaInstance, LimaInstanceInfo, ensure_vm_running, parse_lima_instances};
pub use mounts::{determine_project_root, generate_mounts};
pub use wrap::wrap_for_lima;

/// Prefix for all workmux-managed Lima VM names.
pub const VM_PREFIX: &str = "wm-";

use super::{hash_key, sanitize_name};
use crate::config::{Config, IsolationLevel};
use anyhow::Result;
use std::path::Path;
use tracing::debug;

/// Generate a unique instance name for a worktree based on isolation level.
///
/// For project isolation, the name includes the project directory name for
/// human readability: `wm-<project>-<hash8>`.
/// For shared isolation, the name is a hash of "global": `wm-<hash8>`.
pub fn instance_name(
    worktree: &Path,
    isolation: IsolationLevel,
    _config: &Config,
) -> Result<String> {
    let name = match isolation {
        IsolationLevel::Shared => {
            // Single global VM -- same format as legacy for compatibility
            let hash = hash_key("global", 8);
            format!("{}{}", VM_PREFIX, hash)
        }
        IsolationLevel::Project => {
            let project_root = determine_project_root(worktree)?;
            let canonical = project_root
                .canonicalize()
                .unwrap_or_else(|_| project_root.clone());
            let key = canonical.to_string_lossy();

            let hash = hash_key(&key, 8);

            // Extract project directory name for human-readable prefix
            // Budget: "wm-" (3) + project (up to 18) + "-" (1) + hash (8) = 30 max
            let project_dir_name = canonical
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let sanitized = sanitize_name(&project_dir_name, 18);

            if sanitized.is_empty() {
                format!("{}{}", VM_PREFIX, hash)
            } else {
                format!("{}{}-{}", VM_PREFIX, sanitized, hash)
            }
        }
    };

    debug!(isolation = ?isolation, vm_name = %name, "resolved Lima VM instance name");
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_name_basic() {
        assert_eq!(sanitize_name("workmux", 20), "workmux");
    }

    #[test]
    fn test_sanitize_name_uppercase() {
        assert_eq!(sanitize_name("MyProject", 20), "myproject");
    }

    #[test]
    fn test_sanitize_name_special_chars() {
        assert_eq!(sanitize_name("my_cool.project", 20), "my-cool-project");
    }

    #[test]
    fn test_sanitize_name_consecutive_special() {
        assert_eq!(sanitize_name("a---b___c", 20), "a-b-c");
    }

    #[test]
    fn test_sanitize_name_leading_trailing() {
        assert_eq!(sanitize_name("--project--", 20), "project");
    }

    #[test]
    fn test_sanitize_name_truncation() {
        assert_eq!(
            sanitize_name("a-very-long-project-name-here", 10),
            "a-very-lon"
        );
    }

    #[test]
    fn test_sanitize_name_truncation_strips_trailing_hyphen() {
        // "abcdefghij-rest" sanitizes to "abcdefghij-rest", truncated at 10 = "abcdefghij"
        assert_eq!(sanitize_name("abcdefghij-rest", 10), "abcdefghij");
    }

    #[test]
    fn test_sanitize_name_empty() {
        assert_eq!(sanitize_name("", 20), "");
    }

    #[test]
    fn test_sanitize_name_all_special() {
        assert_eq!(sanitize_name("___", 20), "");
    }

    #[test]
    fn test_hash_key_deterministic() {
        let a = hash_key("test", 8);
        let b = hash_key("test", 8);
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn test_hash_key_different_inputs() {
        let a = hash_key("foo", 8);
        let b = hash_key("bar", 8);
        assert_ne!(a, b);
    }
}
