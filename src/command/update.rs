use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Names this build's release source: the base URL its released builds are
/// downloaded from, as `<base>/v<version>/workmux-<platform>.tar.gz`. Unset
/// until this fork publishes its own releases, so a fork build never offers
/// upstream's.
pub(crate) const RELEASES_URL_VAR: &str = "WORKMUX_RELEASES_URL";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const NOTIFY_COOLDOWN_SECS: u64 = 7 * 24 * 60 * 60;
const NOTIFY_BURST_COUNT: u64 = 3;

/// Map OS/arch to the release artifact suffix used in GitHub releases.
pub(crate) fn platform_suffix() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-amd64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => bail!("Unsupported platform: {os}/{arch}"),
    }
}

/// Check if the binary is managed by Homebrew.
fn is_homebrew_install(exe_path: &std::path::Path) -> bool {
    let path_str = exe_path.to_string_lossy();
    path_str.contains("/Cellar/") || path_str.contains("/homebrew/")
}

/// This build's release source, if one is configured.
pub(crate) fn release_source() -> Option<String> {
    std::env::var(RELEASES_URL_VAR)
        .ok()
        .filter(|base| !base.is_empty())
}

fn require_release_source(source: Option<String>) -> Result<String> {
    source.with_context(|| {
        format!(
            "Updates are not available for this build: it has no release source, so set \
             {RELEASES_URL_VAR} to where its releases are published"
        )
    })
}

/// Where a release source says which version is its latest.
#[derive(Debug, PartialEq)]
enum LatestRelease {
    /// GitHub's API for a source on GitHub releases.
    GitHub(String),
    /// A `latest` file beside the release directories, holding its tag.
    File(String),
}

fn latest_release_url(base: &str) -> LatestRelease {
    let base = base.trim_end_matches('/');
    if let Some(repo) = base
        .strip_prefix("https://github.com/")
        .and_then(|rest| rest.strip_suffix("/releases/download"))
    {
        return LatestRelease::GitHub(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ));
    }
    LatestRelease::File(format!("{base}/latest"))
}

/// Fetch the latest release version from the release source using curl.
fn fetch_latest_version(source: &str) -> Result<String> {
    fetch_latest_version_with_options(source, None, "Failed to run curl. Is curl installed?")
}

/// Fetch the latest release version using curl with timeouts.
fn fetch_latest_version_with_timeout(source: &str) -> Result<String> {
    fetch_latest_version_with_options(source, Some((5, 10)), "Failed to run curl")
}

fn fetch_latest_version_with_options(
    source: &str,
    timeout_secs: Option<(u64, u64)>,
    run_ctx: &str,
) -> Result<String> {
    let latest = latest_release_url(source);
    let url = match &latest {
        LatestRelease::GitHub(url) | LatestRelease::File(url) => url,
    };
    let mut command = Command::new("curl");
    command.arg("-sSf");

    if let Some((connect_timeout, max_time)) = timeout_secs {
        command.arg("--connect-timeout");
        command.arg(connect_timeout.to_string());
        command.arg("--max-time");
        command.arg(max_time.to_string());
    }

    let output = command
        .arg(url)
        .output()
        .with_context(|| run_ctx.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to fetch latest release: {}", stderr.trim());
    }

    match latest {
        LatestRelease::GitHub(_) => parse_latest_release_tag(&output.stdout),
        LatestRelease::File(_) => parse_latest_file(&output.stdout),
    }
}

