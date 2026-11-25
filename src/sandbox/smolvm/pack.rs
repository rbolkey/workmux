//! Project packs: each project's image, `provision` and toolchain, set up
//! once in a base machine and packed with `pack create --from-vm`, so a
//! pane's machine is created from the pack in about 0.3 s.
//!
//! A pack is named after its key, a hash of everything that goes into it, so
//! a changed input names a new pack and the old one is superseded. The pack
//! index is a directory per project under workmux's state dir:
//!
//! ```text
//! <state>/smolvm-packs/<project hash>/
//!   <pack name>.smolmachine     the pack (smolvm's stub executable is removed)
//!   <pack name>.lock            held while the pack is built
//!   current/<image slot>        the name of the image's current pack
//! ```

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use super::machines::{MachineIdentity, Owner, PACK_AGENT, hex, path_hash};

use super::plan::{GuestUser, Invocation};
use crate::config::{SandboxConfig, ToolchainMode};
use crate::sandbox::toolchain::DetectedToolchain;

/// Where the base machine mounts the pack context, read-only.
const GUEST_PACK_CONTEXT_DIR: &str = "/var/lib/workmux/pack-context";
/// Where the guest user copies the pack context, writable, to warm the shell.
const GUEST_WARM_DIR: &str = "/tmp/workmux-pack-context";
/// Nix's profile in the guest, put on `PATH` for toolchain panes.
pub const NIX_PROFILE_BIN: &str = "/nix/var/nix/profiles/default/bin";
/// `XDG_CACHE_HOME` for the warm-up and toolchain panes, a directory the
/// guest user (the workstation's uid) owns, so Nix and Devbox can write it.
/// Not under `HOME` (`/tmp`), which boot wipes: Nix's lazy trees keep a
/// flake input such as nixpkgs only in Nix's cache, not the store, so the
/// warm-up's fetches must stay in the pack for a pane to reuse them.
pub const GUEST_TOOLCHAIN_CACHE: &str = "/var/cache/workmux-toolchain";

/// A project's pack, as the launch plan sees it: the pack index and the
/// inputs that aren't in the sandbox config.
#[derive(Debug, Clone)]
pub struct PackInput {
    /// `wm-pack-<project>-<hash8>`; the key's first 8 characters complete it.
    pub prefix: String,
    /// The project's pack index directory.
    pub dir: PathBuf,
    /// The packs in the index (names without `.smolmachine`).
    pub present: Vec<String>,
    /// The base machine's labels: workmux's, the project and the owner, so a
    /// base left by a killed build is swept as an orphan.
    pub base_labels: Vec<(String, String)>,
    /// The default branch's toolchain.
    pub toolchain: PackToolchain,
    pub workmux_version: String,
    /// `smolvm --version`'s output: a newer smolvm may not read an older
    /// one's pack.
    pub smolvm_version: String,
    /// Locally built images saved for smolvm (`workmux sandbox build`), as
    /// image slot ([`image_slot`]) to `docker save` archive: a pack of such
    /// an image is made from its archive, not the registry.
    pub local_images: BTreeMap<String, PathBuf>,
}

/// When a launch builds its pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackWhen {
    /// The pack is missing: build it (under the lock) before creating.
    BuildFirst,
    /// The pack is present: create from it as is.
    Present,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackPlan {
    pub build: PackBuild,
    pub when: PackWhen,
}

/// How to build a pack: `steps` in order, then `delete_base` whatever
/// happened, then the output moved into place as `sidecar`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBuild {
    /// The pack's name, also its base machine's.
    pub name: String,
    pub image: String,
    pub dir: PathBuf,
    /// The pack machines are created from.
    pub sidecar: PathBuf,
    /// `pack create -o` writes here (a stub executable, removed) and to
    /// `<output>.smolmachine`, renamed to `sidecar` once complete.
    pub output: PathBuf,
    /// Held while building, so a pack is built once.
    pub lock: PathBuf,
    /// Records this pack as current for its image (see [`current_packs`]).
    pub current_marker: PathBuf,
    pub steps: Vec<BuildStep>,
    pub delete_base: Invocation,
    /// The pack context the base machine mounts, written before the build
    /// and removed when it ends; `None` when the pack warms no shell.
    pub context: Option<PackContext>,
}

