//! The image an OpenShell project sandbox runs, built with `docker build` on
//! the workstation before create: the project's image, or
//! workmux's default, plus a Linux workmux matching this one and, when the
//! project's toolchain is a flake, nix with the devshell's closure baked
//! into its store. The sandbox's nix is for working on the flake; panes
//! start in the baked devshell, so nothing has to be built inside the
//! sandbox.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::agents::SandboxAgent;
use crate::remote::provision::{BAKED_DEVSHELL, NIX_PROFILE_BIN};

/// Points at a Linux workmux binary to put in the image instead of the
/// one workmux would install or build.
pub const BINARY_ENV: &str = "WORKMUX_OPENSHELL_WORKMUX";

/// The builder for a dev build's Linux workmux: Alpine's Rust targets musl,
/// so the binary is static like the release's and runs on any base image.
const RUST_IMAGE: &str = "docker.io/library/rust:1.98-alpine";

/// The builder for baked devshells.
const NIX_IMAGE: &str = "docker.io/nixos/nix:2.24.10";

/// workmux's default sandbox image: git and the session holder dtach.
const DEFAULT_BASE: &str = "docker.io/library/ubuntu:24.04";

/// Where the image names remote workmux's default agent, the project's
/// first: remote workmux runs it when no config names an agent.
pub const DEFAULT_AGENT: &str = "/etc/workmux/default-agent";

/// Where the baked devshell records what it was built from, so remote
/// workmux can tell when the project's flake has moved on.
pub const DEVSHELL_SOURCE: &str = "/nix/devshell-source.json";

/// The build secret holding the workstation's GitHub token, for the
/// devshell bake's private flake inputs, and the variable that hands it to
/// `docker build`.
const GITHUB_TOKEN_SECRET: &str = "github-token";
const GITHUB_TOKEN_ENV: &str = "WORKMUX_GITHUB_TOKEN";

/// Fetches the project's flake inputs before the devshell bake builds
/// anything, private GitHub ones with the token from the build secret when
/// there is one: nix's own access token for `github:` inputs, and a git
/// credential helper reading the secret for `git+https:` ones. Only this
/// step's nix and git get the token; the build that runs the project's
/// shellHook and builders has no secret. `git+ssh:` GitHub inputs go over
/// HTTPS, because the build has no SSH.
fn fetch_flake_inputs() -> String {
    let secret = format!("/run/secrets/{GITHUB_TOKEN_SECRET}");
    format!(
        "#!/bin/sh
# Added by workmux: fetch the project's flake inputs, private ones with the
# workstation's GitHub token. It is a build secret, never in the image.
set -e
export GIT_CONFIG_COUNT=1 \\
  GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf \\
  GIT_CONFIG_VALUE_0=ssh://git@github.com/
if [ -s {secret} ]; then
  export GIT_CONFIG_COUNT=2 \\
    GIT_CONFIG_KEY_1=credential.https://github.com.helper \\
    GIT_CONFIG_VALUE_1='!f() {{ test \"$1\" = get && echo username=x-access-token && echo \"password=$(cat {secret})\"; }}; f'
  export NIX_CONFIG=\"access-tokens = github.com=$(cat {secret})
$NIX_CONFIG\"
fi
nix --extra-experimental-features \"nix-command flakes\" flake lock
exec nix --extra-experimental-features \"nix-command flakes\" flake archive
"
    )
}

/// Where the image has workmux's git credential helper.
const CREDENTIAL_HELPER: &str = "/usr/local/bin/git-credential-workmux";