fn parse_latest_file(payload: &[u8]) -> Result<String> {
    let tag = std::str::from_utf8(payload)
        .context("Latest release file is not text")?
        .trim();
    if tag.is_empty() {
        bail!("Latest release file is empty");
    }
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

fn parse_latest_release_tag(payload: &[u8]) -> Result<String> {
    let body: serde_json::Value =
        serde_json::from_slice(payload).context("Failed to parse GitHub API response")?;

    let tag = body["tag_name"]
        .as_str()
        .context("No tag_name in GitHub API response")?;

    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// Download a URL to a file path using curl.
pub(crate) fn download(url: &str, dest: &std::path::Path) -> Result<()> {
    let status = Command::new("curl")
        .args(["-sSLf", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context("Failed to run curl")?;

    if !status.success() {
        bail!("Download failed: {url}");
    }
    Ok(())
}

/// Extract a tar.gz archive into a directory.
pub(crate) fn extract_tar(archive: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("Failed to run tar")?;

    if !status.success() {
        bail!("Failed to extract archive");
    }
    Ok(())
}

/// Compute SHA-256 hash of a file using system tools.
fn sha256_of(path: &std::path::Path) -> Result<String> {
    // Try sha256sum first (common on Linux)
    if let Ok(output) = Command::new("sha256sum").arg(path).output()
        && output.status.success()
    {
        let out = String::from_utf8_lossy(&output.stdout);
        if let Some(hash) = out.split_whitespace().next() {
            return Ok(hash.to_string());
        }
    }

    // Fall back to shasum -a 256 (macOS)
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .context("Neither sha256sum nor shasum found. Cannot verify checksum.")?;

    if !output.status.success() {
        bail!("Checksum command failed");
    }

    let out = String::from_utf8_lossy(&output.stdout);
    out.split_whitespace()
        .next()
        .map(|s| s.to_string())
        .context("Could not parse checksum output")
}

/// Verify SHA-256 checksum of a file against the expected checksum line.
pub(crate) fn verify_checksum(file: &std::path::Path, expected_line: &str) -> Result<()> {
    // The .sha256 file format is: "<hash>  <filename>"
    let expected_hash = expected_line
        .split_whitespace()
        .next()
        .context("Invalid checksum file format")?;

    let actual_hash = sha256_of(file)?;
    if actual_hash != expected_hash {
        bail!("Checksum mismatch!\n  Expected: {expected_hash}\n  Got:      {actual_hash}");
    }
    Ok(())
}

/// Replace the current binary with the new one, with rollback on failure.
fn replace_binary(new_binary: &std::path::Path, current_exe: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let exe_dir = current_exe
        .parent()
        .context("Could not determine binary directory")?;

    // Copy to destination directory to avoid EXDEV (cross-device rename)
    let staged = exe_dir.join(".workmux.new");
    std::fs::copy(new_binary, &staged).context("Failed to copy new binary to install directory")?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;

    // Rename current -> .old, then staged -> current
    let backup = exe_dir.join(".workmux.old");
    std::fs::rename(current_exe, &backup).context("Failed to move current binary aside")?;

    if let Err(e) = std::fs::rename(&staged, current_exe) {
        // Rollback: restore the original
        let _ = std::fs::rename(&backup, current_exe);
        return Err(e).context("Failed to install new binary (rolled back)");
    }

    // Cleanup
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

fn do_update(
    pb: &indicatif::ProgressBar,
    source: &str,
    artifact_name: &str,
    current_exe: &std::path::Path,
) -> Result<String> {
    let latest_version = fetch_latest_version(source)?;

    if latest_version == CURRENT_VERSION {
        return Ok(format!("Already up to date (v{CURRENT_VERSION})"));
    }

    pb.set_message(format!("Downloading v{latest_version}..."));

    let tmp = tempfile::tempdir().context("Failed to create temp directory")?;
    let tar_path = tmp.path().join(format!("{artifact_name}.tar.gz"));
    let sha_path = tmp.path().join(format!("{artifact_name}.sha256"));

    let base_url = format!("{}/v{latest_version}", source.trim_end_matches('/'));

    download(&format!("{base_url}/{artifact_name}.tar.gz"), &tar_path)?;
    download(&format!("{base_url}/{artifact_name}.sha256"), &sha_path)?;

    pb.set_message("Verifying checksum...");
    let sha_content = std::fs::read_to_string(&sha_path).context("Failed to read checksum file")?;
    verify_checksum(&tar_path, &sha_content)?;

    pb.set_message("Installing...");
    let extract_dir = tmp.path().join("extract");
    std::fs::create_dir(&extract_dir)?;
    extract_tar(&tar_path, &extract_dir)?;

    let new_binary = extract_dir.join("workmux");
    if !new_binary.exists() {
        bail!("Extracted archive does not contain 'workmux' binary");
    }

    replace_binary(&new_binary, current_exe)?;

    Ok(format!(
        "Updated workmux v{CURRENT_VERSION} -> v{latest_version}"
    ))
}

pub fn run() -> Result<()> {
    let source = require_release_source(release_source())?;
    let current_exe =
        std::env::current_exe().context("Could not determine current executable path")?;

    // Guard: Homebrew-managed installs (canonicalize to resolve symlinks)
    let canonical_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe.clone());
    if is_homebrew_install(&canonical_exe) {
        bail!("workmux is managed by Homebrew. Run `brew upgrade workmux` instead.");
    }

    let platform = platform_suffix()?;
    let artifact_name = format!("workmux-{platform}");

    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb.set_message("Checking for updates...");

    match do_update(&pb, &source, &artifact_name, &current_exe) {
        Ok(msg) => {
            pb.finish_with_message(format!("✔ {msg}"));
            Ok(())
        }
        Err(e) => {
            pb.finish_with_message("✘ Update failed".to_string());
            Err(e)
        }
    }
}

// --- Auto-update check ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct UpdateCache {
    latest_version: Option<String>,
    /// The release source `latest_version` was found at.
    source: Option<String>,
    last_checked: Option<u64>,
    last_notified: Option<u64>,
    notify_count: Option<u64>,
}

fn update_cache_path() -> Option<std::path::PathBuf> {
    let dir = crate::xdg::cache_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("update_check.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn load_cache(path: &std::path::Path) -> UpdateCache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(path: &std::path::Path, cache: &UpdateCache) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, json);
    }
}

/// Compare two version strings as numeric tuples (e.g. "0.1.10" > "0.1.9").
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').filter_map(|s| s.parse().ok()).collect() };
    let l = parse(latest);
    let c = parse(current);
    l > c
}

/// The cached latest version, if it is newer than this build and was found
/// at `source`.
fn newer_release<'a>(cache: &'a UpdateCache, source: &str) -> Option<&'a str> {
    let latest = cache.latest_version.as_deref()?;
    (cache.source.as_deref() == Some(source) && is_newer_version(latest, CURRENT_VERSION))
        .then_some(latest)
}