/// A pack build's [pack context](ContextSource): the source commit's
/// tracked files, extracted into `staging` (next to the pack) for the
/// build's base machine to mount read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackContext {
    pub source: ContextSource,
    pub staging: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStep {
    /// What the step does, for progress on the terminal.
    pub label: String,
    pub invocation: Invocation,
    /// Warms the pack's shell: best effort, so its failure leaves a cold
    /// pack and a warning rather than failing the build.
    pub warm_up: bool,
}

impl PackBuild {
    /// `pack create`'s sidecar, before it's moved into place.
    pub fn output_sidecar(&self) -> PathBuf {
        let mut path = self.output.clone().into_os_string();
        path.push(".smolmachine");
        PathBuf::from(path)
    }
}

/// Plan the project's pack for `agent`'s image: its name from the key, how
/// to build it, and whether to build it first. The pack gives `user` a
/// passwd entry, and `user` owns the toolchain (Nix's store) in it, as
/// panes run as the workstation's user.
/// `confine_vmm` starts the base machine confined, as panes' machines are.
pub fn plan_pack(
    sandbox: &SandboxConfig,
    agent: &str,
    input: &PackInput,
    user: GuestUser,
    confine_vmm: bool,
) -> PackPlan {
    let image = sandbox.resolved_image(agent);
    let provision = sandbox.smolvm.provision_script();
    let mode = sandbox.toolchain();
    let key = pack_key(&PackKeyInputs {
        image: &image,
        provision,
        toolchain_mode: &mode,
        toolchain: &input.toolchain,
        workmux_version: &input.workmux_version,
        smolvm_version: &input.smolvm_version,
    });
    let name = format!("{}-{}", input.prefix, &key[..8]);
    let when = if input.present.contains(&name) {
        PackWhen::Present
    } else {
        PackWhen::BuildFirst
    };

    let words = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let step = |label: &str, args: Vec<String>| BuildStep {
        label: label.to_string(),
        invocation: Invocation::new(args),
        warm_up: false,
    };
    let exec_as = |user: &str, extra: &[String], command: &str| {
        let mut args = words(&["machine", "exec", "--name", &name, "-u", user]);
        args.extend_from_slice(extra);
        args.extend(words(&["--", "sh", "-c", command]));
        args
    };

    let mut create = words(&["machine", "create", "--name", &name]);
    for (key, value) in &input.base_labels {
        create.extend(["--label".to_string(), format!("{key}={value}")]);
    }
    let source = match input.local_images.get(&image_slot(&image)) {
        Some(archive) => archive.to_string_lossy().into_owned(),
        None => image.clone(),
    };
    create.extend(["--image".to_string(), source, "--net".to_string()]);
    // Only a shell that's realised needs the context. Its mount doesn't
    // carry into the pack, and only its short guest path counts against
    // smolvm's kernel command line.
    let toolchain = &input.toolchain;
    let context = match &toolchain.source {
        Some(source)
            if toolchain.kind != DetectedToolchain::None && !toolchain.files.is_empty() =>
        {
            Some(PackContext {
                source: source.clone(),
                staging: input.dir.join(format!(".{name}.context")),
            })
        }
        _ => None,
    };
    if let Some(context) = &context {
        let mount = super::plan::Mount::new(&context.staging, GUEST_PACK_CONTEXT_DIR, true);
        create.extend(["-v".to_string(), mount.volume()]);
    }

    let mut steps = vec![
        step("create the base machine", create),
        BuildStep {
            label: "start the base machine".to_string(),
            invocation: Invocation {
                args: words(&["machine", "start", "--name", &name]),
                env: super::plan::confinement_env(confine_vmm),
            },
            warm_up: false,
        },
    ];
    if let Some(script) = provision {
        steps.push(step("run provision", exec_as("0:0", &[], script)));
    }
    // Nix as `user` needs it (see `add_user`). After provision, so a user
    // provision creates for the uid is kept rather than refused.
    steps.push(step(
        "add the guest user",
        super::plan::add_user(&name, user),
    ));
    if toolchain.kind != DetectedToolchain::None {
        let label = match toolchain.kind {
            DetectedToolchain::Devbox => "install Nix and Devbox",
            _ => "install Nix",
        };
        steps.push(step(
            label,
            exec_as("0:0", &[], &install_toolchain_script(&toolchain.kind, user)),
        ));
        if context.is_some() {
            let (label, args) = warm_up(&toolchain.kind, |extra, command| {
                exec_as(&user.to_string(), extra, command)
            });
            steps.push(BuildStep {
                warm_up: true,
                ..step(label, args)
            });
        }
    }
    let output = input.dir.join(format!(".{name}.building"));
    steps.extend([
        step(
            "stop the base machine",
            words(&["machine", "stop", "--name", &name]),
        ),
        step(
            "pack the base machine",
            vec![
                "pack".into(),
                "create".into(),
                "--from-vm".into(),
                name.clone(),
                "-o".into(),
                output.to_string_lossy().into_owned(),
            ],
        ),
    ]);

    PackPlan {
        build: PackBuild {
            sidecar: input.dir.join(format!("{name}.smolmachine")),
            lock: input.dir.join(format!("{name}.lock")),
            current_marker: input.dir.join("current").join(image_slot(&image)),
            output,
            image,
            dir: input.dir.clone(),
            steps,
            delete_base: super::plan::delete_machine(&name),
            context,
            name,
        },
        when,
    }
}

