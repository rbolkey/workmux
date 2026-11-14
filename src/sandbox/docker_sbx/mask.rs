//! Masks: files the agent reads in place of others.
//!
//! sbx mounts a file read-only over another (`SOURCE:TARGET:ro`), which
//! hides the target from the agent, as the container backend's `/dev/null`
//! mounts do. A device can't be an sbx source, so each mask's source is a
//! file workmux writes into the sandbox's state directory before creating
//! the sandbox. Masks are fixed when the sandbox is created.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Directory in a sandbox's state directory holding its masks' sources.
const MASKS_DIR: &str = "masks";

/// A file mounted read-only over `target`, so the agent reads `contents`
/// there instead of the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mask {
    /// The guest path hidden.
    pub target: PathBuf,
    /// What the agent reads at `target`.
    pub contents: String,
}

impl Mask {
    /// A mask that leaves `target` empty.
    pub fn empty(target: PathBuf) -> Self {
        Self {
            target,
            contents: String::new(),
        }
    }

    /// The mask's source file in the sandbox's state directory, one per
    /// target.
    pub fn source(&self, state_dir: &Path) -> PathBuf {
        let key = crate::sandbox::lima::hash_key(&self.target.to_string_lossy(), 16);
        state_dir.join(MASKS_DIR).join(key)
    }

    /// The workspace that mounts the mask: `SOURCE:TARGET:ro`.
    pub fn workspace(&self, state_dir: &Path) -> String {
        format!(
            "{}:{}:ro",
            self.source(state_dir).display(),
            self.target.display()
        )
    }
}

/// Write the masks' sources into the sandbox's state directory before the
/// sandbox is created, replacing any an earlier sandbox left.
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