/// Called on CLI startup to show an update notice if one is cached.
/// Also spawns a background check if the cache is stale.
/// Designed to be completely non-blocking and fail-silent.
pub fn check_and_notify(config: &crate::config::Config) {
    // Opt-out via config
    if config.auto_update_check == Some(false) {
        return;
    }

    // Opt-out via environment variable
    if std::env::var("WORKMUX_NO_UPDATE_CHECK").is_ok() {
        return;
    }

    // Nothing to check without a release source
    let Some(source) = release_source() else {
        return;
    };

    // Don't print notices in non-interactive contexts
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || !std::io::stderr().is_terminal() {
        return;
    }

    let cache_path = match update_cache_path() {
        Some(p) => p,
        None => return,
    };

    let mut cache = load_cache(&cache_path);
    let now = now_secs();

    // Spawn background check if cache is stale
    if now.saturating_sub(cache.last_checked.unwrap_or(0)) > CHECK_INTERVAL_SECS {
        let spawned = std::env::current_exe().ok().and_then(|exe| {
            Command::new(exe)
                .arg("_check-update")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()
        });

        // Only mark as checked if spawn succeeded to avoid silent blackout
        if spawned.is_some() {
            cache.last_checked = Some(now);
            save_cache(&cache_path, &cache);
        }
    }

    // Show notice if a newer version is available.
    // Pattern: burst of 3 consecutive notifications, then 7-day cooldown, repeat.
    if let Some(latest) = newer_release(&cache, &source).map(str::to_string) {
        let count = cache.notify_count.unwrap_or(0);
        let in_cooldown = count >= NOTIFY_BURST_COUNT
            && now.saturating_sub(cache.last_notified.unwrap_or(0)) <= NOTIFY_COOLDOWN_SECS;

        if !in_cooldown {
            // Reset burst counter after cooldown expires
            let count = if count >= NOTIFY_BURST_COUNT {
                0
            } else {
                count
            };

            let is_brew = std::env::current_exe()
                .ok()
                .and_then(|p| std::fs::canonicalize(&p).ok())
                .is_some_and(|p| is_homebrew_install(&p));

            let update_cmd = if is_brew {
                "brew upgrade workmux"
            } else {
                "workmux update"
            };

            eprintln!(
                "Update available: workmux v{CURRENT_VERSION} -> v{latest} (run `{update_cmd}`)"
            );

            cache.last_notified = Some(now);
            cache.notify_count = Some(count + 1);
            save_cache(&cache_path, &cache);
        }
    }
}