/// The step that warms the default branch's shell in the base: its label,
/// and the arguments `exec_as_user` makes of the exec's extra arguments and
/// shell command, to run it as the guest user.
fn warm_up(
    kind: &DetectedToolchain,
    exec_as_user: impl Fn(&[String], &str) -> Vec<String>,
) -> (&'static str, Vec<String>) {
    let (label, realise) = match kind {
        DetectedToolchain::Devbox => (
            "realise the default branch's devbox shell",
            "devbox install".to_string(),
        ),
        _ => (
            "realise the default branch's nix develop shell",
            format!("nix develop path:{GUEST_WARM_DIR} --command true"),
        ),
    };
    // From a writable copy (Nix may write the lock file, Devbox its
    // state), in /tmp, which boot wipes, so the copy isn't packed.
    let command = format!(
        "rm -rf {GUEST_WARM_DIR} && \
         cp -R {GUEST_PACK_CONTEXT_DIR} {GUEST_WARM_DIR} && \
         cd {GUEST_WARM_DIR} && \
         {realise}"
    );
    let env = [
        "-w".to_string(),
        "/tmp".to_string(),
        "-e".to_string(),
        "HOME=/tmp".to_string(),
        "-e".to_string(),
        format!("XDG_CACHE_HOME={GUEST_TOOLCHAIN_CACHE}"),
        "-e".to_string(),
        format!("PATH={NIX_PROFILE_BIN}:/usr/local/bin:/usr/bin:/bin"),
    ];
    (label, exec_as_user(&env, &command))
}

