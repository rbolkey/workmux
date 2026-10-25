//! The image an OpenShell project sandbox runs, built with `docker build` on
//! the workstation before create: the project's image, or
//! workmux's default, plus a Linux workmux matching this one and, for a
//! flake project, the devshell's closure baked into `/nix`. Nothing is
//! installed or built inside the sandbox.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::agents::SandboxAgent;
use crate::remote::provision::BAKED_DEVSHELL;

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
/// workmux can tell when the project's `flake.lock` has moved on.
pub const DEVSHELL_SOURCE: &str = "/nix/devshell-source.json";

/// The build secret holding the workstation's GitHub token, for the
/// devshell bake's private flake inputs, and the variable that hands it to
/// `docker build`.
const GITHUB_TOKEN_SECRET: &str = "github-token";
const GITHUB_TOKEN_ENV: &str = "WORKMUX_GITHUB_TOKEN";

/// Runs the devshell bake's `nix develop` with the GitHub token from the
/// build secret, when there is one: nix's own access token for `github:`
/// inputs, and a git credential helper for `git+https:` ones. `git+ssh:`
/// GitHub inputs go over HTTPS, because the build has no SSH.
const WITH_GITHUB_TOKEN: &str = "#!/bin/sh
# Added by workmux: fetch the project's private flake inputs with the
# workstation's GitHub token. It is a build secret, never in the image.
export GIT_CONFIG_COUNT=1 \\
  GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf \\
  GIT_CONFIG_VALUE_0=ssh://git@github.com/
GITHUB_TOKEN=$(cat /run/secrets/github-token 2>/dev/null)
if [ -n \"$GITHUB_TOKEN\" ]; then
  export GITHUB_TOKEN NIX_CONFIG=\"access-tokens = github.com=$GITHUB_TOKEN
$NIX_CONFIG\" \\
    GIT_CONFIG_COUNT=2 \\
    GIT_CONFIG_KEY_1=credential.https://github.com.helper \\
    GIT_CONFIG_VALUE_1='!f() { test \"$1\" = get && echo username=x-access-token && echo \"password=$GITHUB_TOKEN\"; }; f'
fi
exec \"$@\"
";

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
}

