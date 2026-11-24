//! Masks: files mounted read-only over others to hide them from the agent.
//!
//! A device can't be an sbx mount source, so unlike the container backend's
//! `/dev/null` mounts, each mask's source is a file in the state directory.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Directory in a sandbox's state directory holding its masks' sources.
const MASKS_DIR: &str = "masks";

/// A file mounted read-only over `target`, holding `contents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    /// The guest path hidden.
    pub target: PathBuf,
    /// What the agent reads at `target`.
    pub contents: String,
}

impl Mask {
    /// Create a mask that leaves `target` empty.
    pub fn empty(target: PathBuf) -> Self {
        Self {
            target,
            contents: String::new(),
        }
    }

    /// Return the mask's source file in the sandbox's state directory.
    pub fn source(&self, state_dir: &Path) -> PathBuf {
        let key = crate::sandbox::hash_key(&self.target.to_string_lossy(), 16);
        state_dir.join(MASKS_DIR).join(key)
    }

    /// Format the workspace that mounts the mask: `SOURCE:TARGET:ro`.
    pub fn workspace(&self, state_dir: &Path) -> String {
        format!(
            "{}:{}:ro",
            self.source(state_dir).display(),
            self.target.display()
        )
    }
}

/// Write the masks' sources, replacing any an earlier sandbox left.
pub fn write_masks(state_dir: &Path, masks: &[Mask]) -> Result<()> {
    let dir = state_dir.join(MASKS_DIR);
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("Failed to remove {}", dir.display()));
        }
        _ => {}
    }
    if masks.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    for mask in masks {
        let source = mask.source(state_dir);
        std::fs::write(&source, &mask.contents)
            .with_context(|| format!("Failed to write {}", source.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_sources_are_written_and_earlier_ones_removed() {
        let dir = tempfile::tempdir().unwrap();
        let old = Mask::empty(PathBuf::from("/code/wt/.env.old"));
        write_masks(dir.path(), std::slice::from_ref(&old)).unwrap();
        assert!(old.source(dir.path()).is_file());

        let env = Mask::empty(PathBuf::from("/code/wt/.env"));
        let placeholder = Mask {
            target: PathBuf::from("/home/agent/.claude/.credentials.json"),
            contents: "{}".to_string(),
        };
        write_masks(dir.path(), &[env.clone(), placeholder.clone()]).unwrap();

        assert_eq!(std::fs::read_to_string(env.source(dir.path())).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(placeholder.source(dir.path())).unwrap(),
            "{}"
        );
        assert_ne!(env.source(dir.path()), placeholder.source(dir.path()));
        assert!(!old.source(dir.path()).exists());
    }
}