/// The root script that installs Nix (and Devbox) in the base and gives
/// Nix's store to `user`, as the Lima backend does for its single-user VMs.
fn install_toolchain_script(kind: &DetectedToolchain, user: GuestUser) -> String {
    let mut script = "set -e\n".to_string();
    script.push_str(&format!(
        "if [ ! -x {NIX_PROFILE_BIN}/nix ] && ! command -v nix >/dev/null 2>&1; then\n\
         \x20 curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix \\\n\
         \x20   | sh -s -- install linux --init none --no-confirm\n\
         fi\n"
    ));
    if *kind == DetectedToolchain::Devbox {
        // The installer puts a launcher in /usr/local/bin, which downloads
        // the real binary into `$HOME/.cache/devbox/bin` on first run. A
        // pane's HOME is wiped at boot, so the pack keeps the binary itself,
        // fetched by the launcher here, as Lima's provisioning does with
        // `devbox version`.
        script.push_str(
            "if ! command -v devbox >/dev/null 2>&1; then\n\
             \x20 curl -fsSL https://get.jetify.com/devbox | bash -s -- -f\n\
             \x20 launcher_home=$(mktemp -d)\n\
             \x20 devbox_bin=$(HOME=$launcher_home DEVBOX_LAUNCHER_DEBUG=1 \
             /usr/local/bin/devbox | sed -n 's/^bin=//p')\n\
             \x20 install -m 0755 \"$devbox_bin\" /usr/local/bin/devbox\n\
             \x20 rm -rf \"$launcher_home\"\n\
             fi\n",
        );
    }
    script.push_str(&format!("if [ -d /nix ]; then chown -R {user} /nix; fi\n"));
    script.push_str(&format!(
        "mkdir -p {GUEST_TOOLCHAIN_CACHE}\nchown -R {user} {GUEST_TOOLCHAIN_CACHE}\n"
    ));
    script
}

/// The current-pack marker's name for an image: a pack is current per
/// project and image (an agent's panes may use another image).
pub fn image_slot(image: &str) -> String {
    hex(&Sha256::digest(image.as_bytes())[..8])
}

/// The toolchain a pack realises: the one on the project's default branch,
/// with the files that define its shell. Each pane still enters its own
/// worktree's shell; the pack is a warm cache for the default branch's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackToolchain {
    pub kind: DetectedToolchain,
    /// The default branch's toolchain files that exist, by name: the
    /// manifest and lock files the pack key covers.
    pub files: Vec<(String, Vec<u8>)>,
    /// The commit the files were read from, whose tracked files are the
    /// pack context. Not part of the key.
    pub source: Option<ContextSource>,
}

/// The commit a pack's shell is warmed from: its tracked files are the
/// pack context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSource {
    pub repo: PathBuf,
    pub commit: String,
}

#[cfg(test)]
impl PackToolchain {
    pub fn none() -> Self {
        Self {
            kind: DetectedToolchain::None,
            files: Vec::new(),
            source: None,
        }
    }
}

/// The default branch's toolchain under `mode`, reading the default
/// branch's files through `read` (`None` for a missing file). As on the
/// panes, `devbox.json` wins over `flake.nix` under `auto`.
pub fn default_branch_toolchain(
    mode: &ToolchainMode,
    read: impl Fn(&str) -> Option<Vec<u8>>,
) -> PackToolchain {
    let kind = match mode {
        ToolchainMode::Off => DetectedToolchain::None,
        ToolchainMode::Devbox => DetectedToolchain::Devbox,
        ToolchainMode::Flake => DetectedToolchain::Flake,
        ToolchainMode::Auto => {
            if read("devbox.json").is_some() {
                DetectedToolchain::Devbox
            } else if read("flake.nix").is_some() {
                DetectedToolchain::Flake
            } else {
                DetectedToolchain::None
            }
        }
    };
    let names: &[&str] = match kind {
        DetectedToolchain::Devbox => &["devbox.json", "devbox.lock"],
        DetectedToolchain::Flake => &["flake.nix", "flake.lock"],
        DetectedToolchain::None => &[],
    };
    let files = names
        .iter()
        .filter_map(|name| read(name).map(|bytes| (name.to_string(), bytes)))
        .collect();
    PackToolchain {
        kind,
        files,
        source: None,
    }
}

/// Everything a pack is built from.
#[derive(Debug, Clone)]
pub struct PackKeyInputs<'a> {
    /// The image reference. smolvm reports an image's digest only from a
    /// started machine (`machine images`), so the key takes the reference;
    /// a tag that moves is picked up when the pack is marked stale.
    pub image: &'a str,
    pub provision: Option<&'a str>,
    pub toolchain_mode: &'a ToolchainMode,
    pub toolchain: &'a PackToolchain,
    pub workmux_version: &'a str,
    pub smolvm_version: &'a str,
}

