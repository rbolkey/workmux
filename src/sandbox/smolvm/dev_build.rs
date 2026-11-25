//! `workmux sandbox install-dev` under smolvm: a local Linux build of
//! workmux, kept in a workmux-managed directory that each new machine mounts
//! read-only ahead of the image's workmux on `PATH`
//! ([`super::plan::LaunchInput::dev_build`]). Packs don't hold it, so
//! installing a build needs no rebuild; remove the directory to go back to
//! the image's workmux.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The managed directory: `<state>/smolvm-dev/bin`.
pub fn dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("smolvm-dev").join("bin"))
}

/// The managed directory, if it holds a build.
pub fn installed() -> Result<Option<PathBuf>> {
    Ok(installed_in(&dir()?))
}

/// `dir`, if it holds a `workmux` build.
pub fn installed_in(dir: &Path) -> Option<PathBuf> {
    dir.join("workmux").is_file().then(|| dir.to_path_buf())
}

/// Put `binary` in `dir` as `workmux`, replacing a previous build in one
/// rename, so a machine never sees half a binary.
pub fn install(binary: &Path, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let temp = dir.join(".workmux.new");
    std::fs::copy(binary, &temp).with_context(|| format!("Failed to copy {}", binary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    }
    let dest = dir.join("workmux");
    std::fs::rename(&temp, &dest)
        .with_context(|| format!("Failed to install {}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_an_installed_build_replaces_the_last_and_is_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("smolvm-dev/bin");
        assert_eq!(installed_in(&dir), None, "nothing installed yet");

        let build = tmp.path().join("target-workmux");
        std::fs::write(&build, "first").unwrap();
        install(&build, &dir).unwrap();
        std::fs::write(&build, "second").unwrap();
        let dest = install(&build, &dir).unwrap();

        assert_eq!(installed_in(&dir), Some(dir.clone()));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "second");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["workmux"], "no temporary file is left");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
}