/// The Dockerfile for `recipe`. Its context holds `workmux`,
/// `workmux-source`, `default-agent`, `project`, `devshell-source.json`,
/// `workmux-devshell` and `with-github-token` as the recipe needs them.
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
             COPY --chmod=0755 with-github-token /devshell/with-github-token\n\
             COPY project /devshell/project\n\
             WORKDIR /devshell/project\n\
             RUN --mount=type=secret,id={GITHUB_TOKEN_SECRET} \\\n    \
             /devshell/with-github-token nix --extra-experimental-features \"nix-command flakes\" develop --profile /devshell/profile -c true \\\n    \
             && nix --extra-experimental-features \"nix-command flakes\" print-dev-env /devshell/profile > /devshell/env.sh \\\n    \
             && mkdir /closure \\\n    \
             && cp -a $(nix-store -qR /devshell/profile) /closure/\n"
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
    if recipe.devshell.is_some() {
        out.push_str(&format!(
            "COPY --from=devshell /closure /nix/store\n\
             COPY --from=devshell /devshell/env.sh /nix/devshell-env.sh\n\
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
    /// SHA-256 of the project's `flake.lock` (empty when it has none).
    pub flake_lock_sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The `flake.lock` digest of the project checkout at `dir`.
pub fn flake_lock_digest(dir: &Path) -> String {
    std::fs::read(dir.join("flake.lock"))
        .map(|lock| sha256_hex(&lock))
        .unwrap_or_default()
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
                .with_context(|| format!("failed to read {}", binary.display()))?;
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
            bail!("the devshell checkout must be the build context's project directory");
        }
        for file in ["flake.nix", "flake.lock"] {
            digest.extend(std::fs::read(checkout.join(file)).unwrap_or_default());
        }
        let source = DevshellSource {
            clone: clone.to_string(),
            flake_lock_sha256: flake_lock_digest(checkout),
        };
        std::fs::write(
            context.join("devshell-source.json"),
            serde_json::to_vec(&source)?,
        )?;
        std::fs::write(context.join("workmux-devshell"), DEVSHELL_WRAPPER)?;
        digest.extend(WITH_GITHUB_TOKEN.as_bytes());
        std::fs::write(context.join("with-github-token"), WITH_GITHUB_TOKEN)?;
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
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "failed to list workmux's source files in {}",
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
            std::fs::read(&from).with_context(|| format!("failed to read {}", from.display()))?;
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
        .context("failed to run docker build")?;
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
        });

        assert!(!dockerfile.contains("AS base"));
        assert!(!dockerfile.contains("AS workmux"));
        assert!(dockerfile.contains("FROM docker.io/nixos/nix:2.24.10 AS devshell"));
        // Private flake inputs are fetched with the build secret.
        assert!(dockerfile.contains(
            "RUN --mount=type=secret,id=github-token \\\n    \
             /devshell/with-github-token nix --extra-experimental-features \"nix-command flakes\" develop"
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

    #[test]
    fn with_github_credentials_git_gets_the_placeholder_from_a_credential_helper() {
        let recipe = Recipe {
            base: Some("ghcr.io/me/api-dev:1".to_string()),
            workmux: Workmux::Release,
            agents: Vec::new(),
            devshell: None,
            git_credential_helper: true,
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
        assert_eq!(source.flake_lock_sha256, sha256_hex(b"v2"));
        assert_eq!(source.clone, "/sandbox/api");
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
        };
        run_recipe(&recipe, label, None, &[], argv)
    }

    /// Run `argv` as the sandbox user, with `env`, in an image built from
    /// `recipe` with `github_token`. A devshell recipe's project is the
    /// directory `project` in `context`.
    fn run_recipe(
        recipe: &Recipe,
        label: &str,
        github_token: Option<&str>,
        env: &[&str],
        argv: &[&str],
    ) -> String {
        let context = tempfile::tempdir().unwrap();
        run_recipe_in(context.path(), recipe, label, github_token, env, argv)
    }

    fn run_recipe_in(
        context: &Path,
        recipe: &Recipe,
        label: &str,
        github_token: Option<&str>,
        env: &[&str],
        argv: &[&str],
    ) -> String {
        let sandbox = format!("wm-c29-{label}-{:04x}", std::process::id() % 0x10000);
        let tag = prepare(recipe, &sandbox, "/sandbox/api", context).unwrap();
        build(Path::new("docker"), context, &tag, github_token).unwrap();
        let output = Command::new("docker")
            .args(["run", "--rm", "--user", "1000", "-e", "HOME=/tmp"])
            .args(env.iter().flat_map(|var| ["-e", var]))
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
        };
        let out = run_recipe(
            &recipe,
            "git",
            None,
            &["GITHUB_TOKEN=ghp-placeholder"],
            &[
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
    devShells = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: {{
      default = nixpkgs.legacyPackages.${{system}}.mkShellNoCC {{
        PRIVATE_INPUTS = "${{github}} ${{https}} ${{ssh}}";
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
        };

        let out = run_recipe_in(
            context.path(),
            &recipe,
            "flk",
            Some(&token),
            &[],
            &[
                crate::remote::provision::BAKED_DEVSHELL,
                "sh",
                "-c",
                "for input in $PRIVATE_INPUTS; do ls -A $input | wc -l; done; \
                 cat /nix/devshell-env.sh /etc/gitconfig",
            ],
        );

        // Each input's source is in the image; the token is nowhere in it.
        let counts: Vec<u32> = out
            .lines()
            .take(3)
            .map(|line| line.trim().parse().unwrap())
            .collect();
        assert!(counts.iter().all(|count| *count > 0), "{counts:?}");
        assert!(!out.contains(&token));
    }
}