/// The pack key: a sha256 (hex) over the pack's inputs, each field length
/// prefixed so no two sets of inputs hash the same bytes.
pub fn pack_key(inputs: &PackKeyInputs) -> String {
    let mut hasher = Sha256::new();
    let mut field = |name: &str, bytes: &[u8]| {
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field("pack-key", b"v1");
    field("image", inputs.image.as_bytes());
    field("provision", inputs.provision.unwrap_or("").as_bytes());
    field(
        "toolchain-mode",
        format!("{:?}", inputs.toolchain_mode).as_bytes(),
    );
    field(
        "toolchain",
        format!("{:?}", inputs.toolchain.kind).as_bytes(),
    );
    for (name, bytes) in &inputs.toolchain.files {
        field("file", name.as_bytes());
        field("contents", bytes);
    }
    field("workmux", inputs.workmux_version.as_bytes());
    field("smolvm", inputs.smolvm_version.as_bytes());
    hex(&hasher.finalize())
}

/// Write `context`'s staging dir afresh: its commit's tracked files, with
/// `git archive` (no submodule contents; LFS files stay pointers).
pub fn write_pack_context(context: &PackContext) -> Result<()> {
    use std::process::{Command, Stdio};
    let staging = &context.staging;
    remove_pack_context(context);
    std::fs::create_dir_all(staging)
        .with_context(|| format!("Failed to create {}", staging.display()))?;
    let ContextSource { repo, commit } = &context.source;
    let mut archive = crate::git::unattended_git(Some(repo))?
        .args(["archive", "--format=tar", commit])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to run git archive")?;
    // Read on its own, so a full stderr pipe can't stall git and tar.
    let mut git_stderr = archive.stderr.take().expect("piped stderr");
    let git_stderr = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut git_stderr, &mut bytes);
        bytes
    });
    let extract = Command::new("tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(staging)
        .stdin(archive.stdout.take().expect("piped stdout"))
        .output()
        .context("Failed to run tar")?;
    let archived = archive.wait().context("Failed to wait for git archive")?;
    let git_stderr = git_stderr.join().unwrap_or_default();
    if !archived.success() {
        anyhow::bail!(
            "git archive of {commit} in {} failed ({archived}): {}",
            repo.display(),
            String::from_utf8_lossy(&git_stderr).trim()
        );
    }
    if !extract.status.success() {
        anyhow::bail!(
            "Extracting {commit} into {} failed ({}): {}",
            staging.display(),
            extract.status,
            String::from_utf8_lossy(&extract.stderr).trim()
        );
    }
    Ok(())
}

/// Remove `context`'s staging dir, if any.
pub fn remove_pack_context(context: &PackContext) {
    if let Err(error) = std::fs::remove_dir_all(&context.staging)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %context.staging.display(), %error, "failed to remove the pack context");
    }
}

/// workmux's packs, a directory per project, in workmux's state dir.
pub fn packs_root() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("smolvm-packs"))
}

/// The project's pack input: its pack index, its base machine's labels and
/// its default branch's toolchain (read with git, without checking it out).
pub fn gather(
    sandbox: &SandboxConfig,
    project_root: &Path,
    main_branch: Option<&str>,
    owner: &Owner,
) -> Result<PackInput> {
    let identity = MachineIdentity::new(project_root, project_root, PACK_AGENT);
    let dir = packs_root()?.join(path_hash(project_root));
    let branch = match main_branch {
        Some(branch) => Some(branch.to_string()),
        None => crate::git::get_default_branch_in(Some(project_root)).ok(),
    };
    // One commit for the key's files and the pack context alike, even if
    // the branch moves meanwhile.
    let git = |args: &[&str]| {
        crate::cmd::Cmd::new("git")
            .workdir(project_root)
            .args(args)
            .run()
            .ok()
            .map(|output| output.stdout)
    };
    let commit = branch.as_deref().and_then(|branch| {
        let bytes = git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{branch}^{{commit}}"),
        ])?;
        let commit = String::from_utf8(bytes).ok()?.trim().to_string();
        (!commit.is_empty()).then_some(commit)
    });
    let read = |name: &str| git(&["show", &format!("{}:{name}", commit.as_deref()?)]);
    let mut toolchain = default_branch_toolchain(&sandbox.toolchain(), read);
    toolchain.source = commit.map(|commit| ContextSource {
        repo: project_root.to_path_buf(),
        commit,
    });
    Ok(PackInput {
        prefix: identity.pack_prefix(),
        present: present_packs(&dir),
        dir,
        base_labels: identity.pack_base_labels(owner),
        toolchain,
        workmux_version: env!("CARGO_PKG_VERSION").to_string(),
        smolvm_version: super::prerequisites::installed_version().unwrap_or_default(),
        local_images: local_images(&images_root()?),
    })
}