/// Hidden subcommand handler: fetch the latest version and update the cache.
pub fn run_background_check() -> Result<()> {
    let Some(source) = release_source() else {
        return Ok(());
    };
    let latest = fetch_latest_version_with_timeout(&source)?;
    let now = now_secs();

    let cache_path = update_cache_path().context("Could not determine cache path")?;
    let mut cache = load_cache(&cache_path);

    // Reset notification counter when a new version is discovered
    if cache.latest_version.as_deref() != Some(&latest) || cache.source.as_deref() != Some(&source)
    {
        cache.notify_count = Some(0);
    }

    cache.latest_version = Some(latest);
    cache.source = Some(source);
    cache.last_checked = Some(now);
    save_cache(&cache_path, &cache);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_suffix_current() {
        // Should succeed on any supported CI/dev platform
        let suffix = platform_suffix().unwrap();
        assert!(["darwin-arm64", "darwin-amd64", "linux-amd64", "linux-arm64"].contains(&suffix));
    }

    #[test]
    fn test_is_homebrew_cellar() {
        assert!(is_homebrew_install(std::path::Path::new(
            "/opt/homebrew/Cellar/workmux/0.1.124/bin/workmux"
        )));
    }

    #[test]
    fn test_is_homebrew_prefix() {
        assert!(is_homebrew_install(std::path::Path::new(
            "/usr/local/Cellar/workmux/0.1.124/bin/workmux"
        )));
    }

    #[test]
    fn test_is_not_homebrew_local_bin() {
        assert!(!is_homebrew_install(std::path::Path::new(
            "/usr/local/bin/workmux"
        )));
    }

    #[test]
    fn test_is_not_homebrew_home() {
        assert!(!is_homebrew_install(std::path::Path::new(
            "/home/user/.local/bin/workmux"
        )));
    }

    #[test]
    fn test_is_newer_version_patch() {
        assert!(is_newer_version("0.1.10", "0.1.9"));
    }

    #[test]
    fn test_is_newer_version_minor() {
        assert!(is_newer_version("0.2.0", "0.1.124"));
    }

    #[test]
    fn test_is_newer_version_major() {
        assert!(is_newer_version("1.0.0", "0.99.99"));
    }

    #[test]
    fn test_is_not_newer_same() {
        assert!(!is_newer_version("0.1.124", "0.1.124"));
    }

    #[test]
    fn test_is_not_newer_older() {
        assert!(!is_newer_version("0.1.9", "0.1.10"));
    }

    #[test]
    fn a_github_release_source_reads_latest_from_the_github_api() {
        assert_eq!(
            latest_release_url("https://github.com/someone/workmux/releases/download/"),
            LatestRelease::GitHub(
                "https://api.github.com/repos/someone/workmux/releases/latest".to_string()
            )
        );
    }

    #[test]
    fn another_release_source_names_its_latest_version_in_a_file() {
        assert_eq!(
            latest_release_url("file:///srv/workmux-releases"),
            LatestRelease::File("file:///srv/workmux-releases/latest".to_string())
        );
    }

    #[test]
    fn test_parse_latest_file_strips_v_prefix_and_whitespace() {
        assert_eq!(parse_latest_file(b"v1.2.3\n").unwrap(), "1.2.3");
    }

    #[test]
    fn updating_without_a_release_source_says_updates_are_not_available() {
        let err = require_release_source(None).unwrap_err().to_string();

        assert!(err.contains("not available for this build"), "{err}");
        assert!(err.contains("WORKMUX_RELEASES_URL"), "{err}");
    }

    #[test]
    fn a_newer_release_from_the_release_source_is_offered() {
        let cache = UpdateCache {
            latest_version: Some("99.0.0".to_string()),
            source: Some("file:///releases".to_string()),
            ..Default::default()
        };

        assert_eq!(newer_release(&cache, "file:///releases"), Some("99.0.0"));
    }

    #[test]
    fn a_release_found_at_another_source_is_not_offered() {
        // A cache written before release sources existed holds upstream's
        // latest version, with no source.
        let upstream = UpdateCache {
            latest_version: Some("99.0.0".to_string()),
            ..Default::default()
        };
        let elsewhere = UpdateCache {
            source: Some("file:///old".to_string()),
            ..upstream.clone()
        };

        assert_eq!(newer_release(&upstream, "file:///releases"), None);
        assert_eq!(newer_release(&elsewhere, "file:///releases"), None);
    }

    #[test]
    fn test_parse_latest_release_tag_strips_v_prefix() {
        let payload = br#"{"tag_name":"v1.2.3"}"#;

        let parsed = parse_latest_release_tag(payload).unwrap();

        assert_eq!(parsed, "1.2.3");
    }

    #[test]
    fn test_parse_latest_release_tag_keeps_plain_version() {
        let payload = br#"{"tag_name":"2.0.0"}"#;

        let parsed = parse_latest_release_tag(payload).unwrap();

        assert_eq!(parsed, "2.0.0");
    }
}
