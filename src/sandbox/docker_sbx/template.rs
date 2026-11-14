//! sbx templates built from workmux's agent images.
//!
//! A template is the agent image plus the thin layer in `docker/Dockerfile.sbx`.
//! sbx keeps templates in its own image store, so the layer is built with
//! Docker, saved to a tarball and loaded with `sbx template load`.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};
use tracing::{debug, info};

/// The sbx layer added on top of the agent image.
pub const DOCKERFILE_SBX: &str = include_str!("../../../docker/Dockerfile.sbx");

/// Repository name of workmux's sbx templates.
const TEMPLATE_REPOSITORY: &str = "workmux-sbx";

/// Template tag for an agent image.
///
/// Readable from the image's last path segment, and keyed by a hash of the
/// image and the layer, so a different image or a changed layer gets its
/// own template.
pub fn template_tag(image: &str) -> String {
    let readable = image.rsplit('/').next().unwrap_or(image);
    let hash = crate::sandbox::lima::hash_key(&format!("{image}\n{DOCKERFILE_SBX}"), 8);
    let name = crate::sandbox::lima::sanitize_name(readable, 40);
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
    /// Whether this is the template `tag` (as given to `sbx create -t`).
    /// sbx lists local images under `docker.io/library/`.
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
    let output = Command::new("sbx")
        .args(["template", "ls", "--json"])
        .output()
        .context("Failed to list sbx templates")?;
    if !output.status.success() {
        bail!(
            "Failed to list sbx templates: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
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

/// Make sure the template for `image` is loaded into sbx, building it with
/// Docker when it isn't. Returns the template tag.
pub fn ensure_template(image: &str) -> Result<String> {
    let tag = template_tag(image);
    if template_loaded(&tag)? {
        debug!(%tag, "sbx template already loaded");
        return Ok(tag);
    }

    info!(%image, %tag, "building sbx template");
    let image_present = Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker; the docker_sbx backend builds its template with Docker")?
        .success();
    if !image_present {
        eprintln!("Pulling image {image}...");
        run_docker(&["pull", image])?;
    }

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
        let output = Command::new("sbx")
            .args(["template", "load", &tar_path])
            .output()
            .context("Failed to run sbx template load")?;
        if !output.status.success() {
            bail!(
                "sbx template load failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    })?;
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