/// The packs in a project's index directory.
pub fn present_packs(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".smolmachine")?;
            (!stem.starts_with('.')).then(|| stem.to_string())
        })
        .collect();
    names.sort();
    names
}

/// The packs current for some image of the project.
pub fn current_packs(dir: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(dir.join("current")) else {
        return HashSet::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .map(|name| name.trim().to_string())
        .collect()
}

/// The project's packs that are no longer current for any image.
pub fn superseded_packs(dir: &Path) -> Vec<String> {
    let current = current_packs(dir);
    present_packs(dir)
        .into_iter()
        .filter(|name| !current.contains(name))
        .collect()
}

/// Delete a pack and its lock.
pub fn remove_pack(dir: &Path, name: &str) -> Result<()> {
    let sidecar = dir.join(format!("{name}.smolmachine"));
    std::fs::remove_file(&sidecar)
        .with_context(|| format!("Failed to remove {}", sidecar.display()))?;
    let _ = std::fs::remove_file(dir.join(format!("{name}.lock")));
    Ok(())
}

/// Record `build` as its image's current pack.
pub fn mark_current(build: &PackBuild) -> Result<()> {
    let parent = build
        .current_marker
        .parent()
        .context("pack marker has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;
    let temp = parent.join(format!(".{}.tmp", std::process::id()));
    std::fs::write(&temp, format!("{}\n", build.name))?;
    std::fs::rename(&temp, &build.current_marker)
        .with_context(|| format!("Failed to write {}", build.current_marker.display()))
}

/// Locally built images saved for smolvm, in workmux's state dir.
pub fn images_root() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("smolvm-images"))
}

