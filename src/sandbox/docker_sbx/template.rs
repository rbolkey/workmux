//! sbx templates built from workmux's agent images.
//!
//! A template is the agent image plus the layer in `docker/Dockerfile.sbx`.
//! sbx keeps its own image store, so the layer is built with Docker, saved to
//! a tarball and loaded with `sbx template load`, again whenever the agent
//! image changes.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

use crate::config::SandboxRuntime;
use crate::sandbox::freshness;

/// The sbx layer added on top of the agent image.
pub const DOCKERFILE_SBX: &str = include_str!("../../../docker/Dockerfile.sbx");

/// Return `Dockerfile.sbx` without its `ARG BASE`/`FROM` lines, for
/// `workmux sandbox init-dockerfile`.
pub fn dockerfile_layer() -> String {
    DOCKERFILE_SBX
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("ARG BASE=") && !trimmed.starts_with("FROM ${BASE}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Repository name of workmux's sbx templates.
const TEMPLATE_REPOSITORY: &str = "workmux-sbx";

/// Generate the template tag for an agent image.
///
/// The hash covers the image and the layer, so changing either gets a new
/// template.
pub fn template_tag(image: &str) -> String {
    let readable = image.rsplit('/').next().unwrap_or(image);
    let hash = crate::sandbox::hash_key(&format!("{image}\n{DOCKERFILE_SBX}"), 8);
    let name = crate::sandbox::sanitize_name(readable, 40);
    if name.is_empty() {
        format!("{TEMPLATE_REPOSITORY}:{hash}")
    } else {
        format!("{TEMPLATE_REPOSITORY}:{name}-{hash}")
    }
}

/// A template from `sbx template ls --json`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SbxTemplateInfo {
    pub repository: String,
    pub tag: String,
}

impl SbxTemplateInfo {
    /// Check whether this is the template `tag`. sbx lists local images
    /// under `docker.io/library/`.
    fn is(&self, tag: &str) -> bool {
        let Some((repository, tag)) = tag.split_once(':') else {
            return false;
        };
        self.tag == tag
            && (self.repository == repository
                || self.repository == format!("docker.io/library/{repository}"))
    }
}

/// Parse `sbx template ls --json`.
pub fn parse_sbx_templates(stdout: &[u8]) -> Result<Vec<SbxTemplateInfo>> {
    serde_json::from_slice(stdout).context("Failed to parse sbx template ls output")
}

fn template_loaded(tag: &str) -> Result<bool> {
    let output = super::instance::sbx(&["template", "ls", "--json"])?;
    Ok(parse_sbx_templates(&output.stdout)?
        .iter()
        .any(|template| template.is(tag)))
}

fn run_docker(args: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .args(args)
        .status()
        .with_context(|| format!("Failed to run docker {}", args[0]))?;
    if !status.success() {
        bail!("docker {} failed", args[0]);
    }
    Ok(())
}

/// Read Docker's ID for a local image, which changes whenever the image does.
fn local_image_id(image: &str) -> Result<String> {
    let output = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()
        .context("Failed to run docker image inspect")?;
    if !output.status.success() {
        bail!(
            "docker image inspect {image} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Refresh the agent image's freshness cache in the background.
pub fn check_freshness_in_background(image: &str) {
    freshness::check_in_background(image.to_string(), SandboxRuntime::Docker);
}

/// Check whether a template is loaded and built from Docker's current image.
fn template_is_current(loaded: bool, recorded: Option<&str>, source_image_id: &str) -> bool {
    loaded && recorded == Some(source_image_id)
}

/// The image ID a loaded template was built from.
#[derive(Deserialize, Serialize)]
struct TemplateRecord {
    source_image_id: String,
}

fn template_record_path(tag: &str) -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?
        .join("docker-sbx")
        .join("templates")
        .join(format!("{}.json", tag.replace([':', '/'], "-"))))
}

/// Read the image ID recorded for a template. None if missing or corrupted.
fn read_template_record(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<TemplateRecord>(&content) {
        Ok(record) => Some(record.source_image_id),
        Err(e) => {
            warn!(?path, error = %e, "corrupted sbx template record, ignoring");
            None
        }
    }
}

fn write_template_record(path: &Path, source_image_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let record = TemplateRecord {
        source_image_id: source_image_id.to_string(),
    };
    let content = serde_json::to_string_pretty(&record)?;
    crate::util::write_atomic(path, content.as_bytes())
}

/// Check whether the template for `image` is loaded into sbx.
pub fn template_loaded_for(image: &str) -> Result<bool> {
    template_loaded(&template_tag(image))
}

/// Ensure the template for `image` is loaded and built from Docker's current
/// image, rebuilding it if needed. Returns the template tag.
pub fn ensure_template(image: &str) -> Result<String> {
    crate::sandbox::ensure_image_ready(SandboxRuntime::Docker, image)?;
    let tag = template_tag(image);
    let source_image_id = local_image_id(image)?;
    let record_path = template_record_path(&tag)?;
    let recorded = read_template_record(&record_path);
    if template_is_current(
        template_loaded(&tag)?,
        recorded.as_deref(),
        &source_image_id,
    ) {
        debug!(%tag, "sbx template already loaded");
        return Ok(tag);
    }

    info!(%image, %tag, "building sbx template");
    let mut build = Command::new("docker")
        .args(["build", "-q", "-t", &tag, "--build-arg"])
        .arg(format!("BASE={image}"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("Failed to run docker build")?;
    // `-` reads the Dockerfile from stdin, with no build context.
    build
        .stdin
        .take()
        .context("docker build has no stdin")?
        .write_all(DOCKERFILE_SBX.as_bytes())?;
    if !build.wait()?.success() {
        bail!("Failed to build sbx template {tag} from {image}");
    }

    let tar = tempfile::Builder::new()
        .prefix("workmux-sbx-template-")
        .suffix(".tar")
        .tempfile()
        .context("Failed to create template tarball")?;
    let tar_path = tar.path().to_string_lossy().into_owned();
    run_docker(&["save", &tag, "-o", &tar_path])?;

    let msg = format!("Loading sbx template {tag}");
    crate::spinner::with_spinner(&msg, || {
        super::instance::sbx(&["template", "load", &tar_path]).map(drop)
    })?;
    write_template_record(&record_path, &source_image_id)?;
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_tag_names_the_image_and_differs_per_image() {
        let claude = template_tag("ghcr.io/raine/workmux-sandbox:claude");
        assert!(
            claude.starts_with("workmux-sbx:workmux-sandbox-claude-"),
            "{claude}"
        );
        assert_ne!(claude, template_tag("ghcr.io/raine/workmux-sandbox:codex"));
        assert_eq!(claude, template_tag("ghcr.io/raine/workmux-sandbox:claude"));
    }

    #[test]
    fn dockerfile_layer_appends_to_a_combined_dockerfile_without_its_own_base() {
        let layer = dockerfile_layer();
        assert!(!layer.contains("FROM"), "{layer}");
        assert!(!layer.contains("ARG BASE"), "{layer}");
        assert!(layer.trim_start().starts_with('#'), "{layer}");
        let user = layer.find("USER root").expect("switches to root");
        assert!(user < layer.find("useradd").unwrap(), "{layer}");
        assert!(layer.contains("ENV BASH_ENV=/etc/sandbox-persistent.sh"));
        assert!(layer.trim_end().ends_with("WORKDIR /home/agent"), "{layer}");
    }

    #[test]
    fn template_is_current_only_when_loaded_from_the_local_image() {
        let pulled = "sha256:1111";
        assert!(template_is_current(true, Some(pulled), pulled));
        // A newer image was pulled (or install-dev patched it) since the load.
        assert!(!template_is_current(true, Some("sha256:0000"), pulled));
        // Loaded before workmux recorded its source, or removed from sbx.
        assert!(!template_is_current(true, None, pulled));
        assert!(!template_is_current(false, Some(pulled), pulled));
    }

    #[test]
    fn template_record_round_trips_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("templates")
            .join("workmux-sbx-claude-1.json");
        assert_eq!(read_template_record(&path), None);
        write_template_record(&path, "sha256:1111").unwrap();
        assert_eq!(read_template_record(&path).as_deref(), Some("sha256:1111"));
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_template_record(&path), None);
    }

    #[test]
    fn listed_templates_match_their_local_tag() {
        let listed = parse_sbx_templates(
            br#"[{"id":"ae8a46a10575","repository":"docker.io/library/workmux-sbx","tag":"claude-1","flavor":"","created_at":"2026-08-04T19:30:32Z","size":1},
                 {"id":"b178ca62ec2e","repository":"docker.io/docker/sandbox-templates","tag":"shell-docker","flavor":"shell-docker","created_at":"2026-10-21T14:31:20Z","size":2}]"#,
        )
        .unwrap();
        assert!(listed[0].is("workmux-sbx:claude-1"));
        assert!(!listed[0].is("workmux-sbx:claude-2"));
        assert!(!listed[1].is("workmux-sbx:shell-docker"));
    }
}