/// Hands git the provider's placeholder token for github.com, which the
/// OpenShell proxy swaps for the real one. `--credential GITHUB_TOKEN`
/// sets only `GITHUB_TOKEN`, so it falls back to that.
const CREDENTIAL_HELPER_SCRIPT: &str = "#!/bin/sh
# git credential helper added by workmux: OpenShell gives the sandbox a
# placeholder token, and its proxy swaps in the real one on the way out.
cat >/dev/null
[ \"$1\" = get ] || exit 0
token=${GH_TOKEN:-$GITHUB_TOKEN}
[ -n \"$token\" ] || exit 0
printf 'username=x-access-token\\npassword=%s\\n' \"$token\"
";

/// What the image adds to `/etc/gitconfig`. SSH GitHub URLs go over HTTPS,
/// because the sandbox has no SSH; with GitHub credentials, the helper is
/// github.com's only one.
fn gitconfig(git_credential_helper: bool) -> String {
    let mut out =
        String::from("# Added by workmux: OpenShell sandboxes reach github.com over HTTPS only.\n");
    if git_credential_helper {
        out.push_str(&format!(
            "[credential \"https://github.com\"]\n\thelper =\n\thelper = {CREDENTIAL_HELPER}\n"
        ));
    }
    out.push_str(
        "[url \"https://github.com/\"]\n\tinsteadOf = git@github.com:\n\tinsteadOf = ssh://git@github.com/\n",
    );
    out
}

/// Lays out the sandbox's nix in `/store`, which the image copies to
/// `/nix`: nix and, with a baked devshell, the devshell and its flake's
/// inputs, registered in the store's database and as garbage collection
/// roots, so the sandbox's nix neither fetches them again nor collects
/// them. nix is the store's default profile, where the nix installer puts
/// it, and `/links` holds its classic commands for `/usr/local/bin`, as
/// links to the wrapper [`nix_wrapper`] that the image puts at
/// `/usr/local/bin/nix`.
const NIX_STORE_LAYOUT: &str = "#!/bin/sh
# Added by workmux: lay out the OpenShell sandbox's nix store in /store.
set -e
export NIX_CONFIG=\"experimental-features = nix-command flakes\"
store_nix=$(dirname \"$(dirname \"$(readlink -f \"$(command -v nix)\")\")\")
paths=$store_nix
devshell=
if [ -e /devshell/profile ]; then
  devshell=$(readlink -f /devshell/profile)
  # The flake's inputs, not the project's own source.
  nix flake archive --json --dry-run /devshell/project > /tmp/archive.json
  inputs=$(nix eval --impure --raw --expr '
    let
      archive = builtins.fromJSON (builtins.readFile /tmp/archive.json);
      paths = inputs: builtins.concatMap
        (name: [ inputs.${name}.path ] ++ paths (inputs.${name}.inputs or { }))
        (builtins.attrNames inputs);
    in toString (paths (archive.inputs or { }))')
  paths=\"$paths $devshell $inputs\"
fi
nix copy --no-check-sigs --to /store $paths
mkdir -p /store/nix/var/nix/profiles /store/nix/var/nix/gcroots /links
ln -s \"$store_nix\" /store/nix/var/nix/profiles/default-1-link
ln -s default-1-link /store/nix/var/nix/profiles/default
ln -s /nix/var/nix/profiles /store/nix/var/nix/gcroots/profiles
n=0
for root in $devshell $inputs; do
  n=$((n + 1))
  ln -s \"$root\" /store/nix/var/nix/gcroots/workmux-devshell-$n
done
for tool in \"$store_nix\"/bin/nix-*; do
  ln -s nix \"/links/${tool##*/}\"
done
";

/// The sandbox's `nix` and its classic commands, linked to it: the
/// store's, with two additions. The placeholder token of the provider that
/// gives GitHub credentials is nix's access token for github.com, and
/// OpenShell's proxy swaps it for the real token, so private `github:`
/// flake inputs fetch; the placeholder differs per sandbox, so it can't be
/// in `nix.conf`. And SQLite's temporary files go to `/tmp`: it would pick
/// `/var/tmp`, which the sandbox can't write, and garbage collection fails.
fn nix_wrapper() -> String {
    format!(
        "#!/bin/sh
# Added by workmux: nix with the sandbox's GitHub placeholder token, which
# OpenShell's proxy swaps for the real one, and SQLite's temporary files in
# /tmp, because the sandbox can't write /var/tmp.
export SQLITE_TMPDIR=${{SQLITE_TMPDIR:-/tmp}}
token=${{GH_TOKEN:-$GITHUB_TOKEN}}
if [ -n \"$token\" ]; then
  NIX_CONFIG=\"access-tokens = github.com=$token
$NIX_CONFIG\"
  export NIX_CONFIG
fi
exec \"{NIX_PROFILE_BIN}/${{0##*/}}\" \"$@\"
"
    )
}

/// The sandbox's nix configuration. It is in `/etc/xdg/nix/nix.conf`, a
/// user configuration file, because nix keeps the settings it turns off
/// when it thinks it is offline only when a user configuration file (or
/// `NIX_CONFIG`) sets them, not `/etc/nix/nix.conf`.
const NIX_CONF: &str = "# Added by workmux: single-user nix in an OpenShell sandbox.
experimental-features = nix-command flakes
# nix's build sandbox needs kernel namespaces the sandbox doesn't have.
sandbox = false
build-users-group =
# The sandbox's only network interface is loopback (OpenShell's proxy carries
# its traffic), so nix thinks it is offline and turns these off unless a user
# configuration file like this one sets them. These are nix's defaults.
substitute = true
tarball-ttl = 3600
download-attempts = 5
connect-timeout = 0
";

const DEVSHELL_WRAPPER: &str = "#!/bin/bash
# Run a command in the project's devshell, baked into this image by workmux:
# load the environment recorded with `nix print-dev-env`, then exec it.
source /nix/devshell-env.sh
exec \"${@:-bash}\"
";

/// Where the image's workmux comes from. Whichever it is, it lands at
/// `/usr/local/bin/workmux`, in the project's image as in the default one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workmux {
    /// A Linux workmux binary to copy in (`WORKMUX_OPENSHELL_WORKMUX`).
    Binary(PathBuf),
    /// A dev build: this workmux's source checkout, built for Linux in the
    /// image build with this build's profile.
    Source(PathBuf),
    /// The release of this workmux's version.
    Release,
}

impl Workmux {
    /// The workmux for this build: `binary` when given; else the source
    /// checkout at `source` (where this binary was compiled) when it is
    /// still a git checkout, which makes it a dev build; else the release.
    /// A release's checkout is on its build machine, and an installed
    /// crate's source is no git checkout.
    pub fn for_this_build(binary: Option<PathBuf>, source: &Path) -> Workmux {
        if let Some(binary) = binary {
            return Workmux::Binary(binary);
        }
        let checkout = source.join("Cargo.toml").is_file()
            && crate::git::unattended_git(Some(source)).is_ok_and(|mut git| {
                git.args(["rev-parse", "--is-inside-work-tree"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            });
        if checkout {
            Workmux::Source(source.to_path_buf())
        } else {
            Workmux::Release
        }
    }

    /// The workmux for the running binary.
    pub fn current() -> Workmux {
        Workmux::for_this_build(
            std::env::var_os(BINARY_ENV).map(PathBuf::from),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
    }
}

/// What goes into a project's sandbox image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// The project's image; `None` for workmux's default.
    pub base: Option<String>,
    pub workmux: Workmux,
    /// The agents the sandbox runs, by plain CLI: those workmux can
    /// install are, and the first is remote workmux's default agent.
    pub agents: Vec<String>,
    /// A checkout of the project, in the build context's `project`
    /// directory, whose flake devshell is baked in.
    pub devshell: Option<PathBuf>,
    /// Whether the image's git gets workmux's credential helper for
    /// github.com, because an attached provider gives GitHub credentials.
    pub git_credential_helper: bool,
    /// Whether the sandbox gets nix, owning a store with the baked
    /// devshell in it.
    pub nix: bool,
}

/// The Dockerfile for `recipe`. Its context holds `workmux`,
/// `workmux-source`, `default-agent`, `project`, `devshell-source.json`,
/// `workmux-devshell`, `fetch-flake-inputs`, `nix-store-layout`,
/// `nix-wrapper` and `nix.conf` as the recipe needs them.
pub fn dockerfile(recipe: &Recipe) -> String {
    let mut out = String::from("# Generated by workmux for an OpenShell project sandbox.\n");
    if recipe.base.is_none() {
        out.push_str(&format!(
            "FROM {DEFAULT_BASE} AS base\n\
             RUN apt-get update \\\n    \
             && apt-get install -y --no-install-recommends git dtach ca-certificates curl \\\n    \
             && rm -rf /var/lib/apt/lists/*\n"
        ));
    }
    match &recipe.workmux {
        Workmux::Binary(_) => {}
        Workmux::Release => out.push_str(&format!(
            "\nFROM {DEFAULT_BASE} AS workmux\n\
             RUN apt-get update \\\n    \
             && apt-get install -y --no-install-recommends ca-certificates curl \\\n    \
             && curl -fsSL https://raw.githubusercontent.com/raine/workmux/main/scripts/install.sh \\\n    \
             | WORKMUX_VERSION=v{} WORKMUX_INSTALL_DIR=/usr/local/bin bash\n",
            env!("CARGO_PKG_VERSION")
        )),
        Workmux::Source(_) => {
            let (flag, dir) = if cfg!(debug_assertions) {
                ("", "debug")
            } else {
                (" --release", "release")
            };
            out.push_str(&format!(
                "\nFROM {RUST_IMAGE} AS workmux\n\
                 RUN apk add --no-cache musl-dev\n\
                 COPY workmux-source /src\n\
                 WORKDIR /src\n\
                 RUN --mount=type=cache,id=workmux-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \\\n    \
                 --mount=type=cache,id=workmux-cargo-target,target=/target,sharing=locked \\\n    \
                 cargo build --locked{flag} --bin workmux --target-dir /target \\\n    \
                 && cp /target/{dir}/workmux /usr/local/bin/workmux\n"
            ));
        }
    }
    for agent in installed(recipe) {
        out.push('\n');
        out.push_str(&agent.stage(DEFAULT_BASE).0);
    }
    if recipe.devshell.is_some() {
        out.push_str(&format!(
            "\nFROM {NIX_IMAGE} AS devshell\n\
             COPY --chmod=0755 fetch-flake-inputs /devshell/fetch-flake-inputs\n\
             COPY project /devshell/project\n\
             WORKDIR /devshell/project\n\
             RUN --mount=type=secret,id={GITHUB_TOKEN_SECRET} /devshell/fetch-flake-inputs\n\
             RUN nix --extra-experimental-features \"nix-command flakes\" develop --profile /devshell/profile -c true \\\n    \
             && nix --extra-experimental-features \"nix-command flakes\" print-dev-env /devshell/profile > /devshell/env.sh \\\n    \
             && mkdir /closure \\\n    \
             && cp -a $(nix-store -qR /devshell/profile) /closure/\n"
        ));
    }
    if recipe.nix {
        let from = if recipe.devshell.is_some() {
            "devshell"
        } else {
            NIX_IMAGE
        };
        out.push_str(&format!(
            "\nFROM {from} AS nix\n\
             COPY --chmod=0755 nix-store-layout /devshell/nix-store-layout\n\
             RUN /devshell/nix-store-layout\n"
        ));
    }
    let base = recipe.base.as_deref().unwrap_or("base");
    // Appended to the base's own system config, in a stage of its own so
    // the image's user is left as it is.
    out.push_str(&format!(
        "\nFROM {base} AS gitconfig\n\
         USER root\n\
         COPY gitconfig /tmp/workmux-gitconfig\n\
         RUN cat /tmp/workmux-gitconfig >> /etc/gitconfig\n"
    ));
    out.push_str(&format!("\nFROM {base}\n"));
    out.push_str(match recipe.workmux {
        Workmux::Binary(_) => "COPY --chmod=0755 workmux /usr/local/bin/workmux\n",
        _ => "COPY --from=workmux /usr/local/bin/workmux /usr/local/bin/workmux\n",
    });
    for agent in installed(recipe) {
        let (_, stage) = agent.stage(DEFAULT_BASE);
        out.push_str(&format!("COPY --from={stage} {0} {0}\n", agent.path));
    }
    if !recipe.agents.is_empty() {
        out.push_str(&format!("COPY default-agent {DEFAULT_AGENT}\n"));
    }
    if recipe.nix {
        out.push_str(
            "COPY --from=nix --chown=1000:1000 /store/nix /nix\n\
             COPY --from=nix /links/ /usr/local/bin/\n\
             COPY --chmod=0755 nix-wrapper /usr/local/bin/nix\n\
             COPY nix.conf /etc/xdg/nix/nix.conf\n",
        );
    } else if recipe.devshell.is_some() {
        out.push_str("COPY --from=devshell /closure /nix/store\n");
    }
    if recipe.devshell.is_some() {
        out.push_str(&format!(
            "COPY --from=devshell /devshell/env.sh /nix/devshell-env.sh\n\
             COPY devshell-source.json {DEVSHELL_SOURCE}\n\
             COPY --chmod=0755 workmux-devshell {BAKED_DEVSHELL}\n"
        ));
    }
    out.push_str("COPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n");
    if recipe.git_credential_helper {
        out.push_str(&format!(
            "COPY --chmod=0755 git-credential-workmux {CREDENTIAL_HELPER}\n"
        ));
    }
    out
}

/// The agents in `recipe` that workmux installs.
fn installed(recipe: &Recipe) -> impl Iterator<Item = &'static SandboxAgent> + '_ {
    recipe
        .agents
        .iter()
        .filter_map(|name| SandboxAgent::named(name))
}

/// What a baked devshell was built from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DevshellSource {
    /// The project's clone in the sandbox.
    pub clone: String,
    /// [`devshell_digest`] of the project checkout the bake read.
    pub devshell_sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The digest of [`devshell_inputs`] of the project checkout at `dir`: it
/// changes whenever the devshell bake would read something else.
pub fn devshell_digest(dir: &Path) -> String {
    let mut digest = Vec::new();
    for (file, sha256) in devshell_inputs(dir) {
        digest.extend(file.into_bytes());
        digest.extend(sha256.into_bytes());
    }
    sha256_hex(&digest)
}

/// The files of the project checkout at `dir` that the devshell bake
/// reads, by path relative to `dir`, with each one's SHA-256: `flake.nix`,
/// `flake.lock`, and the files and directories the flake names by path
/// (`./nix/shell.nix`, `../tools`, `"${self}/nix"`, `self + "/nix"`),
/// followed through every `.nix` file among them. A name that does not
/// resolve to a file or directory in `dir` is skipped. So is the flake's own
/// directory (`./.`), which is the whole repository: the image follows the
/// devshell, not the project's source. Nix can read files in ways this
/// does not see (paths built from strings at evaluation time); those
/// changes need `workmux remote recreate` after a change to the flake.
pub fn devshell_inputs(dir: &Path) -> BTreeMap<String, String> {
    let mut inputs = BTreeMap::new();
    let mut pending = vec!["flake.nix".to_string(), "flake.lock".to_string()];
    let mut walked = HashSet::new();
    while let Some(name) = pending.pop() {
        if inputs.contains_key(&name) {
            continue;
        }
        let path = dir.join(&name);
        if path.is_dir() {
            // A symlink can lead back to a directory already walked.
            if path.canonicalize().is_ok_and(|real| walked.insert(real)) {
                let Ok(entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                pending.extend(
                    entries
                        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                        .filter(|entry| entry != ".git")
                        .map(|entry| format!("{name}/{entry}")),
                );
            }
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if name.ends_with(".nix") {
            let here = name.rsplit_once('/').map_or("", |(parent, _)| parent);
            let text = String::from_utf8_lossy(&bytes);
            pending.extend(named_paths(&text).into_iter().filter_map(|(base, named)| {
                resolve(if base == NamedFrom::Flake { "" } else { here }, named)
            }));
        }
        inputs.insert(name, sha256_hex(&bytes));
    }
    inputs
}

/// What a path a nix file names is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedFrom {
    /// The nix file's directory: a path literal, `./x` or `../x`.
    File,
    /// The flake's directory: `"${self}/x"` or `self + "/x"`.
    Flake,
}

/// The paths nix `text` names, as written (`./nix/shell.nix`, `nix`). It
/// reads comments and strings as code, so a path mentioned there counts
/// too: one rebuild too many is better than one too few.
fn named_paths(text: &str) -> Vec<(NamedFrom, &str)> {
    let is_path_char = |c: u8| c.is_ascii_alphanumeric() || b"._-+/".contains(&c);
    let path_at = |start: usize| {
        let len = text.as_bytes()[start..]
            .iter()
            .take_while(|&&c| is_path_char(c))
            .count();
        &text[start..start + len]
    };
    let mut named = Vec::new();
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !text.is_char_boundary(start) {
            continue;
        }
        let rest = &text[start..];
        if (rest.starts_with("./") || rest.starts_with("../"))
            && (start == 0 || !is_path_char(bytes[start - 1]))
        {
            named.push((NamedFrom::File, path_at(start)));
        }
        for prefix in ["${self}/", "self + \"/"] {
            if rest.starts_with(prefix) {
                named.push((NamedFrom::Flake, path_at(start + prefix.len())));
            }
        }
    }
    named
}

/// `named` resolved against the directory `here` (both relative to the
/// flake's directory), or `None` when it is the flake's directory itself
/// or outside it.
fn resolve(here: &str, named: &str) -> Option<String> {
    let mut parts: Vec<&str> = here.split('/').filter(|part| !part.is_empty()).collect();
    for part in named.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Lay out the build context for `recipe` in `context`, and return the
/// image tag for sandbox `sandbox`: it changes whenever what goes into the
/// image does.
pub fn prepare(recipe: &Recipe, sandbox: &str, clone: &str, context: &Path) -> Result<String> {
    let dockerfile = dockerfile(recipe);
    let mut digest = dockerfile.clone().into_bytes();
    std::fs::write(context.join("Dockerfile"), &dockerfile)?;
    if let Some(base) = &recipe.base {
        digest.extend(base.as_bytes());
    }
    match &recipe.workmux {
        Workmux::Binary(binary) => {
            let bytes = std::fs::read(binary)
                .with_context(|| format!("Failed to read {}", binary.display()))?;
            digest.extend(sha256_hex(&bytes).into_bytes());
            std::fs::write(context.join("workmux"), bytes)?;
        }
        Workmux::Source(source) => {
            copy_working_tree(source, &context.join("workmux-source"), &mut digest)?;
        }
        Workmux::Release => {}
    }
    let gitconfig = gitconfig(recipe.git_credential_helper);
    digest.extend(gitconfig.as_bytes());
    std::fs::write(context.join("gitconfig"), gitconfig)?;
    if recipe.git_credential_helper {
        digest.extend(CREDENTIAL_HELPER_SCRIPT.as_bytes());
        std::fs::write(
            context.join("git-credential-workmux"),
            CREDENTIAL_HELPER_SCRIPT,
        )?;
    }
    if let Some(agent) = recipe.agents.first() {
        std::fs::write(context.join("default-agent"), format!("{agent}\n"))?;
    }
    if let Some(checkout) = &recipe.devshell {
        if checkout != &context.join("project") {
            bail!("The devshell checkout must be the build context's project directory");
        }
        let source = DevshellSource {
            clone: clone.to_string(),
            devshell_sha256: devshell_digest(checkout),
        };
        digest.extend(source.devshell_sha256.as_bytes());
        std::fs::write(
            context.join("devshell-source.json"),
            serde_json::to_vec(&source)?,
        )?;
        std::fs::write(context.join("workmux-devshell"), DEVSHELL_WRAPPER)?;
        let fetch = fetch_flake_inputs();
        digest.extend(fetch.as_bytes());
        std::fs::write(context.join("fetch-flake-inputs"), fetch)?;
    }
    if recipe.nix {
        for (file, contents) in [
            ("nix-store-layout", NIX_STORE_LAYOUT.to_string()),
            ("nix-wrapper", nix_wrapper()),
            ("nix.conf", NIX_CONF.to_string()),
        ] {
            digest.extend(contents.as_bytes());
            std::fs::write(context.join(file), contents)?;
        }
    }
    Ok(format!(
        "workmux-openshell-{sandbox}:{}",
        &sha256_hex(&digest)[..12]
    ))
}

/// Copy the working tree of the git checkout `source` into `dest`: every
/// file git tracks or would track, edits and new files included, nothing
/// it ignores (such as `target`). Each file's path and digest go into
/// `digest`.
fn copy_working_tree(source: &Path, dest: &Path, digest: &mut Vec<u8>) -> Result<()> {
    let output = crate::git::unattended_git(Some(source))?
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stderr(Stdio::inherit())
        .output()
        .context("Failed to run git")?;
    if !output.status.success() {
        bail!(
            "Failed to list workmux's source files in {}",
            source.display()
        );
    }
    let mut files: Vec<&str> = std::str::from_utf8(&output.stdout)
        .context("workmux's source has a file name that is not UTF-8")?
        .split('\0')
        .filter(|file| !file.is_empty())
        .collect();
    files.sort_unstable();
    files.dedup();
    for file in files {
        let from = source.join(file);
        // Deleted, but not yet staged as deleted; or a submodule.
        if !from.is_file() {
            continue;
        }
        let bytes =
            std::fs::read(&from).with_context(|| format!("Failed to read {}", from.display()))?;
        digest.extend(file.as_bytes());
        digest.extend(sha256_hex(&bytes).into_bytes());
        let to = dest.join(file);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(to, bytes)?;
    }
    Ok(())
}

/// Build `tag` from the context, streaming docker's output to stderr. A
/// `github_token` reaches the devshell bake as a build secret, through
/// docker's environment: it is never on argv and never in the image.
pub fn build(docker: &Path, context: &Path, tag: &str, github_token: Option<&str>) -> Result<()> {
    let mut command = Command::new(docker);
    command
        .env("DOCKER_BUILDKIT", "1")
        .env("DOCKER_CLI_HINTS", "false")
        .arg("build");
    if let Some(token) = github_token {
        command.env(GITHUB_TOKEN_ENV, token).args([
            "--secret",
            &format!("id={GITHUB_TOKEN_SECRET},env={GITHUB_TOKEN_ENV}"),
        ]);
    }
    let status = command
        .args(["-t", tag, "."])
        .current_dir(context)
        .stdin(Stdio::null())
        .stdout(std::io::stderr())
        .status()
        .context("Failed to run docker build")?;
    if !status.success() {
        bail!("docker build of the sandbox image {tag} failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_image_has_git_dtach_and_this_workmux_release() {
        let dockerfile = dockerfile(&Recipe {
            base: None,
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: false,
            nix: false,
        });

        assert!(dockerfile.contains("FROM docker.io/library/ubuntu:24.04 AS base"));
        assert!(dockerfile.contains("git dtach"));
        assert!(dockerfile.contains(&format!("WORKMUX_VERSION=v{}", env!("CARGO_PKG_VERSION"))));
        assert!(
            dockerfile.ends_with(
                "FROM base\nCOPY --from=workmux /usr/local/bin/workmux /usr/local/bin/workmux\nCOPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n"
            ),
            "{dockerfile}"
        );
    }

    #[test]
    fn a_project_image_gets_this_workmux_release_too() {
        let dockerfile = dockerfile(&Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: false,
            nix: false,
        });

        assert!(dockerfile.contains(&format!("WORKMUX_VERSION=v{}", env!("CARGO_PKG_VERSION"))));
        assert!(
            dockerfile.ends_with(
                "FROM ghcr.io/me/api-dev:1\nCOPY --from=workmux /usr/local/bin/workmux /usr/local/bin/workmux\nCOPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n"
            ),
            "{dockerfile}"
        );
    }

    #[test]
    fn claude_code_is_installed_where_openshells_provider_profile_expects_it() {
        let dockerfile = dockerfile(&Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Release,
            agents: vec!["claude".to_string()],
            devshell: None,
            git_credential_helper: false,
            nix: false,
        });

        let stage = dockerfile
            .split("\nFROM ")
            .find(|s| s.contains("AS agent-claude"))
            .unwrap();
        assert!(
            stage.contains("curl -fsSL https://claude.ai/install.sh | bash"),
            "{stage}"
        );
        assert!(
            stage.contains("cp -L /root/.local/bin/claude /usr/local/bin/claude"),
            "{stage}"
        );
        let last_stage = dockerfile.rsplit("\nFROM ").next().unwrap();
        assert!(
            last_stage
                .contains("COPY --from=agent-claude /usr/local/bin/claude /usr/local/bin/claude\n"),
            "{last_stage}"
        );
    }

    #[test]
    fn the_projects_first_agent_is_remote_workmuxs_default_even_when_its_image_brings_it() {
        let recipe = Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Release,
            agents: vec!["gemini".to_string(), "codex".to_string()],
            devshell: None,
            git_credential_helper: false,
            nix: false,
        };
        let context = tempfile::tempdir().unwrap();

        prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();

        let dockerfile = std::fs::read_to_string(context.path().join("Dockerfile")).unwrap();
        assert!(!dockerfile.contains("agent-gemini"), "{dockerfile}");
        assert!(dockerfile.contains("AS agent-codex"), "{dockerfile}");
        assert!(
            dockerfile.contains("COPY default-agent /etc/workmux/default-agent\n"),
            "{dockerfile}"
        );
        assert_eq!(
            std::fs::read_to_string(context.path().join("default-agent")).unwrap(),
            "gemini\n"
        );
    }

    #[test]
    fn a_dev_build_is_built_from_its_source_as_a_static_linux_binary() {
        let dockerfile = dockerfile(&Recipe {
            base: None,
            workmux: Workmux::Source(PathBuf::from("/src/workmux")),
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: false,
            nix: false,
        });

        assert!(!dockerfile.contains("install.sh"), "{dockerfile}");
        let stage = dockerfile
            .split("\nFROM ")
            .find(|s| s.contains("AS workmux"))
            .unwrap();
        assert!(stage.starts_with("docker.io/library/rust:"), "{stage}");
        assert!(stage.contains("-alpine AS workmux"), "{stage}");
        assert!(stage.contains("COPY workmux-source /src"), "{stage}");
        assert!(stage.contains("cargo build --locked"), "{stage}");
        assert!(
            dockerfile.ends_with(
                "COPY --from=workmux /usr/local/bin/workmux /usr/local/bin/workmux\nCOPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n"
            )
        );
    }

    #[test]
    fn a_project_image_gets_the_given_workmux_and_the_baked_devshell() {
        let dockerfile = dockerfile(&Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Binary(PathBuf::from("/tmp/workmux")),
            agents: Vec::new(),
            devshell: Some(PathBuf::from("/tmp/ctx/project")),
            git_credential_helper: false,
            nix: false,
        });

        assert!(!dockerfile.contains("AS base"));
        assert!(!dockerfile.contains("AS workmux"));
        assert!(dockerfile.contains("FROM docker.io/nixos/nix:2.24.10 AS devshell"));
        // Private flake inputs are fetched with the build secret, in a step
        // of their own: the devshell is built without it.
        assert!(dockerfile.contains(
            "RUN --mount=type=secret,id=github-token /devshell/fetch-flake-inputs\n\
             RUN nix --extra-experimental-features \"nix-command flakes\" develop"
        ));
        let last_stage = dockerfile.rsplit("\nFROM ").next().unwrap();
        assert_eq!(
            last_stage,
            "ghcr.io/me/api-dev:1\n\
             COPY --chmod=0755 workmux /usr/local/bin/workmux\n\
             COPY --from=devshell /closure /nix/store\n\
             COPY --from=devshell /devshell/env.sh /nix/devshell-env.sh\n\
             COPY devshell-source.json /nix/devshell-source.json\n\
             COPY --chmod=0755 workmux-devshell /usr/local/bin/workmux-devshell\n\
             COPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n"
        );
    }

    /// A recipe for a project image with nix, and the devshell at
    /// `devshell`.
    fn nix_recipe(devshell: Option<PathBuf>) -> Recipe {
        Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell,
            git_credential_helper: false,
            nix: true,
        }
    }

    #[test]
    fn with_nix_the_sandbox_user_owns_a_store_with_nix_and_the_baked_devshell_registered() {
        let dockerfile = dockerfile(&nix_recipe(Some(PathBuf::from("/tmp/ctx/project"))));

        let stage = dockerfile
            .split("\nFROM ")
            .find(|s| s.contains(" AS nix\n"))
            .unwrap();
        assert_eq!(
            stage,
            "devshell AS nix\n\
             COPY --chmod=0755 nix-store-layout /devshell/nix-store-layout\n\
             RUN /devshell/nix-store-layout\n"
        );
        let last_stage = dockerfile.rsplit("\nFROM ").next().unwrap();
        assert_eq!(
            last_stage,
            "ghcr.io/me/api-dev:1\n\
             COPY --from=workmux /usr/local/bin/workmux /usr/local/bin/workmux\n\
             COPY --from=nix --chown=1000:1000 /store/nix /nix\n\
             COPY --from=nix /links/ /usr/local/bin/\n\
             COPY --chmod=0755 nix-wrapper /usr/local/bin/nix\n\
             COPY nix.conf /etc/xdg/nix/nix.conf\n\
             COPY --from=devshell /devshell/env.sh /nix/devshell-env.sh\n\
             COPY devshell-source.json /nix/devshell-source.json\n\
             COPY --chmod=0755 workmux-devshell /usr/local/bin/workmux-devshell\n\
             COPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n"
        );
    }

    #[test]
    fn with_nix_and_no_devshell_the_store_has_nix_alone() {
        let dockerfile = dockerfile(&nix_recipe(None));

        assert!(!dockerfile.contains("AS devshell"), "{dockerfile}");
        assert!(
            dockerfile.contains("\nFROM docker.io/nixos/nix:2.24.10 AS nix\n"),
            "{dockerfile}"
        );
        let last_stage = dockerfile.rsplit("\nFROM ").next().unwrap();
        assert!(
            last_stage.contains("COPY --from=nix --chown=1000:1000 /store/nix /nix\n"),
            "{last_stage}"
        );
        assert!(!last_stage.contains("devshell"), "{last_stage}");
    }

    #[test]
    fn nix_builds_without_its_sandbox_and_substitutes_although_it_sees_no_network() {
        let context = tempfile::tempdir().unwrap();

        prepare(&nix_recipe(None), "api", "/sandbox/api", context.path()).unwrap();

        let conf = std::fs::read_to_string(context.path().join("nix.conf")).unwrap();
        let settings: Vec<&str> = conf
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .collect();
        assert_eq!(
            settings,
            [
                "experimental-features = nix-command flakes",
                "sandbox = false",
                "build-users-group =",
                "substitute = true",
                "tarball-ttl = 3600",
                "download-attempts = 5",
                "connect-timeout = 0",
            ]
        );
    }

    #[test]
    fn nix_gets_the_github_placeholder_as_its_access_token_and_sqlite_a_writable_temp_dir() {
        let context = tempfile::tempdir().unwrap();
        prepare(&nix_recipe(None), "api", "/sandbox/api", context.path()).unwrap();
        // The wrapper runs a stand-in for the store's command that prints
        // its SQLITE_TMPDIR, NIX_CONFIG and arguments.
        let stand_in = context.path().join("real-nix");
        std::fs::write(
            &stand_in,
            "#!/bin/sh\nprintf '%s|' \"$SQLITE_TMPDIR\" \"$NIX_CONFIG\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &stand_in,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let wrapper = std::fs::read_to_string(context.path().join("nix-wrapper"))
            .unwrap()
            .replace(NIX_PROFILE_BIN, &context.path().display().to_string());
        // Run as the command named `real-nix`, which runs the store's.
        let nix = |env: &[(&str, &str)]| {
            let output = Command::new("sh")
                .args(["-c", &wrapper, "/usr/local/bin/real-nix", "flake", "check"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(env.iter().copied())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };

        assert_eq!(
            nix(&[("GH_TOKEN", "gh-ph"), ("GITHUB_TOKEN", "ghp-ph")]),
            "/tmp|access-tokens = github.com=gh-ph\n|flake|check|"
        );
        assert_eq!(
            nix(&[("GITHUB_TOKEN", "ghp-ph"), ("NIX_CONFIG", "cores = 2")]),
            "/tmp|access-tokens = github.com=ghp-ph\ncores = 2|flake|check|"
        );
        assert_eq!(nix(&[]), "/tmp||flake|check|");
    }

    #[test]
    fn with_github_credentials_git_gets_the_placeholder_from_a_credential_helper() {
        let recipe = Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: true,
            nix: false,
        };
        let context = tempfile::tempdir().unwrap();

        prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();

        let dockerfile = std::fs::read_to_string(context.path().join("Dockerfile")).unwrap();
        // The project image's own /etc/gitconfig is kept and added to.
        let stage = dockerfile
            .split("\nFROM ")
            .find(|s| s.contains("AS gitconfig"))
            .unwrap();
        assert!(
            stage.starts_with("ghcr.io/me/api-dev:1 AS gitconfig\nUSER root\n"),
            "{stage}"
        );
        assert!(stage.contains(">> /etc/gitconfig"), "{stage}");
        let last_stage = dockerfile.rsplit("\nFROM ").next().unwrap();
        assert!(
            last_stage.ends_with(
                "COPY --from=gitconfig /etc/gitconfig /etc/gitconfig\n\
                 COPY --chmod=0755 git-credential-workmux /usr/local/bin/git-credential-workmux\n"
            ),
            "{last_stage}"
        );
        assert_eq!(
            std::fs::read_to_string(context.path().join("gitconfig")).unwrap(),
            "# Added by workmux: OpenShell sandboxes reach github.com over HTTPS only.\n\
             [credential \"https://github.com\"]\n\
             \thelper =\n\
             \thelper = /usr/local/bin/git-credential-workmux\n\
             [url \"https://github.com/\"]\n\
             \tinsteadOf = git@github.com:\n\
             \tinsteadOf = ssh://git@github.com/\n"
        );
        let helper = context.path().join("git-credential-workmux");
        assert_eq!(
            credentials_from(
                &helper,
                "get",
                &[("GH_TOKEN", "gh-ph"), ("GITHUB_TOKEN", "ghp-ph")]
            ),
            "username=x-access-token\npassword=gh-ph\n"
        );
        assert_eq!(
            credentials_from(&helper, "get", &[("GITHUB_TOKEN", "ghp-ph")]),
            "username=x-access-token\npassword=ghp-ph\n"
        );
        assert_eq!(credentials_from(&helper, "get", &[]), "");
        assert_eq!(
            credentials_from(&helper, "store", &[("GH_TOKEN", "gh-ph")]),
            ""
        );
    }

    /// What the credential helper at `helper` answers git's `action`, with
    /// only `env` set.
    fn credentials_from(helper: &Path, action: &str, env: &[(&str, &str)]) -> String {
        use std::io::Write;
        let mut child = Command::new("sh")
            .arg(helper)
            .arg(action)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn without_github_credentials_ssh_github_urls_still_go_over_https() {
        let recipe = Recipe {
            base: None,
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: false,
            nix: false,
        };
        let context = tempfile::tempdir().unwrap();

        let without = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();

        let dockerfile = std::fs::read_to_string(context.path().join("Dockerfile")).unwrap();
        assert!(
            dockerfile.contains("\nFROM base AS gitconfig\n"),
            "{dockerfile}"
        );
        assert!(
            !dockerfile.contains("git-credential-workmux"),
            "{dockerfile}"
        );
        assert!(!context.path().join("git-credential-workmux").exists());
        assert_eq!(
            std::fs::read_to_string(context.path().join("gitconfig")).unwrap(),
            "# Added by workmux: OpenShell sandboxes reach github.com over HTTPS only.\n\
             [url \"https://github.com/\"]\n\
             \tinsteadOf = git@github.com:\n\
             \tinsteadOf = ssh://git@github.com/\n"
        );
        let context = tempfile::tempdir().unwrap();
        let with = prepare(
            &Recipe {
                git_credential_helper: true,
                ..recipe
            },
            "api",
            "/sandbox/api",
            context.path(),
        )
        .unwrap();
        assert_ne!(without, with);
    }

    #[test]
    fn the_override_wins_then_a_source_checkout_then_the_release() {
        let checkout = tempfile::tempdir().unwrap();
        git(checkout.path(), &["init", "-q"]);
        std::fs::write(checkout.path().join("Cargo.toml"), "[package]").unwrap();
        let gone = checkout.path().join("gone");

        assert_eq!(
            Workmux::for_this_build(Some(PathBuf::from("/tmp/wm")), checkout.path()),
            Workmux::Binary(PathBuf::from("/tmp/wm"))
        );
        assert_eq!(
            Workmux::for_this_build(None, checkout.path()),
            Workmux::Source(checkout.path().to_path_buf())
        );
        // A release's build machine path, or a crate's unpacked source.
        assert_eq!(Workmux::for_this_build(None, &gone), Workmux::Release);
        let unpacked = tempfile::tempdir().unwrap();
        std::fs::write(unpacked.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(
            Workmux::for_this_build(None, unpacked.path()),
            Workmux::Release
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_dev_build_carries_its_working_tree_and_the_tag_follows_it() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        std::fs::write(source.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(source.path().join("src")).unwrap();
        std::fs::create_dir_all(source.path().join("target")).unwrap();
        std::fs::write(source.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(source.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(source.path().join("target/junk"), "built").unwrap();
        git(source.path(), &["add", "."]);
        // Uncommitted and untracked work is part of the build.
        std::fs::write(source.path().join("src/new.rs"), "// new").unwrap();
        let recipe = Recipe {
            base: None,
            workmux: Workmux::Source(source.path().to_path_buf()),
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: false,
            nix: false,
        };
        let context = tempfile::tempdir().unwrap();

        let first = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();
        let carried = context.path().join("workmux-source");
        assert_eq!(
            std::fs::read_to_string(carried.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        assert!(carried.join("src/new.rs").exists());
        assert!(!carried.join("target").exists());

        let context = tempfile::tempdir().unwrap();
        let again = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();
        std::fs::write(source.path().join("src/main.rs"), "fn main() { 1; }").unwrap();
        let context = tempfile::tempdir().unwrap();
        let edited = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();
        assert_eq!(first, again);
        assert_ne!(first, edited);
    }

    #[test]
    fn the_tag_follows_the_flake_lock() {
        let context = tempfile::tempdir().unwrap();
        let project = context.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("flake.nix"), "{}").unwrap();
        std::fs::write(project.join("flake.lock"), "v1").unwrap();
        let recipe = Recipe {
            base: None,
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: Some(project.clone()),
            git_credential_helper: false,
            nix: false,
        };

        let first = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();
        let again = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();
        std::fs::write(project.join("flake.lock"), "v2").unwrap();
        let bumped = prepare(&recipe, "api", "/sandbox/api", context.path()).unwrap();

        assert!(first.starts_with("workmux-openshell-api:"), "{first}");
        assert_eq!(first, again);
        assert_ne!(first, bumped);
        let source: DevshellSource = serde_json::from_slice(
            &std::fs::read(context.path().join("devshell-source.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(source.devshell_sha256, devshell_digest(&project));
        assert_eq!(source.clone, "/sandbox/api");
    }

    /// A project checkout with a flake in the build context, and its tag.
    struct FlakeProject {
        context: tempfile::TempDir,
    }

    impl FlakeProject {
        fn new(flake: &str) -> FlakeProject {
            let project = FlakeProject {
                context: tempfile::tempdir().unwrap(),
            };
            project.write("flake.nix", flake);
            project.write("flake.lock", "lock");
            project
        }

        fn write(&self, file: &str, contents: &str) {
            let path = self.context.path().join("project").join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn tag(&self) -> String {
            let recipe = Recipe {
                base: None,
                workmux: Workmux::Release,
                agents: Vec::new(),
                devshell: Some(self.context.path().join("project")),
                git_credential_helper: false,
                nix: false,
            };
            prepare(&recipe, "api", "/sandbox/api", self.context.path()).unwrap()
        }
    }

    #[test]
    fn the_tag_follows_flake_nix() {
        let project = FlakeProject::new("{ outputs = _: { }; }");
        let first = project.tag();
        project.write("flake.nix", "{ outputs = _: { x = 1; }; }");
        assert_ne!(first, project.tag());
    }

    #[test]
    fn the_tag_follows_the_files_the_flake_imports() {
        let project = FlakeProject::new(
            "{ outputs = _: { shell = import ./nix/shell.nix; \
               tools = builtins.readFile ./nix/tools; }; }",
        );
        project.write("nix/shell.nix", "import ../recipes/active.nix");
        project.write("nix/tools", "jq");
        project.write("recipes/active.nix", "[ ]");
        let first = project.tag();
        assert_eq!(first, project.tag());

        let mut tags = vec![first];
        for (file, contents) in [
            ("nix/shell.nix", "import ../recipes/active.nix // { }"),
            ("nix/tools", "jq rg"),
            // Imported by an imported file.
            ("recipes/active.nix", "[ \"rg\" ]"),
        ] {
            project.write(file, contents);
            let tag = project.tag();
            assert!(!tags.contains(&tag), "{file}: {tag}");
            tags.push(tag);
        }
    }

    #[test]
    fn the_tag_follows_directories_and_paths_from_self() {
        let project = FlakeProject::new(
            "{ outputs = { self }: { a = import ./nix; \
               b = import \"${self}/lib/b.nix\"; c = import (self + \"/lib/c.nix\"); }; }",
        );
        project.write("nix/default.nix", "{ }");
        project.write("nix/deep/tools", "jq");
        project.write("lib/b.nix", "1");
        project.write("lib/c.nix", "2");
        let mut tags = vec![project.tag()];
        for (file, contents) in [
            ("nix/default.nix", "{ x = 1; }"),
            ("nix/deep/tools", "jq rg"),
            ("nix/new", "added"),
            ("lib/b.nix", "3"),
            ("lib/c.nix", "4"),
        ] {
            project.write(file, contents);
            let tag = project.tag();
            assert!(!tags.contains(&tag), "{file}: {tag}");
            tags.push(tag);
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_tag_follows_a_symlinked_import_to_where_it_points() {
        let project = FlakeProject::new("{ outputs = _: import ./recipes/active/toolchain.nix; }");
        project.write("recipes/go/toolchain.nix", "[ \"go\" ]");
        project.write("recipes/python/toolchain.nix", "[ \"python3\" ]");
        let active = project.context.path().join("project/recipes/active");
        std::os::unix::fs::symlink("go", &active).unwrap();
        let go = project.tag();
        std::fs::remove_file(&active).unwrap();
        std::os::unix::fs::symlink("python", &active).unwrap();
        assert_ne!(go, project.tag());
    }

    #[test]
    fn the_tag_ignores_the_projects_other_files() {
        // `./.` is the whole repository; `../outside` is not in it.
        let project = FlakeProject::new(
            "{ outputs = _: { src = ./.; up = ../outside; shell = import ./nix/shell.nix; }; }",
        );
        project.write("nix/shell.nix", "{ }");
        project.write("README.md", "v1");
        project.write("src/main.rs", "fn main() {}");
        project.write(".git/HEAD", "ref: refs/heads/main");
        let first = project.tag();
        project.write("README.md", "v2");
        project.write("src/main.rs", "fn main() { 1; }");
        project.write("src/new.rs", "");
        project.write(".git/HEAD", "ref: refs/heads/other");
        assert_eq!(first, project.tag());
    }
}

/// Env-gated like the OpenShell contract tests (`WORKMUX_TEST_OPENSHELL=1`),
/// but needing only Docker: the image's workmux is this workmux's version,
/// whether it comes from the release or from this dev build's source.
#[cfg(test)]
mod contract {
    use super::*;

    fn version_in_image(workmux: Workmux, label: &str) -> String {
        run_in_image(workmux, &[], label, &["workmux", "--version"])
    }

    /// Run `argv` as the sandbox user in an image built from the recipe.
    fn run_in_image(workmux: Workmux, agents: &[&str], label: &str, argv: &[&str]) -> String {
        let recipe = Recipe {
            base: None,
            workmux,
            agents: agents.iter().map(|a| a.to_string()).collect(),
            devshell: None,
            git_credential_helper: false,
            nix: false,
        };
        let context = tempfile::tempdir().unwrap();
        run_recipe(context.path(), &recipe, label, None, argv)
    }

    /// Run `argv` as the sandbox user in an image built in `context` from
    /// `recipe` with `github_token`. A devshell recipe's project is the
    /// directory `project` in `context`.
    fn run_recipe(
        context: &Path,
        recipe: &Recipe,
        label: &str,
        github_token: Option<&str>,
        argv: &[&str],
    ) -> String {
        let sandbox = format!("wm-c29-{label}-{:04x}", std::process::id() % 0x10000);
        let tag = prepare(recipe, &sandbox, "/sandbox/api", context).unwrap();
        build(Path::new("docker"), context, &tag, github_token).unwrap();
        let output = Command::new("docker")
            .args(["run", "--rm", "--user", "1000", "-e", "HOME=/tmp"])
            .arg(&tag)
            .args(argv)
            .output()
            .unwrap();
        let _ = Command::new("docker")
            .args(["rmi", "-f", &tag])
            .stdout(Stdio::null())
            .status();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn enabled() -> bool {
        std::env::var("WORKMUX_TEST_OPENSHELL").as_deref() == Ok("1")
    }

    #[test]
    fn the_release_recipe_installs_this_version() {
        if !enabled() {
            return;
        }
        let version = version_in_image(Workmux::Release, "rel");
        assert!(version.ends_with(env!("CARGO_PKG_VERSION")), "{version}");
    }

    #[test]
    fn a_dev_build_builds_itself_for_linux() {
        if !enabled() {
            return;
        }
        let workmux = Workmux::for_this_build(None, Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(matches!(workmux, Workmux::Source(_)), "{workmux:?}");
        let version = version_in_image(workmux, "dev");
        assert!(version.ends_with(env!("CARGO_PKG_VERSION")), "{version}");
    }

    #[test]
    fn the_agents_are_installed_and_run_and_the_first_is_the_default() {
        if !enabled() {
            return;
        }
        let out = run_in_image(
            Workmux::Release,
            &["claude", "codex"],
            "agt",
            &[
                "sh",
                "-c",
                "claude --version && codex --version && cat /etc/workmux/default-agent",
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("Claude Code"), "{out}");
        assert!(lines[1].starts_with("codex"), "{out}");
        assert_eq!(lines[2], "claude", "{out}");
    }

    /// An image whose `/etc/gitconfig` already names a credential helper
    /// for github.com, as `gh auth setup-git` leaves it.
    const IMAGE_WITH_GH_HELPER: &str = "wm-trellis-base:latest";

    #[test]
    fn git_in_the_image_gets_the_placeholder_and_rewrites_ssh_github_urls() {
        if !enabled() {
            return;
        }
        let has_base = Command::new("docker")
            .args(["image", "inspect", IMAGE_WITH_GH_HELPER])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let base = has_base.then(|| IMAGE_WITH_GH_HELPER.to_string());
        let recipe = Recipe {
            base,
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: true,
            nix: false,
        };
        let context = tempfile::tempdir().unwrap();
        let out = run_recipe(
            context.path(),
            &recipe,
            "git",
            None,
            &[
                "env",
                "GITHUB_TOKEN=ghp-placeholder",
                "sh",
                "-c",
                "printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill \
                 && git ls-remote --get-url git@github.com:me/api.git \
                 && git ls-remote --get-url ssh://git@github.com/me/api.git",
            ],
        );
        assert_eq!(
            out,
            "protocol=https\n\
             host=github.com\n\
             username=x-access-token\n\
             password=ghp-placeholder\n\
             https://github.com/me/api.git\n\
             https://github.com/me/api.git",
            "base: {:?}",
            recipe.base
        );
    }

    /// A private GitHub repository (`owner/repo`) the workstation's gh login
    /// can read, for the private flake inputs test.
    const PRIVATE_REPO_ENV: &str = "WORKMUX_TEST_PRIVATE_FLAKE_REPO";

    #[test]
    fn the_devshell_bake_fetches_private_flake_inputs_with_the_gh_login() {
        let Ok(repo) = std::env::var(PRIVATE_REPO_ENV) else {
            return;
        };
        if !enabled() {
            return;
        }
        let token = crate::remote::openshell::github::login_token(Path::new("gh")).unwrap();
        let context = tempfile::tempdir().unwrap();
        let project = context.path().join("project");
        std::fs::create_dir(&project).unwrap();
        // The fixture's nixpkgs; the bake locks the private inputs itself.
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/openshell/devshell/flake.lock"
            ),
            project.join("flake.lock"),
        )
        .unwrap();
        std::fs::write(
            project.join("flake.nix"),
            format!(
                r#"{{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
  inputs.github = {{ url = "github:{repo}"; flake = false; }};
  inputs.https = {{ url = "git+https://github.com/{repo}"; flake = false; }};
  inputs.ssh = {{ url = "git+ssh://git@github.com/{repo}"; flake = false; }};
  outputs = {{ nixpkgs, github, https, ssh, ... }}: {{
    devShells = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = nixpkgs.legacyPackages.${{system}};
        # A builder recording what it can see: its environment and the
        # bake's secret.
        seen = derivation {{
          name = "seen";
          inherit system;
          builder = "${{pkgs.bash}}/bin/bash";
          args = [ "-c" "{{ export -p; read -r line < /run/secrets/{GITHUB_TOKEN_SECRET} && echo \"$line\"; }} > $out 2>&1; true" ];
        }};
      in {{
        default = pkgs.mkShellNoCC {{
          PRIVATE_INPUTS = "${{github}} ${{https}} ${{ssh}}";
          SEEN = "${{seen}}";
          # The bake runs the shellHook: it must find no token.
          shellHook = ''
            if [ -e /run/secrets/{GITHUB_TOKEN_SECRET} ] \
              || env -u shellHook | grep -qE 'access-tokens|GITHUB_TOKEN|GH_TOKEN|x-access-token'; then
              echo "the shellHook can see the GitHub token" >&2
              exit 1
            fi
          '';
        }};
      }});
  }};
}}
"#
            ),
        )
        .unwrap();
        let recipe = Recipe {
            base: None,
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: Some(project),
            git_credential_helper: false,
            nix: false,
        };

        let out = run_recipe(
            context.path(),
            &recipe,
            "flk",
            Some(&token),
            &[
                crate::remote::provision::BAKED_DEVSHELL,
                "sh",
                "-c",
                "for input in $PRIVATE_INPUTS; do ls -A $input | wc -l; done; \
                 cat $SEEN /nix/devshell-env.sh /etc/gitconfig",
            ],
        );

        // Each input's source is in the image; the token is nowhere in it,
        // nor in what the devshell's builder saw.
        let counts: Vec<u32> = out
            .lines()
            .take(3)
            .map(|line| line.trim().parse().unwrap())
            .collect();
        assert!(counts.iter().all(|count| *count > 0), "{counts:?}");
        // The builder ran, and saw no token.
        assert!(out.contains("declare -x"), "{out}");
        assert!(!out.contains(&token));
    }
}