/// The saved images under `root`, by image slot.
pub fn local_images(root: &Path) -> BTreeMap<String, PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let slot = name.strip_suffix(".tar")?;
            (!slot.starts_with('.')).then(|| (slot.to_string(), entry.path()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn branch(files: &[(&str, &str)]) -> impl Fn(&str) -> Option<Vec<u8>> {
        let files: HashMap<String, Vec<u8>> = files
            .iter()
            .map(|(name, body)| (name.to_string(), body.as_bytes().to_vec()))
            .collect();
        move |name| files.get(name).cloned()
    }

    #[test]
    fn test_the_default_branchs_toolchain_follows_the_mode() {
        let both = [
            ("devbox.json", "{}"),
            ("devbox.lock", "lock"),
            ("flake.nix", "{ }"),
        ];
        let auto = default_branch_toolchain(&ToolchainMode::Auto, branch(&both));
        assert_eq!(auto.kind, DetectedToolchain::Devbox);
        assert_eq!(
            auto.files,
            [
                ("devbox.json".to_string(), b"{}".to_vec()),
                ("devbox.lock".to_string(), b"lock".to_vec())
            ]
        );

        let flake = default_branch_toolchain(&ToolchainMode::Flake, branch(&both));
        assert_eq!(flake.kind, DetectedToolchain::Flake);
        assert_eq!(flake.files, [("flake.nix".to_string(), b"{ }".to_vec())]);

        let off = default_branch_toolchain(&ToolchainMode::Off, branch(&both));
        assert_eq!(off, PackToolchain::none());

        let bare = default_branch_toolchain(&ToolchainMode::Auto, branch(&[]));
        assert_eq!(bare, PackToolchain::none());
    }

    #[test]
    fn test_the_key_changes_with_each_input_and_only_with_them() {
        let flake = default_branch_toolchain(
            &ToolchainMode::Auto,
            branch(&[("flake.nix", "a"), ("flake.lock", "1")]),
        );
        let base = PackKeyInputs {
            image: "ghcr.io/raine/workmux-sandbox:claude",
            provision: Some("apt-get install -y tree"),
            toolchain_mode: &ToolchainMode::Auto,
            toolchain: &flake,
            workmux_version: "0.1.0",
            smolvm_version: "smolvm 1.36.1",
        };
        let key = pack_key(&base);
        assert_eq!(key.len(), 64);
        assert_eq!(pack_key(&base.clone()), key, "the key is stable");

        let relocked = default_branch_toolchain(
            &ToolchainMode::Auto,
            branch(&[("flake.nix", "a"), ("flake.lock", "2")]),
        );
        let none = PackToolchain::none();
        let changed = [
            PackKeyInputs {
                image: "alpine",
                ..base.clone()
            },
            PackKeyInputs {
                provision: None,
                ..base.clone()
            },
            PackKeyInputs {
                toolchain_mode: &ToolchainMode::Flake,
                ..base.clone()
            },
            PackKeyInputs {
                toolchain: &relocked,
                ..base.clone()
            },
            PackKeyInputs {
                toolchain: &none,
                ..base.clone()
            },
            PackKeyInputs {
                workmux_version: "0.1.1",
                ..base.clone()
            },
            PackKeyInputs {
                smolvm_version: "smolvm 1.37.0",
                ..base.clone()
            },
        ];
        for inputs in changed {
            assert_ne!(pack_key(&inputs), key, "{inputs:?}");
        }

        // Another commit with the same manifest and lock files, which
        // changed only other files of the pack context, is the same pack.
        let other_commit = PackToolchain {
            source: Some(ContextSource {
                repo: "/src/proj".into(),
                commit: "beef".into(),
            }),
            ..flake.clone()
        };
        assert_eq!(
            pack_key(&PackKeyInputs {
                toolchain: &other_commit,
                ..base.clone()
            }),
            key
        );
    }

    #[test]
    fn test_the_key_is_version_1_of_its_encoding() {
        let none = PackToolchain::none();
        let inputs = PackKeyInputs {
            image: "alpine",
            provision: None,
            toolchain_mode: &ToolchainMode::Auto,
            toolchain: &none,
            workmux_version: "1",
            smolvm_version: "smolvm 1.36.1",
        };
        // sha256 over length-prefixed fields, from "pack-key" = "v1" on,
        // computed outside workmux.
        assert_eq!(
            pack_key(&inputs),
            "4aefbd228a9ea3469c7c7081c47ff1df64a6509185bdf3f9beb2278ee131d995"
        );
    }

    #[test]
    fn test_no_provision_and_a_blank_one_are_the_same_pack() {
        let none = PackToolchain::none();
        let inputs = PackKeyInputs {
            image: "alpine",
            provision: None,
            toolchain_mode: &ToolchainMode::Auto,
            toolchain: &none,
            workmux_version: "1",
            smolvm_version: "smolvm 1.36.1",
        };
        // `provision_script()` maps a blank script to None before this.
        assert_eq!(
            pack_key(&inputs),
            pack_key(&PackKeyInputs {
                provision: Some(""),
                ..inputs.clone()
            })
        );
    }

    #[test]
    fn test_saved_images_are_found_by_their_image_slot() {
        let root = tempfile::tempdir().unwrap();
        assert!(local_images(root.path()).is_empty());
        let archive = root
            .path()
            .join(format!("{}.tar", image_slot("my-sandbox:latest")));
        std::fs::write(&archive, "tar").unwrap();
        std::fs::write(root.path().join(".partial.tar.tmp"), "").unwrap();

        let found = local_images(root.path());
        assert_eq!(
            found,
            BTreeMap::from([(image_slot("my-sandbox:latest"), archive.clone())])
        );
    }
}
