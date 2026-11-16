//! The launch plan: what a pane launch asks of sbx.
//!
//! [`launch_plan`] is pure, but for checking whether a writable extra mount
//! is a file and which excluded files exist. Callers gather sbx's state (see
//! [`super::instance::gather_state`]), compute the plan, run its setup
//! commands, then run its exec in the foreground.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::config::{DockerSbxAgent, SandboxConfig};
use crate::sandbox::network_proxy::domain_matches;
use crate::sandbox::toolchain::{self, DetectedToolchain};
use crate::shell::shell_quote;

use super::instance::{SbxCheck, SbxRule, SbxSandboxInfo};
use super::mask::Mask;

/// The host sbx runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `std::env::consts::OS`
    pub os: String,
    /// `std::env::consts::ARCH`
    pub arch: String,
    /// Whether `/dev/kvm` exists (Linux only)
    pub kvm: bool,
}

/// What sbx reports about a worktree's sandbox, as parsed data.
#[derive(Debug, Clone)]
pub struct SbxState {
    pub platform: Platform,
    /// Checks from `sbx diagnose --json`.
    pub checks: Vec<SbxCheck>,
    /// The sandbox from `sbx ls --json`, or `None` when it doesn't exist.
    pub sandbox: Option<SbxSandboxInfo>,
    /// Rules from `sbx policy ls <name> --json`, or the global ones from
    /// `sbx policy ls --json` when the sandbox is missing.
    pub rules: Vec<SbxRule>,
}

/// The worktree a launch is for, with the host paths the plan mounts.
#[derive(Debug, Clone)]
pub struct LaunchTarget<'a> {
    /// The worktree's sandbox name (see [`super::sandbox_name`]).
    pub sandbox_name: &'a str,
    pub worktree_root: &'a Path,
    /// The pane's working directory, under `worktree_root`.
    pub pane_cwd: &'a Path,
    /// The git metadata the worktree uses. Finding it walks the worktree,
    /// so it's gathered only when the launch creates the sandbox (see
    /// [`creates_sandbox`]), which is all that mounts it.
    pub git: Option<&'a GitMetadata>,
    /// OpenCode's global config directory (`~/.config/opencode/`), when it
    /// exists; mounted read-only for the opencode agent.
    pub opencode_config_dir: Option<&'a Path>,
    /// Host-side state directory for the sandbox (shims live under it).
    pub state_dir: &'a Path,
    /// The network rules workmux last wrote for the sandbox (see
    /// [`LaunchPlan::network_rules`]).
    pub network_record: &'a [NetworkRule],
    /// The toolchain the worktree uses (see
    /// [`toolchain::resolve_toolchain`]): installed on create, its endpoints
    /// allowed (under `deny`, only those `allowed_domains` lists), and the
    /// agent command run in it.
    pub toolchain: &'a DetectedToolchain,
    /// What the sandbox's state directory records of how it was created.
    pub created_with: &'a CreateRecord,
}

/// A sandbox's create record, as read from its state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRecord {
    /// No launch finished creating the sandbox: it's missing, or a launch
    /// failed or was killed partway.
    Missing,
    Recorded(Box<CreateSettings>),
    /// The record doesn't parse; holds why.
    Unreadable(String),
}

/// What a sandbox is created with and later launches can't change, as
/// recorded in its state directory once it's fully set up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSettings {
    pub agent: String,
    /// `sandbox.docker_sbx.agent`.
    pub agent_mode: DockerSbxAgent,
    /// The agent's config directory, mounted at its usual place.
    pub agent_config_dir: Option<PathBuf>,
    /// OpenCode's global config directory, mounted read-only.
    pub opencode_config_dir: Option<PathBuf>,
    /// `sandbox.extra_mounts`, as workspaces.
    pub extra_mounts: Vec<String>,
    /// The files `sandbox.docker_sbx.excluded_files` masked.
    pub excluded_files: Vec<PathBuf>,
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    pub kits: Vec<String>,
    /// The template tag, which follows `sandbox.image`.
    pub template: String,
    /// The toolchain installed by built-in provisioning: `devbox`, `flake`
    /// or `none`.
    pub toolchain: String,
}

/// Whether an sbx rule allows or denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleDecision {
    Allow,
    Deny,
}

impl RuleDecision {
    /// sbx's name for the decision, as in `sbx policy allow|deny`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// A network rule workmux keeps on a sandbox for `sandbox.network`:
/// `sbx policy <decision> network --sandbox <name> <resource>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkRule {
    pub decision: RuleDecision,
    pub resource: String,
}

/// A linked worktree's git metadata, as found on the host (see
/// [`super::git::git_metadata`]).
#[derive(Debug, Clone, Default)]
pub struct GitMetadata {
    /// The main repository's git dir.
    pub common_dir: PathBuf,
    /// The worktree's own admin dir, `<common_dir>/worktrees/<name>`.
    pub admin_dir: PathBuf,
    /// `.git` pointer files under the worktree: its own and its submodules'.
    pub pointer_files: Vec<PathBuf>,
    /// Submodule git dirs under `<common_dir>/modules`.
    pub module_roots: Vec<PathBuf>,
    /// Files the repository config includes or runs, inside the worktree or
    /// the git dir.
    pub config_paths: Vec<PathBuf>,
    /// Whether `<common_dir>/rr-cache` exists.
    pub rr_cache: bool,
}

/// The command and environment to run in the sandbox.
#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    /// The command: one argument is a shell command line, as a pane's is;
    /// several are its words, each passed on as is.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Identifies the exec's processes in the guest, so they can be hung up.
    pub session: String,
    /// The port of the supervisor's RPC server on the workstation's
    /// loopback, when it runs one.
    pub rpc_port: Option<u16>,
    /// Whether stdin is a terminal. sbx's `exec -t` fails without one.
    pub tty: bool,
    /// Whether the command runs in the worktree's toolchain environment, as
    /// the supervisor runs the agent command (on Lima too); `sandbox shell`
    /// runs its command without it.
    pub in_toolchain: bool,
}

/// One `sbx` invocation. Environment values are passed through the process
/// environment and named with a bare `-e KEY`, so they never appear in argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbxCommand {
    /// Arguments after `sbx`.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Creating a missing sandbox.
#[derive(Debug, Clone)]
pub struct CreateStep {
    /// The `sbx create` command, from the template.
    pub command: SbxCommand,
    /// The workspaces `command` mounts, for showing the user.
    pub workspaces: Vec<String>,
}

/// The ordered sbx commands for one launch.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub sandbox_name: String,
    /// Deletes a sandbox no launch finished setting up, before `create`
    /// creates it again.
    pub remove: Option<SbxCommand>,
    /// Creates the sandbox, when it's missing.
    pub create: Option<CreateStep>,
    /// Files mounted over others when the plan creates the sandbox
    /// (`excluded_files`), whose sources must be written (see
    /// [`super::mask::write_masks`]) before `create` runs.
    pub masks: Vec<Mask>,
    /// Commands to run, in order, after `create` and before `exec`.
    pub setup: Vec<SbxCommand>,
    /// workmux's built-in provisioning, run after `setup` when the plan
    /// creates the sandbox, unless `skip_default_provision` is set: login
    /// shells keep the shims on `PATH`, and the toolchain is installed.
    pub default_provision: Vec<SbxCommand>,
    /// The custom provision script, run as root after `setup` when the plan
    /// creates the sandbox, so the sandbox's network rules apply to it.
    pub provision: Option<SbxCommand>,
    /// The interactive exec that runs the command.
    pub exec: SbxCommand,
    /// Hangs up the exec's processes in the guest. sbx leaves them running
    /// when the `sbx exec` client goes away, as it does when a pane closes.
    pub hangup: SbxCommand,
    /// Commands to run after `exec` ends: removing the RPC rule.
    pub teardown: Vec<SbxCommand>,
    /// Notes for the log: settings ignored or that sbx can't enforce that
    /// only limit access, and toolchain endpoints `deny` leaves blocked.
    pub notes: Vec<String>,
    /// Warnings for the log: checks `sbx diagnose` fails that don't stop
    /// the launch, what sbx lets a `deny` sandbox reach beyond
    /// `allowed_domains`, and create-time settings changed since the
    /// sandbox was created.
    pub warnings: Vec<String>,
    /// The `sandbox.network` and toolchain rules workmux has written for the
    /// sandbox once `setup` has run; the record the next launch reconciles
    /// against.
    pub network_rules: Vec<NetworkRule>,
    /// The settings to record once a sandbox the plan creates is fully set
    /// up and provisioned.
    pub create_settings: CreateSettings,
}

/// Home directory of sbx's `agent` user in the guest.
const GUEST_HOME: &str = "/home/agent";

/// Where Nix puts its own binaries in the guest.
const NIX_PROFILE_BIN: &str = "/nix/var/nix/profiles/default/bin";

/// Environment variable marking the processes of one exec in the guest.
const SESSION_ENV: &str = "WM_SBX_SESSION";

/// Git config sbx includes at command scope in the guest.
const SBX_GIT_CONFIG: &str = "/home/agent/.gitconfig_ignore_scope";

/// Hosts installing and using Nix and Devbox reach: the Nix installer and
/// binary caches, Devbox's installer, binary (a GitHub release) and package
/// search, and GitHub, which serves nixpkgs.
const TOOLCHAIN_HOSTS: &[&str] = &[
    "install.determinate.systems",
    "cache.nixos.org",
    "get.jetify.com",
    "get.jetpack.io",
    "releases.jetify.com",
    "releases.jetpack.io",
    "www.nixsearch.com",
    "github.com",
    "api.github.com",
    "codeload.github.com",
    "release-assets.githubusercontent.com",
];

/// sbx's name for the workstation in policy rules.
const WORKSTATION_HOST: &str = "localhost";

/// Where OpenCode's global config directory goes under the guest home.
const OPENCODE_CONFIG_SUBPATH: &str = ".config/opencode";

/// The subdirectory of an agent's config directory it downloads binaries
/// into, and the guest-local directory that takes its place.
fn guest_binaries_dir(agent: &str) -> Option<(&'static str, &'static str)> {
    match agent {
        "pi" => Some(("bin", "/var/lib/workmux/pi-agent-bin")),
        "omp" => Some(("tools", "/var/lib/workmux/omp-agent-tools")),
        _ => None,
    }
}

/// The agent's host config directory and where it's linked under the guest
/// home, for agents whose config directory workmux mounts.
fn config_dir_and_home_subpath(
    config: &SandboxConfig,
    agent: &str,
) -> Option<(PathBuf, &'static str)> {
    let subpath = crate::sandbox::agent_config_guest_subpath(agent)?;
    Some((config.resolved_agent_config_dir(agent)?, subpath))
}

/// An sbx agent kit, which `agent: kit` creates an agent's sandbox for.
/// sbx has none for the other agent types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kit {
    Claude,
    Codex,
    Copilot,
    Gemini,
    OpenCode,
}

impl Kit {
    /// The kit for the agent type `agent`.
    fn for_agent(agent: &str) -> Result<Self> {
        Ok(match agent {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "copilot" => Self::Copilot,
            "gemini" => Self::Gemini,
            "opencode" => Self::OpenCode,
            _ => bail!(
                "sbx has no agent kit for {agent}; sandbox.docker_sbx.agent: kit supports \
                 claude, codex, copilot, gemini and opencode. Set \
                 `sandbox.docker_sbx.agent: shell` in your global config to run {agent} with \
                 its config directory mounted instead."
            ),
        })
    }

    /// sbx's name for the kit, as `sbx create` takes it.
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
        }
    }

    /// Whether the agent's binary must come from a custom `sandbox.image`:
    /// the copilot kit configures Copilot CLI, and workmux publishes no
    /// Copilot image.
    fn needs_custom_image(self) -> bool {
        matches!(self, Self::Copilot)
    }

    /// Installs workmux's status hooks for the kit's agent into the config
    /// the kit wrote in sandbox `name`'s own home, as `workmux setup` would
    /// into the workstation's.
    fn status_hooks(self, name: &str) -> Result<Vec<SbxCommand>> {
        use crate::agent_setup::{claude, codex, copilot, gemini, opencode};
        Ok(match self {
            Self::Claude => vec![agent_script(
                name,
                CLAUDE_KIT_HOOKS_SCRIPT,
                vec![serde_json::to_string(&claude::load_hooks_from_plugin()?)?],
            )],
            Self::Codex => vec![
                agent_script(
                    name,
                    JSON_HOOKS_SCRIPT,
                    vec![
                        ".codex/hooks.json".to_string(),
                        serde_json::to_string(&codex::load_hooks()?)?,
                    ],
                ),
                agent_script(name, CODEX_KIT_CONFIG_SCRIPT, Vec::new()),
            ],
            Self::Gemini => vec![agent_script(
                name,
                JSON_HOOKS_SCRIPT,
                vec![
                    ".gemini/settings.json".to_string(),
                    serde_json::to_string(&gemini::load_hooks()?)?,
                ],
            )],
            Self::Copilot => vec![agent_script(
                name,
                WRITE_FILE_SCRIPT,
                vec![
                    ".copilot/hooks/workmux-status.json".to_string(),
                    copilot::HOOKS_JSON.to_string(),
                ],
            )],
            Self::OpenCode => {
                let mut sources = vec![opencode::PLUGIN_SOURCE.to_string()];
                for file in ["index.ts", "tui.ts", "status.ts"] {
                    let (_, source) = opencode::V2_FILES
                        .iter()
                        .find(|(name, _)| *name == file)
                        .context("OpenCode plugin file missing")?;
                    sources.push(source.to_string());
                }
                vec![agent_script(name, OPENCODE_KIT_PLUGIN_SCRIPT, sources)]
            }
        })
    }
}

/// The kit an agent's sandbox is created for under `agent: kit`, failing
/// for what the kit can't serve.
fn kit_for(config: &SandboxConfig, agent: &str) -> Result<Kit> {
    let kit = Kit::for_agent(agent)?;
    if kit.needs_custom_image() && config.image.is_none() {
        bail!(
            "workmux has no sandbox image with GitHub Copilot CLI. Set `sandbox.image` to an \
             image that provides `copilot` (built FROM {}:base) to use the copilot kit.",
            crate::sandbox::DEFAULT_IMAGE_REGISTRY
        );
    }
    Ok(kit)
}

/// Under `agent: kit`, a note that `sandbox.agent_config_dir` is ignored:
/// it applies to `agent: shell` and the other backends, which is why it's
/// often set globally, while the kit sets the agent up in the sandbox's own
/// home.
fn ignored_config_dir_note(config: &SandboxConfig, kit: Kit) -> Option<String> {
    config.agent_config_dir.as_ref().map(|_| {
        format!(
            "sandbox.agent_config_dir is ignored under sandbox.docker_sbx.agent: kit: sbx's \
             {} kit sets the agent up in the sandbox's own home.",
            kit.name()
        )
    })
}

/// Merges workmux's status hook groups (`$1`) into the claude kit's
/// `~/.claude/settings.json` in the guest, beside the kit's own. The kit's
/// hooks and status line call `/usr/local/bin/sbx-claude-hook`, which comes
/// with sbx's own claude image, not workmux's template; where it's missing
/// they're dropped, as each would fail on every prompt.
const CLAUDE_KIT_HOOKS_SCRIPT: &str = r#"f="$HOME/.claude/settings.json"
helper=false
[ -x /usr/local/bin/sbx-claude-hook ] && helper=true
[ -s "$f" ] || { mkdir -p "${f%/*}" && echo '{}' > "$f"; }
jq --argjson hooks "$1" --argjson helper "$helper" '
def kit: (.command? // "") | contains("sbx-claude-hook");
(if $helper then . else
  (if (.statusLine | kit) then del(.statusLine) else . end)
  | if .hooks then .hooks |= (map_values(map(.hooks |= map(select(kit | not))
      | select(.hooks | length > 0))) | with_entries(select(.value | length > 0)))
    else . end
end)
| .hooks = reduce ($hooks | to_entries[]) as $e (.hooks // {}; .[$e.key] += $e.value)
' "$f" > "$f.tmp" && mv "$f.tmp" "$f""#;

/// Merges status hook groups (`$2`, by event) into the JSON config at
/// `$HOME/$1` in the guest, after the groups already there.
const JSON_HOOKS_SCRIPT: &str = r#"f="$HOME/$1"
[ -s "$f" ] || { mkdir -p "${f%/*}" && echo '{}' > "$f"; }
jq --argjson hooks "$2" '
.hooks = reduce ($hooks | to_entries[]) as $e (.hooks // {}; .[$e.key] += $e.value)
' "$f" > "$f.tmp" && mv "$f.tmp" "$f""#;

/// Turns Codex's hooks feature on in the codex kit's
/// `~/.codex/config.toml`, without which it doesn't read `hooks.json`. The
/// kit's `notify` calls `/usr/local/bin/sbx-codex-hook`, which comes with
/// sbx's own codex image, not workmux's template; where it's missing it's
/// dropped, as it would fail at the end of every turn.
const CODEX_KIT_CONFIG_SCRIPT: &str = r#"f="$HOME/.codex/config.toml"
mkdir -p "${f%/*}" && touch "$f"
helper=0
[ -x /usr/local/bin/sbx-codex-hook ] && helper=1
awk -v helper="$helper" '
/^\[/ { features = ($0 ~ /^\[features\][ \t]*$/) }
/^\[features\][ \t]*$/ { print; print "hooks = true"; done = 1; next }
features && /^[ \t]*hooks[ \t]*=/ { next }
!helper && /^notify[ \t]*=.*sbx-codex-hook/ { next }
{ print }
END { if (!done) printf "\n[features]\nhooks = true\n" }
' "$f" > "$f.tmp" && mv "$f.tmp" "$f""#;

/// Writes `$2` to `$HOME/$1` in the guest.
const WRITE_FILE_SCRIPT: &str = r#"f="$HOME/$1"
mkdir -p "${f%/*}" && printf %s "$2" > "$f""#;

/// Writes workmux's OpenCode plugin into the opencode kit's
/// `~/.config/opencode/plugins` in the layout the guest's OpenCode reads:
/// `$1` for OpenCode 1, `$2`–`$4` (`index.ts`, `tui.ts`, `status.ts`) for
/// OpenCode 2.
const OPENCODE_KIT_PLUGIN_SCRIPT: &str = r#"d="$HOME/.config/opencode/plugins"
v=$(opencode --version 2>/dev/null | awk '{ print $NF }')
case "${v#v}" in
1.*) mkdir -p "$d" && printf %s "$1" > "$d/workmux-status.ts" ;;
2.*) mkdir -p "$d/workmux-status" && printf %s "$2" > "$d/workmux-status/index.ts" \
  && printf %s "$3" > "$d/workmux-status/tui.ts" \
  && printf %s "$4" > "$d/workmux-status/status.ts" ;;
*) echo "workmux can't install its status plugin for OpenCode version '$v'" >&2; exit 1 ;;
esac"#;

/// An agent-user `sh -c <script>` in the sandbox, with `script_args` as
/// its `$1`….
fn agent_script(name: &str, script: &str, script_args: Vec<String>) -> SbxCommand {
    let mut args = words(&["exec", name, "sh", "-c", script, "sh"]);
    args.extend(script_args);
    SbxCommand {
        args,
        env: Vec::new(),
    }
}

/// A command run as root in the sandbox.
fn root_exec(name: &str, command: Vec<String>) -> SbxCommand {
    let mut args = words(&["exec", "-u", "root", name]);
    args.extend(command);
    SbxCommand {
        args,
        env: Vec::new(),
    }
}

/// Make the agent own the parents of `subpaths` under the guest home.
fn own_home_parents(name: &str, subpaths: &[&str]) -> Option<SbxCommand> {
    let mut parents: Vec<String> = Vec::new();
    for subpath in subpaths {
        let mut ancestors: Vec<String> = Path::new(subpath)
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| format!("{GUEST_HOME}/{}", p.display()))
            .collect();
        ancestors.reverse();
        for parent in ancestors {
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
    }
    if parents.is_empty() {
        return None;
    }
    let mut command = words(&["install", "-d", "-o", "agent", "-g", "agent"]);
    command.extend(parents);
    Some(root_exec(name, command))
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// The interactive `sbx exec` that runs the command in the pane's working
/// directory, with a terminal when stdin is one. Values travel in the
/// process environment; argv names them only.
fn exec_command(target: &LaunchTarget, exec: &ExecRequest) -> SbxCommand {
    let mut env = exec.env.clone();
    keep_sbx_git_config(&mut env);
    env.push((SESSION_ENV.to_string(), exec.session.clone()));
    let interactive = if exec.tty { "-it" } else { "-i" };
    let mut args = words(&["exec", interactive, "-w"]);
    args.push(target.pane_cwd.to_string_lossy().into_owned());
    for (key, _) in &env {
        args.push("-e".to_string());
        args.push(key.clone());
    }
    args.push(target.sandbox_name.to_string());
    // bash, so BASH_ENV brings in the environment sbx keeps in
    // /etc/sandbox-persistent.sh.
    let command = match exec.command.as_slice() {
        [line] => line.clone(),
        words => words
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" "),
    };
    let command = if exec.in_toolchain {
        toolchain::wrap_command(&command, target.toolchain)
    } else {
        command
    };
    // Put the shims first, extending the image's PATH in the guest rather
    // than passing one with `-e`, which would replace it (and, in sbx's own
    // environment, change where sbx itself is looked up).
    let shims = target.state_dir.join("shims/bin");
    let command = format!(
        "export PATH={}:{GUEST_HOME}/.local/bin:{NIX_PROFILE_BIN}:\"$PATH\"; {command}",
        shell_quote(&shims.to_string_lossy())
    );
    args.extend(words(&["bash", "-c", &command]));
    SbxCommand { args, env }
}

/// sbx sets `GIT_CONFIG_COUNT=1` in the guest, including
/// [`SBX_GIT_CONFIG`] at command scope so its `.sbx` ignore holds over the
/// repository's own `core.excludesFile`. The git identity entries replace
/// it; append it to them again.
fn keep_sbx_git_config(env: &mut Vec<(String, String)>) {
    let Some(count) = env
        .iter_mut()
        .find(|(key, _)| key == "GIT_CONFIG_COUNT")
        .map(|(_, count)| count)
    else {
        return;
    };
    let Ok(n) = count.parse::<usize>() else {
        return;
    };
    *count = (n + 1).to_string();
    env.push((format!("GIT_CONFIG_KEY_{n}"), "include.path".to_string()));
    env.push((format!("GIT_CONFIG_VALUE_{n}"), SBX_GIT_CONFIG.to_string()));
}

/// workmux's built-in provisioning of a new sandbox, as Lima's: as root,
/// keep the shims on `PATH` for login shells (agents like Codex run commands
/// with `bash -lc`, whose /etc/profile resets `PATH`) and install Nix and
/// Devbox when the worktree uses a toolchain; then, as the agent, put Nix
/// on login shells' `PATH` and fetch Devbox's binary.
fn default_provision(target: &LaunchTarget) -> Vec<SbxCommand> {
    let shims = shell_quote(&target.state_dir.join("shims/bin").to_string_lossy());
    let mut script = format!(
        r#"set -euxo pipefail
cat > /etc/profile.d/workmux-shims.sh <<'PROFILESCRIPT'
if [ -d {shims} ]; then
    PATH={shims}:"$PATH"
    export PATH
fi
PROFILESCRIPT
"#
    );
    let needs_nix = *target.toolchain != DetectedToolchain::None;
    if needs_nix {
        // Single-user sandbox: the agent owns the store, as on Lima. The
        // launcher Devbox installs is execute-only; bash needs to read it.
        script.push_str(
            r#"
if ! command -v nix >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | \
      sh -s -- install linux --init none --no-confirm
    chown -R agent:agent /nix
fi

if ! command -v devbox >/dev/null 2>&1; then
    curl -fsSL https://get.jetify.com/devbox | bash -s -- -f
    chmod +r /usr/local/bin/devbox
fi
"#,
        );
    }
    let name = target.sandbox_name;
    let mut commands = vec![root_exec(name, words(&["bash", "-c", &script]))];
    if needs_nix {
        // As the agent, so Devbox's binary lands in the agent's cache.
        let script = r#"set -euxo pipefail
if ! grep -q 'nix-daemon.sh' ~/.profile 2>/dev/null; then
    echo '. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh' >> ~/.profile
fi
devbox version
"#;
        commands.push(SbxCommand {
            args: words(&["exec", name, "bash", "-c", script]),
            env: Vec::new(),
        });
    }
    commands
}

/// Send SIGHUP to every guest process that carries the exec's session
/// marker, as a closing terminal would: the command and what it started.
fn hangup_command(target: &LaunchTarget, exec: &ExecRequest) -> SbxCommand {
    let script = format!(
        "for p in /proc/[0-9]*; do \
         grep -qxzF {SESSION_ENV}={} \"$p/environ\" 2>/dev/null \
         && kill -HUP \"${{p#/proc/}}\"; done; true",
        exec.session
    );
    SbxCommand {
        args: words(&["exec", target.sandbox_name, "sh", "-c", &script]),
        env: Vec::new(),
    }
}

/// Workspaces that keep the main repository's git metadata read-only, as the
/// container backend does: the git dir is read-only, the parts commits write
/// (objects, refs, logs, the worktree's admin dir, submodule git dirs) are
/// writable inside it, and their control files are read-only again. sbx
/// mounts parents before children, and read-only mounts hold even for root.
fn git_workspaces(git: &GitMetadata) -> Vec<String> {
    let rw = |path: PathBuf| path.to_string_lossy().into_owned();
    let ro = |path: PathBuf| format!("{}:ro", path.display());
    let common = &git.common_dir;

    let mut workspaces = vec![ro(common.clone())];
    for dir in ["objects", "refs", "logs"] {
        workspaces.push(rw(common.join(dir)));
    }
    if git.rr_cache {
        workspaces.push(rw(common.join("rr-cache")));
    }
    workspaces.push(rw(git.admin_dir.clone()));
    workspaces.extend(git.module_roots.iter().cloned().map(rw));

    workspaces.push(ro(common.join("objects/info")));
    for name in ["config.worktree", "gitdir", "commondir"] {
        workspaces.push(ro(git.admin_dir.join(name)));
    }
    for root in &git.module_roots {
        for name in [
            "hooks",
            "info",
            "objects/info",
            "modules",
            "worktrees",
            "config",
            "config.worktree",
        ] {
            workspaces.push(ro(root.join(name)));
        }
    }
    workspaces.extend(git.pointer_files.iter().cloned().map(ro));
    workspaces.extend(git.config_paths.iter().cloned().map(ro));
    workspaces
}

/// Masks hiding `sandbox.docker_sbx.excluded_files` from the agent, by the
/// container backend's rules: only existing regular files inside the
/// worktree are masked; missing paths, directories and paths outside the
/// worktree are skipped with a warning. A mask over a missing path would
/// make sbx create it in the worktree on the workstation.
fn excluded_file_masks(config: &SandboxConfig, worktree_root: &Path) -> Vec<Mask> {
    let mut masks = Vec::new();
    for rel in config.docker_sbx.excluded_files() {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            tracing::warn!(
                path = %rel,
                "sandbox.docker_sbx.excluded_files entry must be a relative path inside the worktree; skipping"
            );
            continue;
        }
        let host_path = worktree_root.join(rel_path);
        if host_path.is_file() {
            // The guest follows a link itself, so mask the file it reads.
            // The main worktree, which the container backend also masks,
            // isn't mounted: a link into it doesn't resolve in the sandbox.
            let file = host_path.canonicalize().unwrap_or(host_path);
            if !file.starts_with(worktree_root) {
                tracing::warn!(
                    path = %rel,
                    "sandbox.docker_sbx.excluded_files entry resolves outside the worktree; skipping"
                );
            } else if !masks.iter().any(|mask: &Mask| mask.target == file) {
                masks.push(Mask::empty(file));
            }
        } else if host_path.is_dir() {
            tracing::warn!(
                path = %rel,
                "sandbox.docker_sbx.excluded_files entry is a directory; only regular files can be masked. Skipping."
            );
        } else {
            tracing::warn!(
                path = %rel,
                "sandbox.docker_sbx.excluded_files entry does not exist on disk; skipping"
            );
        }
    }
    masks
}

/// An `extra_mounts` entry as a workspace: `HOST[:GUEST][:ro]`.
fn extra_mount_workspace(host: &Path, guest: &Path, read_only: bool) -> String {
    let workspace = if guest == host {
        host.to_string_lossy().into_owned()
    } else {
        format!("{}:{}", host.display(), guest.display())
    };
    if read_only {
        format!("{workspace}:ro")
    } else {
        workspace
    }
}

/// `sandbox.extra_mounts` as the workspaces they become, unchecked: what a
/// sandbox's create-time settings record.
fn extra_mount_settings(config: &SandboxConfig) -> Result<Vec<String>> {
    config
        .extra_mounts()
        .iter()
        .map(|mount| {
            let (host, guest, read_only) = mount.resolve()?;
            Ok(extra_mount_workspace(&host, &guest, read_only))
        })
        .collect()
}

/// `sandbox.extra_mounts` as workspaces (`HOST[:GUEST]`), read-only unless
/// writable. sbx mounts a file only read-only.
fn extra_mount_workspaces(
    config: &SandboxConfig,
    target: &LaunchTarget,
    git: &GitMetadata,
) -> Result<Vec<String>> {
    let protected = [
        target.worktree_root.join(".git"),
        git.admin_dir.clone(),
        git.common_dir.clone(),
    ];
    let mut workspaces = Vec::new();
    for mount in config.extra_mounts() {
        let (host, guest, read_only) = mount.resolve()?;
        if read_only {
            workspaces.push(extra_mount_workspace(&host, &guest, read_only));
            continue;
        }
        // Checking the host is the plan's only look at the filesystem.
        if host.is_file() {
            bail!(
                "sandbox.extra_mounts entry {} is a writable file but the docker_sbx \
                 backend mounts files only read-only. Remove writable: true, or mount \
                 its directory instead.",
                host.display()
            );
        }
        if protected
            .iter()
            .any(|path| guest.starts_with(path) || path.starts_with(&guest))
        {
            bail!(
                "Writable extra mount at {} overlaps protected Git metadata",
                guest.display()
            );
        }
        workspaces.push(extra_mount_workspace(&host, &guest, read_only));
    }
    Ok(workspaces)
}

/// Whether `host` matches one of `patterns`, as the network proxy matches
/// allowed domains.
fn host_allowed(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|pattern| domain_matches(host, pattern))
}

/// `sandbox.allowed_domains` hosts, lowercased.
fn allowed_hosts(config: &SandboxConfig) -> Vec<String> {
    config
        .network
        .allowed_domains()
        .iter()
        .map(|entry| entry.host().to_ascii_lowercase())
        .collect()
}

/// The toolchain's endpoints `sandbox.network.allowed_domains` doesn't
/// allow under `deny`, which workmux leaves blocked.
fn blocked_toolchain_hosts(
    config: &SandboxConfig,
    toolchain: &DetectedToolchain,
) -> Vec<&'static str> {
    if !config.network_policy_is_deny() || *toolchain == DetectedToolchain::None {
        return Vec::new();
    }
    let allowed = allowed_hosts(config);
    TOOLCHAIN_HOSTS
        .iter()
        .copied()
        .filter(|host| !host_allowed(&allowed, host))
        .collect()
}

/// `sandbox.network` as rules scoped to the sandbox. Under `deny`, port 443
/// to each allowed domain, as workmux's CONNECT proxy allows on the
/// container backend: the toolchain's endpoints only when
/// `allowed_domains` lists them, since a repository can add a toolchain
/// and `sandbox.network` is the user's own. Under `allow`, port 443 to the
/// toolchain's endpoints when the worktree uses one: the global sbx policy
/// may block them (`balanced` does).
///
/// workmux's `*.d` matches subdomains at any depth but not `d`; sbx's `*.d`
/// matches one label and its `**.d` any depth and `d` too. So `*.d` becomes
/// `**.d` with a deny for `d`, unless `d` is itself allowed.
fn network_rules(config: &SandboxConfig, toolchain: &DetectedToolchain) -> Vec<NetworkRule> {
    let hosts: Vec<String> = if config.network_policy_is_deny() {
        allowed_hosts(config)
    } else if *toolchain != DetectedToolchain::None {
        TOOLCHAIN_HOSTS
            .iter()
            .map(|host| host.to_string())
            .collect()
    } else {
        Vec::new()
    };
    let mut rules = Vec::new();
    let mut add = |decision, resource: String| {
        let rule = NetworkRule { decision, resource };
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    };
    for host in &hosts {
        match host.strip_prefix("*.") {
            Some(base) => {
                add(RuleDecision::Allow, format!("**.{base}:443"));
                if !host_allowed(&hosts, base) {
                    add(RuleDecision::Deny, format!("{base}:443"));
                }
            }
            None => add(RuleDecision::Allow, format!("{host}:443")),
        }
    }
    rules
}

/// What sbx enforces differently from `sandbox.network` without widening
/// it: settings it has no equivalent for, and the toolchain's endpoints
/// `allowed_domains` leaves blocked.
fn network_notes(config: &SandboxConfig, toolchain: &DetectedToolchain) -> Vec<String> {
    if !config.network_policy_is_deny() {
        return Vec::new();
    }
    let mut notes = Vec::new();
    let blocked = blocked_toolchain_hosts(config, toolchain);
    if !blocked.is_empty() {
        notes.push(format!(
            "the worktree's toolchain reaches {}, which sandbox.network.allowed_domains \
             doesn't list; under policy: deny workmux allows them only when you add them \
             to allowed_domains in your global config.",
            blocked.join(", ")
        ));
    }
    if config.network.max_connections.is_some() {
        notes.push(
            "sandbox.network.max_connections is ignored under the docker_sbx backend: \
             sbx doesn't cap concurrent connections."
                .to_string(),
        );
    }
    notes
}

/// How many destinations the user's global sbx policy allows. Every
/// sandbox can reach them, whatever its own rules say.
fn global_network_allows(state: &SbxState) -> usize {
    state
        .rules
        .iter()
        .filter(|rule| {
            rule.scope == "global" && rule.decision == "allow" && rule.resource_type == "network"
        })
        .map(|rule| rule.resources.len())
        .sum()
}

/// Where sbx under `deny` gives the sandbox more than workmux's CONNECT
/// proxy would on the container backend, which it can't take back per
/// sandbox: what the global sbx policy allows, and private or loopback
/// addresses an allowed domain resolves to. Warned on every launch.
fn network_warnings(config: &SandboxConfig, state: &SbxState, name: &str) -> Vec<String> {
    if !config.network_policy_is_deny() {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    let global_allows = global_network_allows(state);
    if global_allows > 0 {
        warnings.push(format!(
            "sandbox.network.policy is deny, but sbx's global network policy also allows \
             {global_allows} destinations besides sandbox.network.allowed_domains \
             (`sbx policy ls {name}` lists them), and a sandbox's rules can't take them back. \
             For the container backend's behaviour, choose `sbx policy init deny-all` \
             (after `sbx policy reset`); workmux never changes the global policy."
        ));
    }
    if global_allows > 0
        || config
            .network
            .allowed_domains()
            .iter()
            .any(|entry| !entry.allow_private_ips())
    {
        warnings.push(
            "sbx doesn't refuse private or loopback addresses an allowed domain resolves to, \
             so allow_private_ips: false can't be enforced under the docker_sbx backend."
                .to_string(),
        );
    }
    warnings
}

/// Whether `allowed` (`allowed_domains` hosts) covers an sbx resource
/// (`host:port`, `*.d` for one label, `**.d` for `d` and any depth). Only
/// port 443, the only port `allowed_domains` allows.
fn resource_covered(allowed: &[String], resource: &str) -> bool {
    let Some((host, "443")) = resource.rsplit_once(':') else {
        return false;
    };
    match host.strip_prefix("**.") {
        Some(base) => host_allowed(allowed, base) && host_allowed(allowed, &format!("*.{base}")),
        None => host_allowed(allowed, host),
    }
}

/// Under `deny`, what the rules sbx's kits added to the sandbox (the agent
/// kit's and any mixin kit's) allow that `allowed_domains` doesn't cover,
/// as a warning; for every launch, and again after a launch creates the
/// sandbox, since the kit adds them at create. A kit's rule is scoped to
/// the sandbox and `provisioned`; the user's rules and workmux's are
/// `added`. A resource workmux's own rules (`network_rules`, its record
/// after the launch) name isn't counted either: workmux's allow is its own,
/// and its deny (a wildcard's base) wins over the kit's allow. workmux never
/// removes a kit's rules: the kit needs them.
pub fn kit_rule_warnings(
    config: &SandboxConfig,
    rules: &[SbxRule],
    name: &str,
    network_rules: &[NetworkRule],
) -> Vec<String> {
    if !config.network_policy_is_deny() {
        return Vec::new();
    }
    let scope = format!("sandbox:{name}");
    let allowed = allowed_hosts(config);
    let mut uncovered: Vec<&str> = Vec::new();
    let kit_resources = rules
        .iter()
        .filter(|rule| {
            rule.scope == scope
                && rule.decision == "allow"
                && rule.resource_type == "network"
                && rule.provenance.created_via == "provisioned"
        })
        .flat_map(|rule| &rule.resources);
    for resource in kit_resources {
        let recorded = network_rules.iter().any(|own| own.resource == *resource);
        if !recorded && !resource_covered(&allowed, resource) && !uncovered.contains(&&**resource) {
            uncovered.push(resource);
        }
    }
    if uncovered.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "sandbox.network.policy is deny, but the rules sbx's kits added to this sandbox also \
         allow {}, which sandbox.network.allowed_domains doesn't cover \
         (`sbx policy ls {name}` lists them). workmux never removes a kit's rules, which the \
         kit needs; list your agent's API domains in allowed_domains (it covers port 443 \
         only) to keep this warning to the rest.",
        uncovered.join(", ")
    )]
}

/// Fail closed when a project's `.workmux.yaml` chose docker_sbx for a
/// global `deny` that sbx's global policy would widen: a repository can
/// lower its own sandbox's protection only with the user's say-so.
fn check_backend_choice(config: &SandboxConfig, state: &SbxState) -> Result<()> {
    let global_allows = global_network_allows(state);
    if config.backend_from_project && config.network_policy_is_deny() && global_allows > 0 {
        bail!(
            "sandbox.backend: docker_sbx comes from the project config (.workmux.yaml), but \
             your global config sets sandbox.network.policy: deny and sbx's global network \
             policy allows {global_allows} destinations beyond sandbox.network.allowed_domains. \
             A repository can't move a deny sandbox onto a wider policy. Set \
             sandbox.backend: docker_sbx in your global config to accept sbx's global policy, \
             or choose `sbx policy init deny-all` (after `sbx policy reset`)."
        );
    }
    Ok(())
}

/// The `sbx policy` command adding or removing a rule scoped to the sandbox.
fn policy_command(name: &str, verb: &str, resource: &str) -> SbxCommand {
    let mut args = words(&["policy", verb, "network", "--sandbox", name]);
    if verb == "rm" {
        args.extend(words(&["--resource", resource, "-f"]));
    } else {
        args.push(resource.to_string());
    }
    SbxCommand {
        args,
        env: Vec::new(),
    }
}

/// Whether sbx's diagnostics report `name` as failed.
fn check_failed(state: &SbxState, name: &str) -> bool {
    state
        .checks
        .iter()
        .any(|check| check.name == name && check.status == "fail")
}

/// Checks sbx's diagnostics fail that [`check_prerequisites`] doesn't stop
/// for, such as disk space. sbx decides for itself whether it can still run
/// the sandbox, and reports its own error if not.
fn check_warnings(state: &SbxState) -> Vec<String> {
    state
        .checks
        .iter()
        .filter(|check| {
            check.status == "fail"
                && !matches!(
                    check.name.as_str(),
                    "Authentication" | "Global network policy"
                )
        })
        .map(|check| {
            let mut warning = format!("sbx diagnose: {} failed", check.name);
            if !check.message.is_empty() {
                warning.push_str(&format!(" ({})", check.message));
            }
            warning.push('.');
            if !check.hint.is_empty() {
                warning.push_str(&format!(" {}", check.hint));
            }
            warning
        })
        .collect()
}

/// Fail closed unless sbx can run sandboxes. workmux never runs `sbx login`
/// or `sbx policy init` itself.
fn check_prerequisites(state: &SbxState) -> Result<()> {
    let platform = &state.platform;
    let supported = match platform.os.as_str() {
        "macos" => platform.arch == "aarch64",
        "linux" => platform.kvm,
        "windows" => true,
        _ => false,
    };
    if !supported {
        bail!(
            "Docker Sandboxes doesn't support this host ({} on {}). sbx runs on macOS 14+ \
             on Apple silicon, Windows 11, and Ubuntu 24.04+ with KVM.\n\
             Use the container or lima sandbox backend instead.",
            platform.os,
            platform.arch
        );
    }
    if check_failed(state, "Authentication") {
        bail!(
            "sbx is not logged in. Sign in to Docker with `sbx login`, then try again.\n\
             Or disable sandbox: set 'sandbox.enabled: false' in config."
        );
    }
    if check_failed(state, "Global network policy") {
        bail!(
            "sbx's global network policy has not been initialized. Choose one with \
             `sbx policy init`, then try again; workmux never picks it for you."
        );
    }
    Ok(())
}

/// Settings that differ from what an existing sandbox was created with,
/// which apply only to a new one: a warning naming them and how to
/// recreate it, as Lima says for `provision`.
fn create_settings_warnings(
    current: &CreateSettings,
    created: &CreateSettings,
    name: &str,
) -> Vec<String> {
    let changed: Vec<&str> = [
        ("agent", current.agent != created.agent),
        (
            "agent_config_dir",
            current.agent_config_dir != created.agent_config_dir,
        ),
        (
            "OpenCode's config directory",
            current.opencode_config_dir != created.opencode_config_dir,
        ),
        ("extra_mounts", current.extra_mounts != created.extra_mounts),
        (
            "docker_sbx.excluded_files",
            current.excluded_files != created.excluded_files,
        ),
        ("docker_sbx.cpus", current.cpus != created.cpus),
        ("docker_sbx.memory", current.memory != created.memory),
        ("docker_sbx.kits", current.kits != created.kits),
        ("image", current.template != created.template),
        ("toolchain", current.toolchain != created.toolchain),
    ]
    .into_iter()
    .filter(|(_, differs)| *differs)
    .map(|(setting, _)| setting)
    .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "sbx sandbox {name} was created with different settings ({}), which apply only \
         when the sandbox is created; recreate it to apply them with `workmux sandbox prune` \
         or `sbx rm -f {name}`.",
        changed.join(", ")
    )]
}

/// Fail closed when the agent could read a file workmux hides: one of
/// `excluded_files` that exists now but the existing sandbox wasn't created
/// masking, listed or created since. Masks are fixed when sbx creates the
/// sandbox, where the container backend masks on every launch. A masked
/// file deleted since only gets the settings warning: sbx puts an empty
/// file back over it.
fn check_masked_since_create(
    excluded: &[Mask],
    created: &CreateSettings,
    name: &str,
) -> Result<()> {
    if let Some(mask) = excluded
        .iter()
        .find(|mask| !created.excluded_files.contains(&mask.target))
    {
        bail!(
            "sandbox.docker_sbx.excluded_files lists {}, but sbx sandbox {name} was created \
             without a mask over it, so the agent could read it. Masks are fixed when the \
             sandbox is created; recreate it with `workmux sandbox prune` or `sbx rm -f {name}`.",
            mask.target.display()
        );
    }
    Ok(())
}

/// Fail closed when the existing sandbox was created under the other
/// `sandbox.docker_sbx.agent` mode: a `shell` sandbox has the agent's real
/// credential mounted, which switching to `kit` must not keep, and a `kit`
/// sandbox lacks the workstation's agent config `shell` promises.
fn check_agent_mode_since_create(
    mode: DockerSbxAgent,
    created: DockerSbxAgent,
    name: &str,
) -> Result<()> {
    if mode != created {
        bail!(
            "sbx sandbox {name} was created under sandbox.docker_sbx.agent: {}, but the \
             config now says agent: {}, which applies only when the sandbox is created. \
             Recreate it with `workmux sandbox prune` or `sbx rm -f {name}` (close the \
             worktree's panes first).",
            created.as_str(),
            mode.as_str()
        );
    }
    Ok(())
}

/// Whether the launch creates the sandbox: it's missing, or it has no
/// create record and isn't running. A running sandbox may have panes in it,
/// which removing it would kill.
pub fn creates_sandbox(state: &SbxState, created_with: &CreateRecord) -> bool {
    match (&state.sandbox, created_with) {
        (None, _) => true,
        (Some(sandbox), CreateRecord::Missing) => !sandbox.is_running(),
        (Some(_), _) => false,
    }
}

/// The settings an existing sandbox was created with. Fails when workmux
/// can't vouch for them: the sandbox is running without a create record,
/// or the record doesn't parse.
fn existing_settings<'a>(created_with: &'a CreateRecord, name: &str) -> Result<&'a CreateSettings> {
    let recreate = format!(
        "Close the worktree's panes and remove it with `workmux sandbox prune` or \
         `sbx rm -f {name}`; the next launch creates it again."
    );
    match created_with {
        CreateRecord::Recorded(created) => Ok(created.as_ref()),
        CreateRecord::Missing => bail!(
            "sbx sandbox {name} is running, but workmux has no record of creating it, so it \
             can't vouch for how it was set up. {recreate}"
        ),
        CreateRecord::Unreadable(error) => bail!(
            "workmux's record of how sbx sandbox {name} was created doesn't parse \
             ({error}). {recreate}"
        ),
    }
}

/// Compute the sbx commands a launch needs.
///
/// When the sandbox is missing, or it's stopped and no launch finished
/// setting it up (it's removed first): create it from the template, with
/// the configured resources and mixin kits, mounting the worktree, the main
/// repository's git dir (read-only but for what commits write) and the
/// shims at their host paths, an empty file over each of `excluded_files`,
/// and `extra_mounts`.
/// Under `agent: kit` it's created for sbx's agent kit, which sets the agent
/// up in the sandbox's own home; then workmux's status hooks are added to
/// the kit's settings. Under `agent: shell` it's created with sbx's generic
/// `shell` agent (a kit would write into the mounted config directory) and
/// the agent's config directory is mounted where the agent looks for it;
/// then, as root, the agent gets the config directory's parents and
/// Claude's onboarding state is seeded. A sandbox created under the other
/// mode fails to launch, as does a running one with no create record or one
/// whose record doesn't parse.
/// On every launch: bind pi's and OMP's binaries directory to a guest-local
/// one, reconcile the sandbox's rules with `sandbox.network` and the
/// toolchain's endpoints, and allow the supervisor's RPC port until the exec
/// ends. Launching into an existing sandbox fails when the agent could read
/// a file workmux hides that the sandbox has no mask over. Last, when the
/// sandbox was just created, workmux's built-in
/// provisioning (shims on login shells' `PATH`, Nix and Devbox for a
/// toolchain) and the `provision` script. The exec runs the agent command in the toolchain's
/// environment.
pub fn launch_plan(
    config: &SandboxConfig,
    target: &LaunchTarget,
    agent: &str,
    state: &SbxState,
    exec: &ExecRequest,
) -> Result<LaunchPlan> {
    check_prerequisites(state)?;
    check_backend_choice(config, state)?;
    config
        .network
        .validate()
        .context("Invalid sandbox network config")?;

    let name = target.sandbox_name;
    let mut setup = Vec::new();
    let mut create = None;
    let excluded = excluded_file_masks(config, target.worktree_root);
    let mut masks = Vec::new();
    let template = super::template::template_tag(&config.resolved_image(agent));
    let mode = config.docker_sbx.agent();
    // Under `kit`, sbx's agent kit sets the agent up in the sandbox's own
    // home, so its config directory isn't mounted.
    let kit = match mode {
        DockerSbxAgent::Kit => Some(kit_for(config, agent)?),
        DockerSbxAgent::Shell => None,
    };
    let config_dir = kit
        .is_none()
        .then(|| config_dir_and_home_subpath(config, agent))
        .flatten();
    // The opencode kit writes its own ~/.config/opencode.
    let opencode_config_dir = target
        .opencode_config_dir
        .filter(|_| agent == "opencode" && kit.is_none());
    let sbx = &config.docker_sbx;
    let create_settings = CreateSettings {
        agent: agent.to_string(),
        agent_mode: mode,
        agent_config_dir: config_dir.as_ref().map(|(dir, _)| dir.clone()),
        opencode_config_dir: opencode_config_dir.map(Path::to_path_buf),
        extra_mounts: extra_mount_settings(config)?,
        excluded_files: excluded.iter().map(|mask| mask.target.clone()).collect(),
        cpus: sbx.cpus(),
        memory: sbx.memory().map(str::to_string),
        kits: sbx.kits().to_vec(),
        template: template.clone(),
        toolchain: match target.toolchain {
            DetectedToolchain::Devbox => "devbox",
            DetectedToolchain::Flake => "flake",
            DetectedToolchain::None => "none",
        }
        .to_string(),
    };

    // A stopped sandbox no launch finished setting up is removed and
    // created again.
    let exists = !creates_sandbox(state, target.created_with);
    let remove = (state.sandbox.is_some() && !exists).then(|| SbxCommand {
        args: words(&["rm", "-f", name]),
        env: Vec::new(),
    });

    let created_with = if exists {
        let created = existing_settings(target.created_with, name)?;
        check_agent_mode_since_create(mode, created.agent_mode, name)?;
        check_masked_since_create(&excluded, created, name)?;
        Some(created)
    } else {
        None
    };

    if !exists {
        let git = target
            .git
            .context("git metadata missing for a sandbox the plan creates")?;
        let extra_mounts = extra_mount_workspaces(config, target, git)?;
        let mut args = words(&[
            "create",
            "--name",
            name,
            "--no-progress",
            "--template",
            &template,
            "--pull",
            "never",
        ]);
        if let Some(cpus) = sbx.cpus() {
            args.extend(["--cpus".to_string(), cpus.to_string()]);
        }
        if let Some(memory) = sbx.memory() {
            args.extend(["--memory".to_string(), memory.to_string()]);
        }
        for kit in sbx.kits() {
            args.extend(["--kit".to_string(), kit.clone()]);
        }
        args.push(kit.map_or("shell", Kit::name).to_string());
        let mut workspaces = vec![target.worktree_root.to_string_lossy().into_owned()];
        workspaces.extend(git_workspaces(git));
        workspaces.extend(excluded.iter().map(|mask| mask.workspace(target.state_dir)));
        masks.extend(excluded);
        // The agent's config directory where the agent looks for it, as on
        // the other backends.
        if let Some((config_dir, subpath)) = &config_dir {
            workspaces.push(format!("{}:{GUEST_HOME}/{subpath}", config_dir.display()));
        }
        if let Some(dir) = opencode_config_dir {
            workspaces.push(format!(
                "{}:{GUEST_HOME}/{OPENCODE_CONFIG_SUBPATH}:ro",
                dir.display()
            ));
        }
        workspaces.push(format!("{}:ro", target.state_dir.display()));
        workspaces.extend(extra_mounts);
        args.extend(workspaces.iter().cloned());
        create = Some(CreateStep {
            command: SbxCommand {
                args,
                env: Vec::new(),
            },
            workspaces,
        });

        // sbx creates the missing parents of a mount's guest path as root;
        // hand them to the agent, so it can still write beside the mount
        // (`~/.local/state`).
        let mut subpaths: Vec<&str> = config_dir.iter().map(|(_, subpath)| *subpath).collect();
        if opencode_config_dir.is_some() {
            subpaths.push(OPENCODE_CONFIG_SUBPATH);
        }
        setup.extend(own_home_parents(name, &subpaths));

        // Claude keeps its onboarding state in ~/.claude.json, outside the
        // config directory. Seed the sandbox's own copy, as Lima seeds each
        // VM's, so Claude doesn't run onboarding in every sandbox.
        // The claude kit does so itself.
        if agent == "claude" && kit.is_none() {
            let mut command = words(&[
                "sh",
                "-c",
                "[ -e \"$1\" ] || { printf %s \"$2\" > \"$1\" && chown agent:agent \"$1\"; }",
                "sh",
            ]);
            command.push(format!("{GUEST_HOME}/.claude.json"));
            command.push(crate::sandbox::CLAUDE_ONBOARDING_JSON.to_string());
            setup.push(root_exec(name, command));
        }

        // The workstation's hooks aren't there under `kit`: install
        // workmux's status hooks into the config the kit wrote, as the
        // agent. The kit writes it only at create, so this lasts.
        if let Some(kit) = kit {
            setup.extend(kit.status_hooks(name)?);
        }
    }

    // Pi and OMP download platform-specific fd/rg binaries into their config
    // directory, which would overwrite the workstation's own through the
    // mount. Bind a guest-local directory over it, as the container backend
    // overlays pi's bin/. A guest mount ends when sbx stops the sandbox, so
    // every launch makes it.
    if let Some((subdir, guest_dir)) = guest_binaries_dir(agent)
        && let Some((_, subpath)) = config_dir_and_home_subpath(config, agent)
    {
        let mut command = words(&[
            "sh",
            "-c",
            "mountpoint -q \"$1\" || { install -d -o agent -g agent \"$2\" \
             && mkdir -p \"$1\" && mount --bind \"$2\" \"$1\"; }",
            "sh",
        ]);
        command.push(format!("{GUEST_HOME}/{subpath}/{subdir}"));
        command.push(guest_dir.to_string());
        setup.push(root_exec(name, command));
    }

    // Reconcile `sandbox.network` with the sandbox's rules. Only rules
    // workmux wrote (its record) are ever removed; stale ones go first, as
    // sbx refuses a rule while its scope holds the opposite decision for
    // the same resource. A rule the user already had stays theirs.
    let sandbox_scope = format!("sandbox:{name}");
    let present = |rule: &NetworkRule| {
        exists
            && state.rules.iter().any(|r| {
                r.scope == sandbox_scope
                    && r.decision == rule.decision.as_str()
                    && r.resources.contains(&rule.resource)
            })
    };
    let wanted = network_rules(config, target.toolchain);
    for rule in target.network_record {
        if !wanted.contains(rule) && present(rule) {
            setup.push(policy_command(name, "rm", &rule.resource));
        }
    }
    let mut network_rules = Vec::new();
    for rule in wanted {
        if !present(&rule) {
            setup.push(policy_command(name, rule.decision.as_str(), &rule.resource));
            network_rules.push(rule);
        } else if target.network_record.contains(&rule) {
            network_rules.push(rule);
        }
    }

    // Provisioning runs once, when the sandbox is created: workmux's own
    // (unless skip_default_provision), then the custom script, as root.
    let provision = config
        .docker_sbx
        .provision_script()
        .filter(|_| !exists)
        .map(|script| root_exec(name, words(&["bash", "-c", script])));

    // A rule scoped to the sandbox lets the guest reach the supervisor's RPC
    // server through host.docker.internal; sbx names the workstation
    // `localhost`. Only its port, and only while the supervisor runs: the
    // rule is removed when the exec ends, unless it was already there.
    let mut teardown = Vec::new();
    if let Some(port) = exec.rpc_port {
        let rpc_rule = NetworkRule {
            decision: RuleDecision::Allow,
            resource: format!("{WORKSTATION_HOST}:{port}"),
        };
        if !present(&rpc_rule) {
            setup.push(policy_command(name, "allow", &rpc_rule.resource));
            teardown.push(policy_command(name, "rm", &rpc_rule.resource));
        }
    }

    let default_provision = if !exists && !config.docker_sbx.skip_default_provision() {
        default_provision(target)
    } else {
        Vec::new()
    };

    Ok(LaunchPlan {
        sandbox_name: name.to_string(),
        remove,
        create,
        masks,
        setup,
        default_provision,
        provision,
        exec: exec_command(target, exec),
        hangup: hangup_command(target, exec),
        teardown,
        notes: kit
            .and_then(|kit| ignored_config_dir_note(config, kit))
            .into_iter()
            .chain(network_notes(config, target.toolchain))
            .collect(),
        warnings: check_warnings(state)
            .into_iter()
            .chain(network_warnings(config, state, name))
            .chain(kit_rule_warnings(
                config,
                &state.rules,
                name,
                &network_rules,
            ))
            .chain(
                created_with
                    .into_iter()
                    .flat_map(|created| create_settings_warnings(&create_settings, created, name)),
            )
            .collect(),
        network_rules,
        create_settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AllowedDomainDetails, AllowedDomainEntry, DockerSbxAgent, DockerSbxConfig, ExtraMount,
        NetworkConfig, NetworkPolicy,
    };
    use crate::sandbox::docker_sbx::instance::SbxRuleProvenance;
    use std::path::PathBuf;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// A config under `agent: shell`, which mounts the agent's config
    /// directory.
    fn config() -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            backend: Some(crate::config::SandboxBackend::DockerSbx),
            image: Some("test-image:latest".to_string()),
            agent_config_dir: Some("/home/me/.{agent}".to_string()),
            docker_sbx: DockerSbxConfig {
                agent: Some(DockerSbxAgent::Shell),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// A config under `agent: kit`, the default.
    fn kit_config() -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            backend: Some(crate::config::SandboxBackend::DockerSbx),
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        }
    }

    fn passing_checks() -> Vec<SbxCheck> {
        [
            ("Authentication", "pass"),
            ("Global network policy", "pass"),
        ]
        .into_iter()
        .map(|(name, status)| SbxCheck {
            name: name.to_string(),
            status: status.to_string(),
            message: String::new(),
            hint: String::new(),
        })
        .collect()
    }

    fn missing() -> SbxState {
        SbxState {
            platform: Platform {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                kvm: false,
            },
            checks: passing_checks(),
            sandbox: None,
            rules: Vec::new(),
        }
    }

    fn sandbox(status: &str) -> SbxSandboxInfo {
        SbxSandboxInfo {
            name: "wm-proj-12345678".to_string(),
            status: status.to_string(),
            created_at: None,
            last_used_at: None,
        }
    }

    struct Paths {
        worktree: PathBuf,
        cwd: PathBuf,
        git: GitMetadata,
        state: PathBuf,
        record: Vec<NetworkRule>,
        toolchain: DetectedToolchain,
        created: CreateRecord,
    }

    fn git_metadata() -> GitMetadata {
        GitMetadata {
            common_dir: PathBuf::from("/code/proj/.git"),
            admin_dir: PathBuf::from("/code/proj/.git/worktrees/feat"),
            pointer_files: vec![PathBuf::from("/code/proj__worktrees/feat/.git")],
            ..Default::default()
        }
    }

    /// What `config()` creates a claude sandbox with.
    fn created_settings() -> CreateSettings {
        CreateSettings {
            agent: "claude".to_string(),
            agent_mode: DockerSbxAgent::Shell,
            agent_config_dir: Some(PathBuf::from("/home/me/.claude")),
            template: super::super::template::template_tag("test-image:latest"),
            toolchain: "none".to_string(),
            ..Default::default()
        }
    }

    fn paths() -> Paths {
        Paths {
            worktree: PathBuf::from("/code/proj__worktrees/feat"),
            cwd: PathBuf::from("/code/proj__worktrees/feat/sub"),
            git: git_metadata(),
            state: PathBuf::from("/state/docker-sbx/wm-proj-12345678"),
            record: Vec::new(),
            toolchain: DetectedToolchain::None,
            created: CreateRecord::Recorded(Box::new(created_settings())),
        }
    }

    fn target(p: &Paths) -> LaunchTarget<'_> {
        LaunchTarget {
            sandbox_name: "wm-proj-12345678",
            worktree_root: &p.worktree,
            pane_cwd: &p.cwd,
            git: Some(&p.git),
            opencode_config_dir: None,
            state_dir: &p.state,
            network_record: &p.record,
            toolchain: &p.toolchain,
            created_with: &p.created,
        }
    }

    fn exec() -> ExecRequest {
        ExecRequest {
            command: args(&["claude --dangerously-skip-permissions"]),
            env: vec![
                ("WM_RPC_PORT".to_string(), "4242".to_string()),
                ("WM_RPC_TOKEN".to_string(), "secret".to_string()),
            ],
            session: "0123456789abcdef".to_string(),
            rpc_port: Some(4242),
            tty: true,
            in_toolchain: true,
        }
    }

    #[test]
    fn missing_sandbox_is_created_from_the_template_with_the_shell_agent() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let tag = super::super::template::template_tag("test-image:latest");
        let create = plan.create.as_ref().unwrap();
        assert_eq!(create.command.args[9..], create.workspaces[..]);
        assert_eq!(
            create_args(&plan)[..10],
            args(&[
                "create",
                "--name",
                "wm-proj-12345678",
                "--no-progress",
                "--template",
                &tag,
                "--pull",
                "never",
                "shell",
                "/code/proj__worktrees/feat",
            ])
        );
        assert_eq!(
            create_args(&plan).last().unwrap(),
            "/state/docker-sbx/wm-proj-12345678:ro"
        );
    }

    #[test]
    fn existing_sandbox_is_neither_created_nor_set_up_again() {
        let p = paths();
        let state = SbxState {
            sandbox: Some(sandbox("stopped")),
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();

        assert!(plan.create.is_none(), "{:?}", plan.create);
        assert!(
            plan.setup
                .iter()
                .all(|c| c.args[0] != "create" && !c.args.contains(&"root".to_string())),
            "{:?}",
            plan.setup
        );
    }

    #[test]
    fn a_stopped_sandbox_without_a_create_record_is_removed_and_created_again() {
        // A launch that failed or was killed between `sbx create` and the
        // end of provisioning leaves a sandbox with no record of its
        // settings, which would otherwise be used half set up.
        let mut p = paths();
        p.created = CreateRecord::Missing;
        p.record = vec![recorded(RuleDecision::Allow, "kept.com:443")];
        let config = with_network(NetworkPolicy::Deny, domains(&["kept.com"]));
        let state = SbxState {
            sandbox: Some(sandbox("stopped")),
            ..running_with(vec![rule(SCOPE, "kept.com:443")])
        };
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(
            plan.remove.as_ref().map(|c| c.args.clone()),
            Some(args(&["rm", "-f", "wm-proj-12345678"]))
        );
        assert!(plan.create.is_some());
        assert!(!plan.default_provision.is_empty());
        // `sbx rm` takes the sandbox's rules with it.
        assert!(
            policy_commands(&plan).contains(&policy("allow", "kept.com:443")),
            "{:?}",
            plan.setup
        );
    }

    #[test]
    fn a_running_sandbox_without_a_create_record_fails_naming_workmux_sandbox_prune() {
        // Panes may be running in it, which removing it would kill, and
        // workmux can't vouch for how it was set up.
        for config in [config(), kit_config()] {
            let mut p = paths();
            p.created = CreateRecord::Missing;
            let err = launch_plan(
                &config,
                &target(&p),
                "claude",
                &running_with(Vec::new()),
                &exec(),
            )
            .unwrap_err()
            .to_string();
            for expected in [
                "wm-proj-12345678",
                "workmux sandbox prune",
                "sbx rm -f wm-proj-12345678",
            ] {
                assert!(err.contains(expected), "{expected}: {err}");
            }
        }
    }

    #[test]
    fn an_unparsable_create_record_fails_naming_workmux_sandbox_prune() {
        let mut p = paths();
        p.created = CreateRecord::Unreadable("expected value at line 1".to_string());
        for state in [
            running_with(Vec::new()),
            SbxState {
                sandbox: Some(sandbox("stopped")),
                ..missing()
            },
        ] {
            let err = launch_plan(&config(), &target(&p), "claude", &state, &exec())
                .unwrap_err()
                .to_string();
            for expected in [
                "wm-proj-12345678",
                "expected value at line 1",
                "workmux sandbox prune",
                "sbx rm -f wm-proj-12345678",
            ] {
                assert!(err.contains(expected), "{expected}: {err}");
            }
        }
    }

    #[test]
    fn a_missing_sandbox_is_created_whatever_its_stale_record_says() {
        // `workmux sandbox prune` only acts on sandboxes sbx lists, so a
        // record left behind by a removed sandbox mustn't block creating it.
        for created in [
            CreateRecord::Missing,
            CreateRecord::Unreadable("expected value at line 1".to_string()),
        ] {
            let mut p = paths();
            p.created = created;
            let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
            assert!(plan.create.is_some());
            assert!(plan.remove.is_none(), "{:?}", plan.remove);
        }
    }

    #[test]
    fn a_finished_or_missing_sandbox_is_never_removed() {
        let p = paths();
        let existing = running_with(Vec::new());
        for state in [missing(), existing] {
            let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();
            assert!(plan.remove.is_none(), "{:?}", plan.remove);
        }
    }

    #[test]
    fn plan_names_the_settings_a_created_sandbox_is_recorded_with() {
        let p = paths();
        let config = SandboxConfig {
            docker_sbx: DockerSbxConfig {
                cpus: Some(4),
                memory: Some("8g".to_string()),
                kits: Some(vec!["docker.io/sbx/task:latest".to_string()]),
                ..config().docker_sbx
            },
            ..config()
        };
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.create_settings,
            CreateSettings {
                agent: "claude".to_string(),
                agent_mode: DockerSbxAgent::Shell,
                agent_config_dir: Some(PathBuf::from("/home/me/.claude")),
                opencode_config_dir: None,
                extra_mounts: Vec::new(),
                excluded_files: Vec::new(),
                cpus: Some(4),
                memory: Some("8g".to_string()),
                kits: vec!["docker.io/sbx/task:latest".to_string()],
                template: super::super::template::template_tag("test-image:latest"),
                toolchain: "none".to_string(),
            }
        );
    }

    #[test]
    fn a_sandbox_created_under_the_other_agent_mode_fails_naming_how_to_recreate_it() {
        // Switching to `kit` must never keep running a sandbox that has the
        // agent's real credential mounted.
        let kit_record = CreateSettings {
            agent_mode: DockerSbxAgent::Kit,
            agent_config_dir: None,
            ..created_settings()
        };
        for (config, created, mode) in [
            (kit_config(), created_settings(), "shell"),
            (config(), kit_record, "kit"),
        ] {
            let mut p = paths();
            p.created = CreateRecord::Recorded(Box::new(created));
            for state in [
                running_with(Vec::new()),
                SbxState {
                    sandbox: Some(sandbox("stopped")),
                    ..missing()
                },
            ] {
                let err = launch_plan(&config, &target(&p), "claude", &state, &exec())
                    .unwrap_err()
                    .to_string();
                let created_under = format!("agent: {mode}");
                for expected in [
                    created_under.as_str(),
                    "wm-proj-12345678",
                    "workmux sandbox prune",
                    "sbx rm -f wm-proj-12345678",
                ] {
                    assert!(err.contains(expected), "{expected}: {err}");
                }
            }
        }
    }

    #[test]
    fn the_create_record_holds_the_agent_mode() {
        let p = paths();
        for (config, mode) in [
            (kit_config(), DockerSbxAgent::Kit),
            (config(), DockerSbxAgent::Shell),
        ] {
            let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
            assert_eq!(plan.create_settings.agent_mode, mode);
        }
    }

    #[test]
    fn existing_sandbox_created_with_other_settings_warns_how_to_recreate_it() {
        // Mounts, resources, kits, the template and the toolchain are fixed
        // when sbx creates the sandbox.
        let mut p = paths();
        p.created = CreateRecord::Recorded(Box::new(CreateSettings {
            agent: "codex".to_string(),
            agent_config_dir: Some(PathBuf::from("/home/me/.codex")),
            memory: Some("4g".to_string()),
            ..created_settings()
        }));
        let config = SandboxConfig {
            docker_sbx: DockerSbxConfig {
                memory: Some("8g".to_string()),
                ..config().docker_sbx
            },
            ..config()
        };
        let state = running_with(Vec::new());
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert!(plan.create.is_none(), "{:?}", plan.create);
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        let warning = &plan.warnings[0];
        for expected in [
            "wm-proj-12345678",
            "agent, agent_config_dir, docker_sbx.memory",
            "workmux sandbox prune",
            "sbx rm -f wm-proj-12345678",
        ] {
            assert!(warning.contains(expected), "{expected}: {warning}");
        }
        assert!(!warning.contains("docker_sbx.cpus"), "{warning}");
    }

    #[test]
    fn unchanged_or_new_sandboxes_get_no_settings_warning() {
        let p = paths();
        for state in [running_with(Vec::new()), missing()] {
            let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();
            assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        }
    }

    #[test]
    fn existing_sandbox_launches_without_its_git_metadata() {
        // Finding the git metadata walks the worktree; only creating the
        // sandbox mounts it.
        let p = paths();
        let target = LaunchTarget {
            git: None,
            ..target(&p)
        };
        let plan = launch_plan(
            &config(),
            &target,
            "claude",
            &running_with(Vec::new()),
            &exec(),
        );
        assert!(plan.is_ok(), "{:?}", plan.err());

        let err = launch_plan(&config(), &target, "claude", &missing(), &exec()).unwrap_err();
        assert!(format!("{err:#}").contains("git metadata"), "{err:#}");
    }

    fn rule(scope: &str, resource: &str) -> SbxRule {
        SbxRule {
            scope: scope.to_string(),
            decision: "allow".to_string(),
            resource_type: "network".to_string(),
            resources: vec![resource.to_string()],
            provenance: SbxRuleProvenance {
                created_via: "added".to_string(),
            },
        }
    }

    fn with_network(policy: NetworkPolicy, domains: Vec<AllowedDomainEntry>) -> SandboxConfig {
        SandboxConfig {
            network: NetworkConfig {
                policy: Some(policy),
                allowed_domains: Some(domains),
                max_connections: None,
            },
            ..config()
        }
    }

    fn domains(hosts: &[&str]) -> Vec<AllowedDomainEntry> {
        hosts
            .iter()
            .map(|host| AllowedDomainEntry::Plain(host.to_string()))
            .collect()
    }

    fn policy_commands(plan: &LaunchPlan) -> Vec<Vec<String>> {
        plan.setup
            .iter()
            .filter(|c| c.args[0] == "policy")
            .map(|c| c.args.clone())
            .collect()
    }

    fn policy(verb: &str, resource: &str) -> Vec<String> {
        match verb {
            "rm" => args(&[
                "policy",
                "rm",
                "network",
                "--sandbox",
                "wm-proj-12345678",
                "--resource",
                resource,
                "-f",
            ]),
            _ => args(&[
                "policy",
                verb,
                "network",
                "--sandbox",
                "wm-proj-12345678",
                resource,
            ]),
        }
    }

    #[test]
    fn rpc_rule_allows_only_the_supervisors_port_until_it_exits() {
        // A bare `localhost` rule would reach every service on the
        // workstation's loopback, not just workmux's RPC server.
        let p = paths();
        for policy_kind in [NetworkPolicy::Allow, NetworkPolicy::Deny] {
            let config = with_network(policy_kind, Vec::new());
            let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

            assert!(
                policy_commands(&plan).contains(&policy("allow", "localhost:4242")),
                "{:?}",
                plan.setup
            );
            assert_eq!(
                plan.teardown
                    .iter()
                    .map(|c| c.args.clone())
                    .collect::<Vec<_>>(),
                vec![policy("rm", "localhost:4242")]
            );
            assert!(
                plan.network_rules.is_empty(),
                "the record is sandbox.network's"
            );
        }
    }

    #[test]
    fn rpc_rule_already_there_is_neither_added_nor_removed() {
        let p = paths();
        let existing = running_with(vec![
            rule("global", "api.anthropic.com"),
            rule(SCOPE, "localhost:4242"),
        ]);
        let plan = launch_plan(&config(), &target(&p), "claude", &existing, &exec()).unwrap();
        assert!(plan.setup.is_empty(), "{:?}", plan.setup);
        assert!(plan.teardown.is_empty(), "{:?}", plan.teardown);

        // A global rule, or one for another port, isn't this supervisor's.
        for rules in [
            vec![rule("global", "localhost:4242")],
            vec![rule(SCOPE, "localhost:4243")],
        ] {
            let plan = launch_plan(
                &config(),
                &target(&p),
                "claude",
                &running_with(rules),
                &exec(),
            )
            .unwrap();
            assert_eq!(
                policy_commands(&plan),
                vec![policy("allow", "localhost:4242")]
            );
        }
    }

    #[test]
    fn launch_without_an_rpc_server_adds_no_rpc_rule() {
        // `workmux add` creating the sandbox, and `sandbox shell`.
        let p = paths();
        let request = ExecRequest {
            rpc_port: None,
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &request).unwrap();
        assert!(policy_commands(&plan).is_empty(), "{:?}", plan.setup);
        assert!(plan.teardown.is_empty());
    }

    #[test]
    fn deny_allows_port_443_to_each_allowed_domain_in_the_sandbox_only() {
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&["api.anthropic.com", "github.com"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(
            commands.contains(&policy("allow", "api.anthropic.com:443")),
            "{commands:?}"
        );
        assert!(
            commands.contains(&policy("allow", "github.com:443")),
            "{commands:?}"
        );
    }

    #[test]
    fn allow_adds_no_rules_for_allowed_domains() {
        let p = paths();
        let config = with_network(NetworkPolicy::Allow, domains(&["github.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(
            !policy_commands(&plan)
                .iter()
                .any(|c| c.last().unwrap().ends_with(":443")),
            "{:?}",
            plan.setup
        );
        assert!(plan.network_rules.is_empty());
    }

    #[test]
    fn wildcard_domains_allow_subdomains_at_any_depth_but_not_the_base() {
        // sbx's `*.d` matches one label and `**.d` any depth plus d itself;
        // workmux's `*.d` matches any depth without d.
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&["*.googleapis.com", "*.github.com", "github.com"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let commands = policy_commands(&plan);
        for expected in [
            policy("allow", "**.googleapis.com:443"),
            policy("deny", "googleapis.com:443"),
            policy("allow", "**.github.com:443"),
            policy("allow", "github.com:443"),
        ] {
            assert!(commands.contains(&expected), "{expected:?} in {commands:?}");
        }
        assert!(
            !commands.contains(&policy("deny", "github.com:443")),
            "github.com is allowed: {commands:?}"
        );
    }

    #[test]
    fn nested_wildcards_dont_deny_a_base_an_outer_wildcard_allows() {
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&["*.example.com", "*.api.example.com"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(commands.contains(&policy("deny", "example.com:443")));
        assert!(
            !commands.contains(&policy("deny", "api.example.com:443")),
            "*.example.com allows api.example.com: {commands:?}"
        );
    }

    fn recorded(decision: RuleDecision, resource: &str) -> NetworkRule {
        NetworkRule {
            decision,
            resource: resource.to_string(),
        }
    }

    fn running_with(rules: Vec<SbxRule>) -> SbxState {
        SbxState {
            sandbox: Some(sandbox("running")),
            rules,
            ..missing()
        }
    }

    const SCOPE: &str = "sandbox:wm-proj-12345678";

    #[test]
    fn reconciling_removes_only_stale_rules_workmux_wrote() {
        let mut p = paths();
        p.record = vec![
            recorded(RuleDecision::Allow, "kept.com:443"),
            recorded(RuleDecision::Allow, "dropped.com:443"),
            recorded(RuleDecision::Allow, "gone-already.com:443"),
        ];
        let state = running_with(vec![
            rule("global", "dropped.com:443"),
            rule(SCOPE, "kept.com:443"),
            rule(SCOPE, "dropped.com:443"),
            rule(SCOPE, "users-own.com:443"),
        ]);
        let config = with_network(NetworkPolicy::Deny, domains(&["kept.com", "new.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(
            commands.contains(&policy("rm", "dropped.com:443")),
            "{commands:?}"
        );
        assert!(
            commands.contains(&policy("allow", "new.com:443")),
            "{commands:?}"
        );
        for untouched in ["kept.com:443", "users-own.com:443", "gone-already.com:443"] {
            assert!(
                !commands.iter().any(|c| c.iter().any(|a| a == untouched)),
                "{untouched} in {commands:?}"
            );
        }
        assert_eq!(
            plan.network_rules,
            vec![
                recorded(RuleDecision::Allow, "kept.com:443"),
                recorded(RuleDecision::Allow, "new.com:443"),
            ]
        );
    }

    #[test]
    fn a_rule_the_user_already_had_stays_theirs() {
        // Present before workmux would have added it: never recorded, so
        // never removed when config drops the domain.
        let p = paths();
        let state = running_with(vec![rule(SCOPE, "github.com:443")]);
        let config = with_network(NetworkPolicy::Deny, domains(&["github.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert!(
            !policy_commands(&plan)
                .iter()
                .any(|c| c.contains(&"github.com:443".to_string()))
        );
        assert!(plan.network_rules.is_empty(), "{:?}", plan.network_rules);
    }

    #[test]
    fn switching_to_allow_removes_every_rule_workmux_wrote() {
        let mut p = paths();
        p.record = vec![
            recorded(RuleDecision::Allow, "**.example.com:443"),
            recorded(RuleDecision::Deny, "example.com:443"),
        ];
        let mut deny = rule(SCOPE, "example.com:443");
        deny.decision = "deny".to_string();
        let state = running_with(vec![rule(SCOPE, "**.example.com:443"), deny]);
        let config = with_network(NetworkPolicy::Allow, domains(&["*.example.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let commands = policy_commands(&plan);
        assert!(
            commands.contains(&policy("rm", "**.example.com:443")),
            "{commands:?}"
        );
        assert!(
            commands.contains(&policy("rm", "example.com:443")),
            "{commands:?}"
        );
        assert!(plan.network_rules.is_empty());
    }

    #[test]
    fn stale_rules_are_removed_before_rules_are_added() {
        // sbx refuses a rule while the same scope holds the opposite
        // decision for its resource.
        let mut p = paths();
        p.record = vec![
            recorded(RuleDecision::Allow, "**.example.com:443"),
            recorded(RuleDecision::Deny, "example.com:443"),
        ];
        let mut deny = rule(SCOPE, "example.com:443");
        deny.decision = "deny".to_string();
        let state = running_with(vec![rule(SCOPE, "**.example.com:443"), deny]);
        let config = with_network(NetworkPolicy::Deny, domains(&["example.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let commands = policy_commands(&plan);
        let removed = commands
            .iter()
            .position(|c| *c == policy("rm", "example.com:443"))
            .expect("deny removed");
        let added = commands
            .iter()
            .position(|c| *c == policy("allow", "example.com:443"))
            .expect("allow added");
        assert!(removed < added, "{commands:?}");
    }

    #[test]
    fn ip_literals_are_refused_as_on_the_container_backend() {
        let p = paths();
        for host in ["10.0.0.1", "::1"] {
            let config = with_network(NetworkPolicy::Deny, domains(&[host]));
            let err = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap_err();
            assert!(
                format!("{err:#}").contains("IP literals not allowed"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn allow_private_ips_needs_no_more_since_sbx_reaches_private_addresses_anyway() {
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            vec![AllowedDomainEntry::Detailed(AllowedDomainDetails {
                host: "git.internal.example".to_string(),
                allow_private_ips: true,
            })],
        );
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.network_rules,
            vec![recorded(RuleDecision::Allow, "git.internal.example:443")]
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn max_connections_gets_a_note_since_sbx_has_no_cap() {
        let p = paths();
        let mut config = with_network(NetworkPolicy::Deny, domains(&["github.com"]));
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        config.network.max_connections = Some(16);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert_eq!(plan.notes.len(), 1);
        assert!(
            plan.notes[0].contains("max_connections"),
            "{:?}",
            plan.notes
        );

        // Under allow it limits nothing on any backend.
        config.network.policy = Some(NetworkPolicy::Allow);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    /// Global rules as sbx's balanced policy lists them: two allowed
    /// destinations, a filesystem rule and the fallback deny.
    fn balanced_global_rules() -> Vec<SbxRule> {
        let mut filesystem = rule("global", "**");
        filesystem.resource_type = "filesystem:read".to_string();
        let mut fallback = rule("global", "**");
        fallback.decision = "deny".to_string();
        let mut balanced = rule("global", "github.com:443");
        balanced.resources.push("**.npmjs.org:443".to_string());
        vec![balanced, filesystem, fallback]
    }

    fn deny_all_global_rules() -> Vec<SbxRule> {
        let mut fallback = rule("global", "**");
        fallback.decision = "deny".to_string();
        vec![fallback]
    }

    fn private_ips_allowed(hosts: &[&str]) -> Vec<AllowedDomainEntry> {
        hosts
            .iter()
            .map(|host| {
                AllowedDomainEntry::Detailed(AllowedDomainDetails {
                    host: host.to_string(),
                    allow_private_ips: true,
                })
            })
            .collect()
    }

    #[test]
    fn deny_warns_on_every_launch_what_the_global_sbx_policy_also_allows() {
        // workmux never writes the global policy, and a sandbox allow can't
        // take back a global one.
        let p = paths();
        let config = with_network(
            NetworkPolicy::Deny,
            private_ips_allowed(&["api.anthropic.com"]),
        );
        for sandbox in [None, Some(sandbox("running"))] {
            let state = SbxState {
                sandbox,
                rules: balanced_global_rules(),
                ..missing()
            };
            let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

            assert!(plan.notes.is_empty(), "{:?}", plan.notes);
            // The global policy's destinations can resolve to private
            // addresses too.
            assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
            let warning = &plan.warnings[0];
            for expected in [
                "2 destinations",
                "sbx policy ls wm-proj-12345678",
                "sbx policy init deny-all",
            ] {
                assert!(warning.contains(expected), "{expected}: {warning}");
            }
        }

        let allow = with_network(NetworkPolicy::Allow, Vec::new());
        let state = SbxState {
            rules: balanced_global_rules(),
            ..missing()
        };
        let plan = launch_plan(&allow, &target(&p), "claude", &state, &exec()).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let deny_all = SbxState {
            rules: deny_all_global_rules(),
            ..missing()
        };
        let plan = launch_plan(&config, &target(&p), "claude", &deny_all, &exec()).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn deny_warns_that_sbx_reaches_private_addresses_allowed_domains_resolve_to() {
        // sbx has no equivalent of allow_private_ips: false. An error would
        // make deny unusable, so it's a warning.
        let p = paths();
        let deny_all = SbxState {
            rules: deny_all_global_rules(),
            ..missing()
        };
        let config = with_network(
            NetworkPolicy::Deny,
            vec![
                AllowedDomainEntry::Plain("api.anthropic.com".to_string()),
                private_ips_allowed(&["git.internal.example"]).remove(0),
            ],
        );
        let plan = launch_plan(&config, &target(&p), "claude", &deny_all, &exec()).unwrap();
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(
            plan.warnings[0].contains("private or loopback")
                && plan.warnings[0].contains("allow_private_ips"),
            "{:?}",
            plan.warnings
        );

        // Nothing allowed reaches a private address unasked.
        let config = with_network(
            NetworkPolicy::Deny,
            private_ips_allowed(&["git.internal.example"]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &deny_all, &exec()).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    /// The rule an agent kit adds when sbx creates the sandbox for it, as
    /// `sbx policy ls <name> --json` lists it.
    fn kit_rule(resources: &[&str]) -> SbxRule {
        SbxRule {
            scope: SCOPE.to_string(),
            decision: "allow".to_string(),
            resource_type: "network".to_string(),
            resources: args(resources),
            provenance: SbxRuleProvenance {
                created_via: "provisioned".to_string(),
            },
        }
    }

    /// Some of the claude kit's hosts, as ticket 16 saw them.
    const CLAUDE_KIT_HOSTS: &[&str] = &[
        "api.anthropic.com:443",
        "claude.com:443",
        "downloads.claude.ai:443",
        "deb.debian.org:80",
    ];

    /// An existing sandbox created under `agent: kit`.
    fn kit_paths() -> Paths {
        Paths {
            created: CreateRecord::Recorded(Box::new(CreateSettings {
                agent_mode: DockerSbxAgent::Kit,
                agent_config_dir: None,
                ..created_settings()
            })),
            ..paths()
        }
    }

    fn kit_network(policy: NetworkPolicy, domains: Vec<AllowedDomainEntry>) -> SandboxConfig {
        let mut config = with_network(policy, domains);
        config.agent_config_dir = None;
        config.docker_sbx.agent = Some(DockerSbxAgent::Kit);
        config
    }

    /// The warnings about kit rules, without the private-address one.
    fn kit_warnings(plan: &LaunchPlan) -> Vec<&String> {
        plan.warnings.iter().filter(|w| w.contains("kit")).collect()
    }

    #[test]
    fn deny_warns_on_every_launch_with_the_kit_hosts_allowed_domains_doesnt_cover() {
        // allowed_domains allows port 443 only, so a kit's `:80` host stays
        // uncovered. workmux denies a wildcard's base (`claude.com:443`),
        // and sbx's deny wins over the kit's allow.
        let mut p = kit_paths();
        p.record = vec![recorded(RuleDecision::Allow, "dropped.com:443")];
        let config = kit_network(
            NetworkPolicy::Deny,
            domains(&["api.anthropic.com", "*.claude.com", "deb.debian.org"]),
        );
        // workmux's own stale rule and the user's rule aren't the kit's.
        let state = running_with(vec![
            kit_rule(CLAUDE_KIT_HOSTS),
            rule(SCOPE, "dropped.com:443"),
            rule(SCOPE, "users-own.com:443"),
        ]);
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        let warnings = kit_warnings(&plan);
        assert_eq!(warnings.len(), 1, "{:?}", plan.warnings);
        let warning = warnings[0];
        for expected in [
            "also allow downloads.claude.ai:443, deb.debian.org:80, which",
            "sbx policy ls wm-proj-12345678",
            "allowed_domains",
        ] {
            assert!(warning.contains(expected), "{expected}: {warning}");
        }
        for unexpected in ["api.anthropic.com", "dropped.com", "users-own.com"] {
            assert!(!warning.contains(unexpected), "{unexpected}: {warning}");
        }
        // workmux never removes them.
        let commands = policy_commands(&plan);
        assert_eq!(
            commands.iter().filter(|c| c[1] == "rm").collect::<Vec<_>>(),
            vec![&policy("rm", "dropped.com:443")],
        );
    }

    #[test]
    fn no_kit_host_warning_under_allow_or_when_allowed_domains_covers_them() {
        let p = kit_paths();
        let state = running_with(vec![kit_rule(&[
            "api.anthropic.com:443",
            "claude.com:443",
            "**.anthropic.com:443",
        ])]);
        let allow = kit_network(NetworkPolicy::Allow, Vec::new());
        let covered = kit_network(
            NetworkPolicy::Deny,
            domains(&["claude.com", "anthropic.com", "*.anthropic.com"]),
        );
        for config in [allow, covered] {
            let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();
            assert!(kit_warnings(&plan).is_empty(), "{:?}", plan.warnings);
        }
    }

    #[test]
    fn kit_hosts_match_allowed_domains_whatever_their_case() {
        let p = kit_paths();
        let state = running_with(vec![kit_rule(&[
            "API.Anthropic.com:443",
            "Claude.COM:443",
            "**.Anthropic.COM:443",
        ])]);
        let config = kit_network(
            NetworkPolicy::Deny,
            domains(&[
                "api.anthropic.COM",
                "CLAUDE.com",
                "Anthropic.com",
                "*.ANTHROPIC.com",
            ]),
        );
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();
        assert!(kit_warnings(&plan).is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_project_cant_move_a_deny_sandbox_onto_a_wider_global_sbx_policy() {
        let p = paths();
        let balanced = SbxState {
            rules: balanced_global_rules(),
            ..missing()
        };
        let mut config = with_network(NetworkPolicy::Deny, domains(&["api.anthropic.com"]));
        config.backend_from_project = true;
        let err = launch_plan(&config, &target(&p), "claude", &balanced, &exec()).unwrap_err();
        let message = format!("{err:#}");
        for expected in [".workmux.yaml", "policy: deny", "sbx policy init deny-all"] {
            assert!(message.contains(expected), "{expected}: {message}");
        }

        // A deny-all global policy matches deny; allow has nothing to keep.
        let deny_all = SbxState {
            rules: deny_all_global_rules(),
            ..missing()
        };
        assert!(launch_plan(&config, &target(&p), "claude", &deny_all, &exec()).is_ok());
        let mut allow = with_network(NetworkPolicy::Allow, Vec::new());
        allow.backend_from_project = true;
        assert!(launch_plan(&allow, &target(&p), "claude", &balanced, &exec()).is_ok());

        // Chosen in the global config, it's the user's call: a warning.
        config.backend_from_project = false;
        let plan = launch_plan(&config, &target(&p), "claude", &balanced, &exec()).unwrap();
        assert!(!plan.warnings.is_empty());
    }

    fn passing(vars: &[(&str, &str)]) -> ExecRequest {
        let mut exec = exec();
        exec.env.extend(
            vars.iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        exec
    }

    #[test]
    fn git_identity_keeps_the_git_config_sbx_sets_in_the_guest() {
        let p = paths();
        let exec = passing(&[
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_VALUE_0", "/code/proj__worktrees/feat"),
            ("GIT_CONFIG_KEY_1", "user.name"),
            ("GIT_CONFIG_VALUE_1", "Me"),
        ]);

        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec).unwrap();

        let env = |key: &str| {
            plan.exec
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(env("GIT_CONFIG_COUNT"), Some("3"));
        assert_eq!(env("GIT_CONFIG_KEY_1"), Some("user.name"));
        assert_eq!(env("GIT_CONFIG_KEY_2"), Some("include.path"));
        assert_eq!(
            env("GIT_CONFIG_VALUE_2"),
            Some("/home/agent/.gitconfig_ignore_scope")
        );
        // Named, never valued, in argv like every other variable.
        let named = |key: &str| {
            plan.exec
                .args
                .windows(2)
                .any(|w| w[0] == "-e" && w[1] == key)
        };
        assert!(named("GIT_CONFIG_KEY_2") && named("GIT_CONFIG_VALUE_2"));
    }

    #[test]
    fn without_git_identity_sbx_keeps_its_own_git_config() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(
            !plan
                .exec
                .env
                .iter()
                .any(|(k, _)| k.starts_with("GIT_CONFIG")),
            "{:?}",
            plan.exec.env
        );
    }

    #[test]
    fn exec_runs_the_command_in_the_pane_cwd_with_env_named_not_valued() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.exec.args,
            args(&[
                "exec",
                "-it",
                "-w",
                "/code/proj__worktrees/feat/sub",
                "-e",
                "WM_RPC_PORT",
                "-e",
                "WM_RPC_TOKEN",
                "-e",
                "WM_SBX_SESSION",
                "wm-proj-12345678",
                "bash",
                "-c",
                "export PATH=/state/docker-sbx/wm-proj-12345678/shims/bin:/home/agent/.local/bin:\
                 /nix/var/nix/profiles/default/bin:\"$PATH\"; \
                 claude --dangerously-skip-permissions",
            ])
        );
        assert!(!plan.exec.args.iter().any(|a| a.contains("secret")));
        assert_eq!(
            plan.exec.env,
            vec![
                ("WM_RPC_PORT".to_string(), "4242".to_string()),
                ("WM_RPC_TOKEN".to_string(), "secret".to_string()),
                ("WM_SBX_SESSION".to_string(), "0123456789abcdef".to_string()),
            ]
        );
    }

    #[test]
    fn several_command_arguments_reach_the_guest_without_being_split_again() {
        // `workmux sandbox shell -- bash -c 'set -x; echo "a b"'`
        let p = paths();
        let words = ExecRequest {
            command: args(&["bash", "-c", "set -x; echo \"a b\""]),
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &words).unwrap();
        let line = plan.exec.args.last().unwrap();
        assert!(line.ends_with("; bash -c 'set -x; echo \"a b\"'"), "{line}");
    }

    fn exec_line(plan: &LaunchPlan) -> &str {
        plan.exec.args.last().unwrap()
    }

    fn with_toolchain(toolchain: DetectedToolchain) -> Paths {
        Paths {
            toolchain,
            ..paths()
        }
    }

    #[test]
    fn agent_command_runs_in_the_worktrees_toolchain_as_on_lima() {
        let p = with_toolchain(DetectedToolchain::Flake);
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(
            exec_line(&plan).ends_with(
                "; nix develop --command bash -c 'claude --dangerously-skip-permissions'"
            ),
            "{}",
            exec_line(&plan)
        );

        let p = with_toolchain(DetectedToolchain::Devbox);
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(
            exec_line(&plan).ends_with(
                "devbox run -c \"$_WM_CACHE\" -- 'cd \"$_WM_CWD\" && \
                 claude --dangerously-skip-permissions'"
            ),
            "{}",
            exec_line(&plan)
        );

        let p = with_toolchain(DetectedToolchain::None);
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(
            exec_line(&plan).ends_with("\"$PATH\"; claude --dangerously-skip-permissions"),
            "{}",
            exec_line(&plan)
        );
    }

    #[test]
    fn sandbox_shell_runs_outside_the_toolchain_as_on_lima() {
        let p = with_toolchain(DetectedToolchain::Flake);
        let shell = ExecRequest {
            command: args(&["bash"]),
            in_toolchain: false,
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &shell).unwrap();
        assert!(
            exec_line(&plan).ends_with("\"$PATH\"; bash"),
            "{}",
            exec_line(&plan)
        );
    }

    #[test]
    fn exec_extends_the_images_path_keeping_the_shims_first() {
        // An image's own PATH entries (omp's /opt/bun/bin) stay reachable.
        let p = Paths {
            state: PathBuf::from("/Users/me/Library/State dir/wm-proj-12345678"),
            ..paths()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(
            !plan.exec.args.iter().any(|a| a.starts_with("PATH=")),
            "{:?}",
            plan.exec.args
        );
        assert_eq!(
            plan.exec.args.last().unwrap(),
            "export PATH='/Users/me/Library/State dir/wm-proj-12345678/shims/bin':\
             /home/agent/.local/bin:/nix/var/nix/profiles/default/bin:\"$PATH\"; \
             claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn exec_asks_for_a_tty_only_when_stdin_is_a_terminal() {
        // sbx's `exec -it` fails with piped stdin ("context deadline
        // exceeded"), as when `workmux sandbox shell -- <cmd>` runs from a
        // script.
        let p = paths();
        let piped = ExecRequest {
            tty: false,
            ..exec()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &piped).unwrap();
        assert_eq!(plan.exec.args[..3], args(&["exec", "-i", "-w"]));

        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert_eq!(plan.exec.args[..3], args(&["exec", "-it", "-w"]));
    }

    #[test]
    fn hangup_ends_only_the_processes_of_the_panes_session() {
        // sbx leaves the guest command running when its `sbx exec` client
        // hangs up, so the supervisor hangs up the session's processes itself.
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.hangup.args,
            args(&[
                "exec",
                "wm-proj-12345678",
                "sh",
                "-c",
                "for p in /proc/[0-9]*; do \
                 grep -qxzF WM_SBX_SESSION=0123456789abcdef \"$p/environ\" 2>/dev/null \
                 && kill -HUP \"${p#/proc/}\"; done; true",
            ])
        );
        assert!(plan.hangup.env.is_empty());
    }

    #[test]
    fn stopped_sandbox_needs_no_start_step_since_exec_starts_it() {
        let p = paths();
        let stopped = SbxState {
            sandbox: Some(sandbox("stopped")),
            rules: vec![rule("sandbox:wm-proj-12345678", "localhost:4242")],
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &stopped, &exec()).unwrap();

        assert!(plan.setup.is_empty(), "{:?}", plan.setup);
        assert_eq!(plan.exec.args[..2], args(&["exec", "-it"]));
    }

    fn failing(check: &str) -> SbxState {
        let mut state = missing();
        for c in &mut state.checks {
            if c.name == check {
                c.status = "fail".to_string();
            }
        }
        state
    }

    fn plan_error(state: &SbxState, agent: &str) -> String {
        let p = paths();
        launch_plan(&config(), &target(&p), agent, state, &exec())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn logged_out_sbx_fails_naming_sbx_login() {
        let message = plan_error(&failing("Authentication"), "claude");
        assert!(message.contains("sbx login"), "{message}");
    }

    #[test]
    fn uninitialised_global_policy_fails_naming_sbx_policy_init() {
        let message = plan_error(&failing("Global network policy"), "claude");
        assert!(message.contains("sbx policy init"), "{message}");
    }

    #[test]
    fn other_failing_sbx_checks_are_warnings_naming_the_check_and_its_hint() {
        let mut state = missing();
        state.checks.push(SbxCheck {
            name: "Disk space".to_string(),
            status: "fail".to_string(),
            message: "1.3GiB free".to_string(),
            hint: "Free up disk space on the state directory's volume".to_string(),
        });
        state.checks.push(SbxCheck {
            name: "Binary version".to_string(),
            status: "warn".to_string(),
            message: "update available".to_string(),
            hint: "Download the latest release".to_string(),
        });
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        let warning = &plan.warnings[0];
        assert!(warning.contains("Disk space"), "{warning}");
        assert!(warning.contains("1.3GiB free"), "{warning}");
        assert!(
            warning.contains("Free up disk space on the state directory's volume"),
            "{warning}"
        );
    }

    #[test]
    fn unsupported_hosts_fail_naming_supported_platforms() {
        for (os, arch, kvm) in [
            ("macos", "x86_64", false),
            ("linux", "x86_64", false),
            ("freebsd", "x86_64", true),
        ] {
            let state = SbxState {
                platform: Platform {
                    os: os.to_string(),
                    arch: arch.to_string(),
                    kvm,
                },
                ..missing()
            };
            let message = plan_error(&state, "claude");
            assert!(message.contains("Apple silicon"), "{os}/{arch}: {message}");
        }

        let linux_with_kvm = SbxState {
            platform: Platform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                kvm: true,
            },
            ..missing()
        };
        let p = paths();
        assert!(launch_plan(&config(), &target(&p), "claude", &linux_with_kvm, &exec()).is_ok());
    }

    fn setup_args(plan: &LaunchPlan) -> Vec<Vec<String>> {
        plan.setup.iter().map(|c| c.args.clone()).collect()
    }

    #[test]
    fn each_agents_config_dir_is_mounted_at_its_usual_place() {
        for (agent, home_path) in [
            ("claude", "/home/agent/.claude"),
            ("copilot", "/home/agent/.copilot"),
            ("gemini", "/home/agent/.gemini"),
            ("agy", "/home/agent/.gemini/antigravity-cli"),
            ("codex", "/home/agent/.codex"),
            ("opencode", "/home/agent/.local/share/opencode"),
            ("pi", "/home/agent/.pi/agent"),
            ("omp", "/home/agent/.omp/agent"),
        ] {
            let p = paths();
            let plan = launch_plan(&config(), &target(&p), agent, &missing(), &exec()).unwrap();
            let workspace = format!("/home/me/.{agent}:{home_path}");

            assert!(
                workspaces(&plan).contains(&workspace),
                "{agent}: {:?}",
                plan.create
            );
            assert!(
                !setup_args(&plan)
                    .iter()
                    .any(|a| a.contains(&"ln".to_string())),
                "{agent}: {:?}",
                plan.setup
            );
        }
    }

    #[test]
    fn created_claude_sandbox_is_seeded_with_completed_onboarding() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let seed = args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "sh",
            "-c",
            "[ -e \"$1\" ] || { printf %s \"$2\" > \"$1\" && chown agent:agent \"$1\"; }",
            "sh",
            "/home/agent/.claude.json",
            r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
        ]);
        assert!(setup_args(&plan).contains(&seed), "{:?}", plan.setup);

        let plan = launch_plan(&config(), &target(&p), "codex", &missing(), &exec()).unwrap();
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.iter().any(|w| w.contains(".claude.json"))),
            "{:?}",
            plan.setup
        );
    }

    #[test]
    fn under_kit_a_claude_sandbox_is_created_for_sbxs_claude_kit_without_its_config_dir() {
        // The kit installs Claude in the sandbox's own home and sbx holds the
        // credential; mounting the workstation's config would hand it over.
        let p = paths();
        let plan = launch_plan(&kit_config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let tag = super::super::template::template_tag("test-image:latest");
        assert_eq!(
            create_args(&plan)[..10],
            args(&[
                "create",
                "--name",
                "wm-proj-12345678",
                "--no-progress",
                "--template",
                &tag,
                "--pull",
                "never",
                "claude",
                "/code/proj__worktrees/feat",
            ])
        );
        assert!(
            !workspaces(&plan).iter().any(|w| w.contains("/home/agent")),
            "{:?}",
            plan.create
        );
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.contains(&"root".to_string())),
            "{:?}",
            plan.setup
        );
        assert_eq!(plan.create_settings.agent_config_dir, None);
    }

    #[test]
    fn under_kit_each_agent_sbx_has_a_kit_for_is_created_for_its_kit_without_workstation_config() {
        // Each kit writes its agent's config into the sandbox's own home
        // (OpenCode's into ~/.config/opencode), so nothing of the
        // workstation's is mounted there.
        let p = paths();
        let opencode_config = PathBuf::from("/home/me/.config/opencode");
        let target = LaunchTarget {
            opencode_config_dir: Some(&opencode_config),
            ..target(&p)
        };
        for agent in ["codex", "gemini", "opencode"] {
            let plan = launch_plan(&kit_config(), &target, agent, &missing(), &exec()).unwrap();

            assert_eq!(create_args(&plan)[8], agent, "{agent}");
            assert!(
                !workspaces(&plan).iter().any(|w| w.contains("/home/")),
                "{agent}: {:?}",
                plan.create
            );
            assert!(
                !setup_args(&plan)
                    .iter()
                    .any(|a| a.contains(&"root".to_string())),
                "{agent}: {:?}",
                plan.setup
            );
            assert_eq!(plan.create_settings.agent_config_dir, None, "{agent}");
            assert_eq!(plan.create_settings.opencode_config_dir, None, "{agent}");
        }
    }

    #[test]
    fn under_kit_copilot_needs_an_image_that_provides_it() {
        // The copilot kit configures Copilot CLI but the binary comes from
        // the template, and workmux publishes no Copilot image.
        let p = paths();
        let default_image = SandboxConfig {
            image: None,
            ..kit_config()
        };
        let err = launch_plan(&default_image, &target(&p), "copilot", &missing(), &exec())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sandbox.image") && err.contains("copilot"),
            "{err}"
        );

        let plan = launch_plan(&kit_config(), &target(&p), "copilot", &missing(), &exec()).unwrap();
        assert_eq!(create_args(&plan)[8], "copilot");
        assert!(
            !workspaces(&plan).iter().any(|w| w.contains("/home/")),
            "{:?}",
            plan.create
        );
    }

    #[test]
    fn under_kit_agent_config_dir_is_ignored_with_a_note() {
        // It applies to `shell` and the other backends, where it's usually
        // set globally; the kit's sandbox has its own config.
        let p = paths();
        let config = SandboxConfig {
            agent_config_dir: Some("/home/me/.{agent}".to_string()),
            ..kit_config()
        };
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(
            !workspaces(&plan).iter().any(|w| w.contains("/home/me")),
            "{:?}",
            plan.create
        );
        assert_eq!(plan.create_settings.agent_config_dir, None);
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("agent_config_dir") && n.contains("ignored")),
            "{:?}",
            plan.notes
        );
        let plan = launch_plan(&kit_config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(
            !plan.notes.iter().any(|n| n.contains("agent_config_dir")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn under_kit_agents_sbx_has_no_kit_for_fail_naming_agent_shell() {
        // Fail closed rather than mount the config under `kit`.
        let p = paths();
        for agent in ["pi", "omp", "agy", "default"] {
            let err = launch_plan(&kit_config(), &target(&p), agent, &missing(), &exec())
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(agent) && err.contains("sandbox.docker_sbx.agent: shell"),
                "{err}"
            );
        }
    }

    /// The setup commands that edit Claude's settings in the guest.
    fn claude_settings_edits(plan: &LaunchPlan) -> Vec<Vec<String>> {
        setup_args(plan)
            .into_iter()
            .filter(|a| a.iter().any(|w| w.contains(".claude/settings.json")))
            .collect()
    }

    #[test]
    fn under_kit_a_created_claude_sandbox_gets_workmuxs_status_hooks_and_keeps_the_kits_onboarding()
    {
        // The workstation's hooks aren't mounted under `kit`; workmux's
        // status hooks go into the settings the kit wrote, as the agent.
        // The kit already completes onboarding and trusts the workspace.
        let p = paths();
        let plan = launch_plan(&kit_config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let edits = claude_settings_edits(&plan);
        assert_eq!(edits.len(), 1, "{:?}", plan.setup);
        let edit = &edits[0];
        assert_eq!(edit[..4], args(&["exec", "wm-proj-12345678", "sh", "-c"]));
        let plugin: serde_json::Value =
            serde_json::from_str(include_str!("../../../.claude-plugin/plugin.json")).unwrap();
        assert!(
            edit.iter().any(
                |w| serde_json::from_str::<serde_json::Value>(w).ok().as_ref()
                    == Some(&plugin["hooks"])
            ),
            "{edit:?}"
        );
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.iter().any(|w| w.contains(".claude.json"))),
            "{:?}",
            plan.setup
        );

        // Only when the sandbox is created: the kit writes its settings then.
        let mut existing = paths();
        existing.created = CreateRecord::Recorded(Box::new(plan.create_settings.clone()));
        let plan = launch_plan(
            &kit_config(),
            &target(&existing),
            "claude",
            &running_with(Vec::new()),
            &exec(),
        )
        .unwrap();
        assert!(claude_settings_edits(&plan).is_empty(), "{:?}", plan.setup);

        // Under `shell`, Claude has the workstation's own settings.
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(claude_settings_edits(&plan).is_empty(), "{:?}", plan.setup);
    }

    /// The agent-user setup commands one of whose words contains `needle`.
    fn agent_setup_with(plan: &LaunchPlan, needle: &str) -> Vec<Vec<String>> {
        setup_args(plan)
            .into_iter()
            .filter(|a| a[..2] == args(&["exec", "wm-proj-12345678"]))
            .filter(|a| a.iter().any(|w| w.contains(needle)))
            .collect()
    }

    /// Whether some word of `command` is JSON equal to `value`.
    fn passes_json(command: &[String], value: &serde_json::Value) -> bool {
        command
            .iter()
            .any(|w| serde_json::from_str::<serde_json::Value>(w).ok().as_ref() == Some(value))
    }

    fn embedded_hooks(source: &str) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(source).unwrap()["hooks"].clone()
    }

    #[test]
    fn under_kit_every_kit_agent_gets_workmuxs_status_hooks_in_the_config_its_kit_wrote() {
        // As for Claude: the workstation's hooks aren't there, so workmux's
        // go into the sandbox's own config, as the agent, on create.
        let p = paths();
        let plan_for = |agent: &str| {
            launch_plan(&kit_config(), &target(&p), agent, &missing(), &exec()).unwrap()
        };

        let codex = plan_for("codex");
        let hooks = agent_setup_with(&codex, ".codex/hooks.json");
        assert_eq!(hooks.len(), 1, "{:?}", codex.setup);
        assert!(
            passes_json(
                &hooks[0],
                &embedded_hooks(include_str!(
                    "../../../resources/codex/hooks/workmux-status.json"
                ))
            ),
            "{hooks:?}"
        );
        // Codex reads hooks.json only with its hooks feature on.
        assert_eq!(
            agent_setup_with(&codex, ".codex/config.toml").len(),
            1,
            "{:?}",
            codex.setup
        );

        let gemini = plan_for("gemini");
        let hooks = agent_setup_with(&gemini, ".gemini/settings.json");
        assert_eq!(hooks.len(), 1, "{:?}", gemini.setup);
        assert!(
            passes_json(
                &hooks[0],
                &embedded_hooks(include_str!("../../../resources/gemini/settings.json"))
            ),
            "{hooks:?}"
        );

        let copilot = plan_for("copilot");
        let hooks = agent_setup_with(&copilot, ".copilot/hooks/workmux-status.json");
        assert_eq!(hooks.len(), 1, "{:?}", copilot.setup);
        assert!(
            hooks[0].contains(
                &include_str!("../../../resources/copilot/hooks/workmux-status/hooks.json")
                    .to_string()
            ),
            "{hooks:?}"
        );

        // OpenCode's plugin layout follows its version, found in the guest.
        let opencode = plan_for("opencode");
        let plugin = agent_setup_with(&opencode, ".config/opencode/plugins");
        assert_eq!(plugin.len(), 1, "{:?}", opencode.setup);
        for source in [
            include_str!("../../../resources/opencode/plugins/workmux-status.ts"),
            include_str!("../../../resources/opencode/v2/workmux-status/index.ts"),
            include_str!("../../../resources/opencode/v2/workmux-status/tui.ts"),
            include_str!("../../../resources/opencode/v2/workmux-status/status.ts"),
        ] {
            assert!(plugin[0].contains(&source.to_string()), "{plugin:?}");
        }

        // Only on create: the kit writes its config then.
        for agent in ["codex", "gemini", "copilot", "opencode"] {
            let mut existing = paths();
            existing.created = CreateRecord::Recorded(Box::new(plan_for(agent).create_settings));
            let plan = launch_plan(
                &kit_config(),
                &target(&existing),
                agent,
                &running_with(Vec::new()),
                &exec(),
            )
            .unwrap();
            assert!(
                agent_setup_with(&plan, "workmux").is_empty(),
                "{agent}: {:?}",
                plan.setup
            );
        }
    }

    #[test]
    fn codex_kit_config_gets_hooks_on_and_loses_the_notify_helper_the_template_lacks() {
        // Run the script as the guest would, on the config the codex kit
        // writes (sbx v0.51), with a home of its own.
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".codex/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let kit = "approval_policy = \"never\"\n\
                   notify = [\"/usr/local/bin/sbx-codex-hook\", \"codex\"]\n\
                   model_provider = \"sandboxd\"\n\
                   \n\
                   [model_providers.sandboxd]\n\
                   name = \"Sandbox Proxy\"\n";
        let run = |content: &str| {
            std::fs::write(&config, content).unwrap();
            let status = std::process::Command::new("sh")
                .args(["-c", CODEX_KIT_CONFIG_SCRIPT])
                .env("HOME", home.path())
                .status()
                .unwrap();
            assert!(status.success());
            std::fs::read_to_string(&config).unwrap()
        };

        let edited: toml_edit::DocumentMut = run(kit).parse().unwrap();
        assert_eq!(edited["features"]["hooks"].as_bool(), Some(true));
        assert!(!edited.contains_key("notify"), "{edited}");
        assert_eq!(edited["model_provider"].as_str(), Some("sandboxd"));
        assert_eq!(
            edited["model_providers"]["sandboxd"]["name"].as_str(),
            Some("Sandbox Proxy")
        );

        // A features table already there gets hooks on once.
        let edited: toml_edit::DocumentMut = run("[features]\nhooks = false\nweb_search = true\n")
            .parse()
            .unwrap();
        assert_eq!(edited["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(edited["features"]["web_search"].as_bool(), Some(true));
    }

    #[test]
    fn opencode_global_config_dir_is_mounted_read_only_at_its_usual_place() {
        let p = paths();
        let opencode_config = PathBuf::from("/home/me/.config/opencode");
        let target = LaunchTarget {
            opencode_config_dir: Some(&opencode_config),
            ..target(&p)
        };
        let plan = launch_plan(&config(), &target, "opencode", &missing(), &exec()).unwrap();

        assert!(
            create_args(&plan)
                .contains(&"/home/me/.config/opencode:/home/agent/.config/opencode:ro".to_string()),
            "{:?}",
            plan.create
        );
        let setup = setup_args(&plan);
        assert!(
            setup.contains(&args(&[
                "exec",
                "-u",
                "root",
                "wm-proj-12345678",
                "install",
                "-d",
                "-o",
                "agent",
                "-g",
                "agent",
                "/home/agent/.local",
                "/home/agent/.local/share",
                "/home/agent/.config",
            ])),
            "{setup:?}"
        );

        // Only OpenCode gets it.
        let plan = launch_plan(&config(), &target, "codex", &missing(), &exec()).unwrap();
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.iter().any(|w| w.contains(".config/opencode"))),
            "{:?}",
            plan.setup
        );
    }

    fn bind_guest_dir(host: &str, guest: &str) -> Vec<String> {
        args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "sh",
            "-c",
            "mountpoint -q \"$1\" || { install -d -o agent -g agent \"$2\" \
             && mkdir -p \"$1\" && mount --bind \"$2\" \"$1\"; }",
            "sh",
            host,
            guest,
        ])
    }

    #[test]
    fn pi_and_omp_download_their_binaries_into_a_guest_local_dir_on_every_launch() {
        // They download Linux fd/rg into the mounted config dir, over the
        // workstation's own binaries. A guest bind mount covers that dir; it
        // doesn't survive sbx stopping the sandbox, so every launch makes it.
        let p = paths();
        let existing = SbxState {
            sandbox: Some(sandbox("stopped")),
            rules: vec![rule("sandbox:wm-proj-12345678", "localhost:4242")],
            ..missing()
        };
        for (agent, host, guest) in [
            (
                "pi",
                "/home/agent/.pi/agent/bin",
                "/var/lib/workmux/pi-agent-bin",
            ),
            (
                "omp",
                "/home/agent/.omp/agent/tools",
                "/var/lib/workmux/omp-agent-tools",
            ),
        ] {
            for state in [missing(), existing.clone()] {
                let plan = launch_plan(&config(), &target(&p), agent, &state, &exec()).unwrap();
                assert!(
                    setup_args(&plan).contains(&bind_guest_dir(host, guest)),
                    "{agent}: {:?}",
                    plan.setup
                );
            }
        }

        let plan = launch_plan(&config(), &target(&p), "codex", &existing, &exec()).unwrap();
        assert!(plan.setup.is_empty(), "{:?}", plan.setup);
    }

    #[test]
    fn nested_mounts_get_parent_dirs_the_agent_owns() {
        // sbx creates a mount's missing parents as root; the agent couldn't
        // write elsewhere under them (~/.local/state).
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "opencode", &missing(), &exec()).unwrap();
        let setup = setup_args(&plan);
        let parents = args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "install",
            "-d",
            "-o",
            "agent",
            "-g",
            "agent",
            "/home/agent/.local",
            "/home/agent/.local/share",
        ]);
        assert!(setup.contains(&parents), "{setup:?}");

        let plan = launch_plan(&config(), &target(&p), "codex", &missing(), &exec()).unwrap();
        assert!(
            !setup_args(&plan)
                .iter()
                .any(|a| a.contains(&"install".to_string())),
            "{:?}",
            plan.setup
        );
    }

    #[test]
    fn agents_without_a_config_dir_get_no_mount() {
        let p = paths();
        let config = SandboxConfig {
            agent_config_dir: None,
            ..config()
        };
        let plan = launch_plan(&config, &target(&p), "default", &missing(), &exec()).unwrap();

        assert!(
            !workspaces(&plan).iter().any(|w| w.starts_with("/home/me")),
            "{:?}",
            plan.create
        );
        assert!(
            !plan
                .setup
                .iter()
                .any(|c| c.args.contains(&"install".to_string())),
            "{:?}",
            plan.setup
        );
    }

    /// The `sbx create` command's arguments, for a plan that creates the
    /// sandbox.
    fn create_args(plan: &LaunchPlan) -> &[String] {
        &plan
            .create
            .as_ref()
            .expect("plan creates the sandbox")
            .command
            .args
    }

    fn workspaces(plan: &LaunchPlan) -> Vec<String> {
        plan.create
            .as_ref()
            .expect("plan creates the sandbox")
            .workspaces
            .clone()
    }

    #[test]
    fn main_git_dir_is_read_only_but_for_what_commits_write() {
        // As on the container backend: hooks, config and the rest of the git
        // dir are read-only; objects, refs, logs and the worktree's own admin
        // dir stay writable, with the admin dir's pointers read-only again.
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            workspaces(&plan),
            args(&[
                "/code/proj__worktrees/feat",
                "/code/proj/.git:ro",
                "/code/proj/.git/objects",
                "/code/proj/.git/refs",
                "/code/proj/.git/logs",
                "/code/proj/.git/worktrees/feat",
                "/code/proj/.git/objects/info:ro",
                "/code/proj/.git/worktrees/feat/config.worktree:ro",
                "/code/proj/.git/worktrees/feat/gitdir:ro",
                "/code/proj/.git/worktrees/feat/commondir:ro",
                "/code/proj__worktrees/feat/.git:ro",
                "/home/me/.claude:/home/agent/.claude",
                "/state/docker-sbx/wm-proj-12345678:ro",
            ])
        );
    }

    #[test]
    fn submodule_git_dirs_are_writable_with_their_control_files_read_only() {
        let mut p = paths();
        p.git.module_roots = vec![PathBuf::from("/code/proj/.git/modules/vendor")];
        p.git
            .pointer_files
            .push(PathBuf::from("/code/proj__worktrees/feat/vendor/.git"));
        p.git.config_paths = vec![PathBuf::from("/code/proj__worktrees/feat/extra.gitconfig")];
        p.git.rr_cache = true;
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();
        let workspaces = workspaces(&plan);

        for expected in [
            "/code/proj/.git/rr-cache",
            "/code/proj/.git/modules/vendor",
            "/code/proj/.git/modules/vendor/hooks:ro",
            "/code/proj/.git/modules/vendor/info:ro",
            "/code/proj/.git/modules/vendor/objects/info:ro",
            "/code/proj/.git/modules/vendor/modules:ro",
            "/code/proj/.git/modules/vendor/worktrees:ro",
            "/code/proj/.git/modules/vendor/config:ro",
            "/code/proj/.git/modules/vendor/config.worktree:ro",
            "/code/proj__worktrees/feat/vendor/.git:ro",
            "/code/proj__worktrees/feat/extra.gitconfig:ro",
        ] {
            assert!(
                workspaces.contains(&expected.to_string()),
                "{expected}: {workspaces:?}"
            );
        }
    }

    fn config_error(config: &SandboxConfig) -> String {
        let p = paths();
        launch_plan(config, &target(&p), "claude", &missing(), &exec())
            .unwrap_err()
            .to_string()
    }

    /// `paths()` with a real worktree holding `files`, as excluded files
    /// are looked for on the host.
    fn worktree_with(files: &[&str]) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().canonicalize().unwrap();
        for file in files {
            let path = worktree.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "SECRET=1\n").unwrap();
        }
        let p = Paths {
            cwd: worktree.clone(),
            worktree,
            ..paths()
        };
        (dir, p)
    }

    fn excluding(files: &[&str]) -> SandboxConfig {
        let mut config = config();
        config.docker_sbx.excluded_files = Some(files.iter().map(|f| f.to_string()).collect());
        config
    }

    #[test]
    fn container_excluded_files_have_no_effect_under_docker_sbx() {
        // `sandbox.container.*` is the container backend's namespace.
        let (_dir, p) = worktree_with(&[".env"]);
        let mut config = config();
        config.container.excluded_files = Some(vec![".env".to_string()]);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(plan.masks.is_empty(), "{:?}", plan.masks);
        assert!(plan.create_settings.excluded_files.is_empty());
    }

    #[test]
    fn excluded_files_are_masked_read_only_with_an_empty_file_from_the_state_dir() {
        let (_dir, p) = worktree_with(&[".env", "config/.env.local"]);
        let config = excluding(&[".env", "config/.env.local"]);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let env = p.worktree.join(".env");
        let local = p.worktree.join("config/.env.local");
        assert_eq!(
            plan.masks,
            vec![Mask::empty(env.clone()), Mask::empty(local.clone())]
        );
        let workspaces = workspaces(&plan);
        for target in [&env, &local] {
            let source = Mask::empty(target.clone()).source(&p.state);
            assert!(source.starts_with(&p.state), "{}", source.display());
            let expected = format!("{}:{}:ro", source.display(), target.display());
            assert!(workspaces.contains(&expected), "{expected}: {workspaces:?}");
        }
    }

    #[test]
    fn only_existing_files_inside_the_worktree_are_masked_as_on_the_container_backend() {
        // A mask over a missing path would make sbx create it in the
        // worktree on the workstation; directories can't be masked; paths
        // outside the worktree are refused.
        let (_dir, p) = worktree_with(&[".env", "secrets/key"]);
        let config = excluding(&[
            ".env.missing",
            "secrets",
            "/etc/hosts",
            "../outside/.env",
            "sub/../.env",
            ".env",
        ]);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(plan.masks, vec![Mask::empty(p.worktree.join(".env"))]);
        assert!(!p.worktree.join(".env.missing").exists());
    }

    #[test]
    fn a_symlinked_excluded_file_masks_the_file_it_resolves_to_in_the_worktree() {
        // The guest resolves the link itself, so the mask goes over the
        // file it reads. The main worktree, where the container backend
        // also masks the path, isn't mounted under docker_sbx: a link into
        // it doesn't resolve in the sandbox.
        let (_dir, p) = worktree_with(&["config/real.env"]);
        let main = tempfile::tempdir().unwrap();
        std::fs::write(main.path().join(".env.main"), "SECRET=1\n").unwrap();
        std::os::unix::fs::symlink("config/real.env", p.worktree.join(".env")).unwrap();
        std::os::unix::fs::symlink(main.path().join(".env.main"), p.worktree.join(".env.main"))
            .unwrap();
        let config = excluding(&[".env", "config/real.env", ".env.main"]);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.masks,
            vec![Mask::empty(p.worktree.join("config/real.env"))]
        );
    }

    #[test]
    fn an_excluded_file_the_sandbox_wasnt_created_masking_is_an_error() {
        // A file listed or created since the sandbox was created is
        // readable to the agent until the sandbox is created again: fail
        // closed, as the container backend masks every launch.
        let (_dir, mut p) = worktree_with(&[".env", ".env.local"]);
        let env = p.worktree.join(".env");
        let local = p.worktree.join(".env.local");
        p.created = CreateRecord::Recorded(Box::new(CreateSettings {
            excluded_files: vec![env.clone()],
            ..created_settings()
        }));
        let state = running_with(Vec::new());

        let plan = launch_plan(
            &excluding(&[".env"]),
            &target(&p),
            "claude",
            &state,
            &exec(),
        )
        .unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let config = excluding(&[".env", ".env.local"]);
        let error = launch_plan(&config, &target(&p), "claude", &state, &exec())
            .unwrap_err()
            .to_string();
        for expected in [
            local.display().to_string().as_str(),
            "sandbox.docker_sbx.excluded_files",
            "sbx rm -f wm-proj-12345678",
        ] {
            assert!(error.contains(expected), "{expected}: {error}");
        }

        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert_eq!(plan.create_settings.excluded_files, vec![env, local]);
    }

    #[test]
    fn an_excluded_file_masked_at_create_and_deleted_since_warns_how_to_recreate() {
        // sbx puts an empty file back where it was; nothing is readable.
        let (_dir, mut p) = worktree_with(&[".env"]);
        p.created = CreateRecord::Recorded(Box::new(CreateSettings {
            excluded_files: vec![p.worktree.join(".env"), p.worktree.join(".env.old")],
            ..created_settings()
        }));
        let config = excluding(&[".env", ".env.old"]);
        let state = running_with(Vec::new());
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        let warning = &plan.warnings[0];
        assert!(warning.contains("(docker_sbx.excluded_files)"), "{warning}");
        assert!(warning.contains("workmux sandbox prune"), "{warning}");
    }

    #[test]
    fn excluded_files_are_masked_only_when_the_sandbox_is_created() {
        let (_dir, mut p) = worktree_with(&[".env"]);
        p.created = CreateRecord::Recorded(Box::new(CreateSettings {
            excluded_files: vec![p.worktree.join(".env")],
            ..created_settings()
        }));
        let plan = launch_plan(
            &excluding(&[".env"]),
            &target(&p),
            "claude",
            &running_with(Vec::new()),
            &exec(),
        )
        .unwrap();

        assert!(plan.masks.is_empty(), "{:?}", plan.masks);
    }

    fn with_extra_mounts(mounts: Vec<ExtraMount>) -> SandboxConfig {
        SandboxConfig {
            extra_mounts: Some(mounts),
            ..config()
        }
    }

    #[test]
    fn extra_mounts_are_workspaces_read_only_unless_writable() {
        let config = with_extra_mounts(vec![
            ExtraMount::Path("/data/notes".to_string()),
            ExtraMount::Spec {
                host_path: "/data/scratch".to_string(),
                guest_path: Some("/data/scratch".to_string()),
                writable: Some(true),
            },
            ExtraMount::Spec {
                host_path: "/data/reference".to_string(),
                guest_path: None,
                writable: None,
            },
        ]);
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let workspaces = workspaces(&plan);
        assert_eq!(
            workspaces[workspaces.len() - 3..],
            args(&["/data/notes:ro", "/data/scratch", "/data/reference:ro"])
        );
    }

    #[test]
    fn extra_mounts_at_a_different_guest_path_mount_there() {
        let config = with_extra_mounts(vec![
            ExtraMount::Spec {
                host_path: "/data/notes".to_string(),
                guest_path: Some("/mnt/notes".to_string()),
                writable: None,
            },
            ExtraMount::Spec {
                host_path: "/data/scratch".to_string(),
                guest_path: Some("/mnt/scratch".to_string()),
                writable: Some(true),
            },
        ]);
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let workspaces = workspaces(&plan);
        assert_eq!(
            workspaces[workspaces.len() - 2..],
            args(&["/data/notes:/mnt/notes:ro", "/data/scratch:/mnt/scratch"])
        );
    }

    #[test]
    fn writable_extra_mount_of_a_file_is_an_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let host_path = file.path().to_string_lossy().into_owned();
        let config = with_extra_mounts(vec![ExtraMount::Spec {
            host_path: host_path.clone(),
            guest_path: None,
            writable: Some(true),
        }]);

        let message = config_error(&config);
        assert!(message.contains(&host_path), "{message}");
        assert!(message.contains("read-only"), "{message}");

        // Read-only, the file mounts.
        let config = with_extra_mounts(vec![ExtraMount::Path(host_path.clone())]);
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();
        assert!(workspaces(&plan).contains(&format!("{host_path}:ro")));
    }

    #[test]
    fn writable_extra_mount_cannot_overlap_git_metadata() {
        for host_path in [
            "/code/proj/.git/hooks",
            "/code",
            "/code/proj__worktrees/feat/.git",
        ] {
            let config = with_extra_mounts(vec![ExtraMount::Spec {
                host_path: host_path.to_string(),
                guest_path: None,
                writable: Some(true),
            }]);

            let message = config_error(&config);
            assert!(
                message.contains("protected Git metadata"),
                "{host_path}: {message}"
            );
        }

        // Read-only, it can.
        let config = with_extra_mounts(vec![ExtraMount::Path("/code".to_string())]);
        let p = paths();
        assert!(launch_plan(&config, &target(&p), "claude", &missing(), &exec()).is_ok());
    }

    fn with_docker_sbx(edit: impl FnOnce(&mut DockerSbxConfig)) -> SandboxConfig {
        let mut config = config();
        edit(&mut config.docker_sbx);
        config
    }

    /// The flags `sbx create` gets before the agent.
    fn create_flags(plan: &LaunchPlan) -> Vec<String> {
        create_args(plan)
            .iter()
            .take_while(|a| *a != "shell")
            .cloned()
            .collect()
    }

    #[test]
    fn created_sandbox_gets_the_configured_cpus_memory_and_kits() {
        let config = with_docker_sbx(|sbx| {
            sbx.cpus = Some(8);
            sbx.memory = Some("8g".to_string());
            sbx.kits = Some(vec![
                "docker.io/sbx/task:latest".to_string(),
                "./my-mixin/".to_string(),
            ]);
        });
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let flags = create_flags(&plan);
        let tag = super::super::template::template_tag("test-image:latest");
        assert_eq!(
            flags,
            args(&[
                "create",
                "--name",
                "wm-proj-12345678",
                "--no-progress",
                "--template",
                &tag,
                "--pull",
                "never",
                "--cpus",
                "8",
                "--memory",
                "8g",
                "--kit",
                "docker.io/sbx/task:latest",
                "--kit",
                "./my-mixin/",
            ])
        );
    }

    #[test]
    fn unset_resources_leave_sbx_its_defaults() {
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let flags = create_flags(&plan);
        for flag in ["--cpus", "--memory", "--kit"] {
            assert!(!flags.contains(&flag.to_string()), "{flags:?}");
        }
    }

    fn provision_command() -> Vec<String> {
        args(&[
            "exec",
            "-u",
            "root",
            "wm-proj-12345678",
            "bash",
            "-c",
            "apt-get install -y ripgrep",
        ])
    }

    #[test]
    fn provision_runs_as_root_after_the_sandbox_is_set_up_and_its_rules_added() {
        let config = SandboxConfig {
            docker_sbx: DockerSbxConfig {
                provision: Some("apt-get install -y ripgrep".to_string()),
                ..config().docker_sbx
            },
            ..with_network(NetworkPolicy::Deny, domains(&["deb.debian.org"]))
        };
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        // Provisioning runs after every setup command: the seed, the parent
        // dirs and the domain rules its downloads may need.
        let setup = setup_args(&plan);
        assert!(
            setup
                .iter()
                .any(|a| a.iter().any(|w| w.contains(".claude.json"))),
            "{setup:?}"
        );
        assert!(
            setup.contains(&policy("allow", "deb.debian.org:443")),
            "{setup:?}"
        );
        assert!(!setup.contains(&provision_command()), "{setup:?}");
        assert_eq!(plan.provision.map(|c| c.args), Some(provision_command()));
    }

    #[test]
    fn provision_never_runs_on_an_existing_sandbox() {
        let config = with_docker_sbx(|sbx| {
            sbx.provision = Some("apt-get install -y ripgrep".to_string());
            sbx.kits = Some(vec!["docker.io/sbx/task:latest".to_string()]);
            sbx.cpus = Some(8);
        });
        let p = paths();
        let state = SbxState {
            sandbox: Some(sandbox("stopped")),
            ..missing()
        };
        let plan = launch_plan(&config, &target(&p), "claude", &state, &exec()).unwrap();

        assert_eq!(plan.provision, None);
        assert!(plan.create.is_none(), "{:?}", plan.create);
    }

    #[test]
    fn skip_default_provision_still_runs_provision_and_workmux_setup() {
        let config = with_docker_sbx(|sbx| {
            sbx.provision = Some("apt-get install -y ripgrep".to_string());
            sbx.skip_default_provision = Some(true);
        });
        let p = paths();
        let skipped = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let config = with_docker_sbx(|sbx| {
            sbx.provision = Some("apt-get install -y ripgrep".to_string());
        });
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            skipped.provision.as_ref().map(|c| c.args.clone()),
            Some(provision_command())
        );
        assert_eq!(setup_args(&skipped), setup_args(&plan));
    }

    #[test]
    fn blank_provision_script_runs_nothing() {
        let config = with_docker_sbx(|sbx| sbx.provision = Some("  \n".to_string()));
        let p = paths();
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(plan.provision, None);
    }

    /// The scripts of the plan's built-in provisioning, with the user each
    /// runs as.
    fn default_provision(plan: &LaunchPlan) -> Vec<(&str, &str)> {
        plan.default_provision
            .iter()
            .map(|c| match c.args.as_slice() {
                [exec, u, root, name, bash, dash_c, script]
                    if exec == "exec"
                        && u == "-u"
                        && name == "wm-proj-12345678"
                        && bash == "bash"
                        && dash_c == "-c" =>
                {
                    (root.as_str(), script.as_str())
                }
                [exec, name, bash, dash_c, script]
                    if exec == "exec"
                        && name == "wm-proj-12345678"
                        && bash == "bash"
                        && dash_c == "-c" =>
                {
                    ("agent", script.as_str())
                }
                other => panic!("unexpected provisioning command {other:?}"),
            })
            .collect()
    }

    #[test]
    fn created_sandbox_keeps_the_shims_on_path_for_login_shells() {
        // Codex runs commands with `bash -lc`, whose /etc/profile resets
        // PATH; the image's profile.d script names the container's shims.
        let p = paths();
        let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

        let steps = default_provision(&plan);
        assert_eq!(steps.len(), 1, "{steps:?}");
        let (user, script) = steps[0];
        assert_eq!(user, "root");
        assert!(
            script.contains(
                "cat > /etc/profile.d/workmux-shims.sh <<'PROFILESCRIPT'\n\
                 if [ -d /state/docker-sbx/wm-proj-12345678/shims/bin ]; then\n    \
                 PATH=/state/docker-sbx/wm-proj-12345678/shims/bin:\"$PATH\"\n    \
                 export PATH\n\
                 fi\n\
                 PROFILESCRIPT\n"
            ),
            "{script}"
        );
        assert!(!script.contains("install.determinate.systems"), "{script}");
    }

    #[test]
    fn created_sandbox_gets_nix_and_devbox_when_the_worktree_uses_a_toolchain() {
        for toolchain in [DetectedToolchain::Flake, DetectedToolchain::Devbox] {
            let p = with_toolchain(toolchain.clone());
            let plan = launch_plan(&config(), &target(&p), "claude", &missing(), &exec()).unwrap();

            let steps = default_provision(&plan);
            assert_eq!(steps.len(), 2, "{toolchain:?}: {steps:?}");
            let (user, root_script) = steps[0];
            assert_eq!(user, "root");
            for line in [
                "cat > /etc/profile.d/workmux-shims.sh",
                "curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | \\\n      \
                 sh -s -- install linux --init none --no-confirm",
                "chown -R agent:agent /nix",
                "curl -fsSL https://get.jetify.com/devbox | bash -s -- -f",
            ] {
                assert!(
                    root_script.contains(line),
                    "{toolchain:?}: {line}\n{root_script}"
                );
            }
            let (user, agent_script) = steps[1];
            assert_eq!(user, "agent");
            for line in [
                "echo '. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh' >> ~/.profile",
                "devbox version",
            ] {
                assert!(
                    agent_script.contains(line),
                    "{toolchain:?}: {line}\n{agent_script}"
                );
            }
        }
    }

    #[test]
    fn skip_default_provision_skips_the_built_in_provisioning_but_not_provision() {
        let config = with_docker_sbx(|sbx| {
            sbx.provision = Some("apt-get install -y ripgrep".to_string());
            sbx.skip_default_provision = Some(true);
        });
        let p = with_toolchain(DetectedToolchain::Flake);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert!(
            plan.default_provision.is_empty(),
            "{:?}",
            plan.default_provision
        );
        assert_eq!(
            plan.provision.as_ref().map(|c| c.args.clone()),
            Some(provision_command())
        );
        // The agent command still runs in the toolchain the image provides.
        assert!(
            exec_line(&plan).contains("nix develop"),
            "{}",
            exec_line(&plan)
        );
    }

    #[test]
    fn existing_sandbox_is_not_provisioned_again() {
        let p = with_toolchain(DetectedToolchain::Devbox);
        let state = SbxState {
            sandbox: Some(sandbox("running")),
            ..missing()
        };
        let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();

        assert!(
            plan.default_provision.is_empty(),
            "{:?}",
            plan.default_provision
        );
    }

    /// What installing and using Nix and Devbox reached in a sandbox, as
    /// `sbx policy log` showed them.
    const TOOLCHAIN_RESOURCES: [&str; 11] = [
        "install.determinate.systems:443",
        "cache.nixos.org:443",
        "get.jetify.com:443",
        "get.jetpack.io:443",
        "releases.jetify.com:443",
        "releases.jetpack.io:443",
        "www.nixsearch.com:443",
        "github.com:443",
        "api.github.com:443",
        "codeload.github.com:443",
        "release-assets.githubusercontent.com:443",
    ];

    #[test]
    fn under_deny_the_toolchain_gets_only_the_endpoints_allowed_domains_lists() {
        // A repository can add a devbox.json or flake.nix; under deny it must
        // not open more than the global allowed_domains do.
        let config = with_network(
            NetworkPolicy::Deny,
            domains(&[
                "api.anthropic.com",
                "cache.nixos.org",
                "*.jetify.com",
                "*.github.com",
            ]),
        );
        let p = with_toolchain(DetectedToolchain::Flake);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        assert_eq!(
            plan.network_rules,
            vec![
                recorded(RuleDecision::Allow, "api.anthropic.com:443"),
                recorded(RuleDecision::Allow, "cache.nixos.org:443"),
                recorded(RuleDecision::Allow, "**.jetify.com:443"),
                recorded(RuleDecision::Deny, "jetify.com:443"),
                recorded(RuleDecision::Allow, "**.github.com:443"),
                recorded(RuleDecision::Deny, "github.com:443"),
            ]
        );
        assert_eq!(plan.notes.len(), 1, "{:?}", plan.notes);
        let note = &plan.notes[0];
        for missing in [
            "install.determinate.systems",
            "get.jetpack.io",
            "releases.jetpack.io",
            "www.nixsearch.com",
            "github.com,",
            "release-assets.githubusercontent.com",
        ] {
            assert!(note.contains(missing), "{missing}: {note}");
        }
        for listed in ["cache.nixos.org", "get.jetify.com", "api.github.com"] {
            assert!(!note.contains(listed), "{listed}: {note}");
        }
        assert!(note.contains("allowed_domains"), "{note}");
    }

    #[test]
    fn under_deny_no_note_when_allowed_domains_cover_the_toolchain() {
        let hosts: Vec<&str> = TOOLCHAIN_RESOURCES
            .iter()
            .map(|r| r.trim_end_matches(":443"))
            .collect();
        let config = with_network(NetworkPolicy::Deny, domains(&hosts));
        let p = with_toolchain(DetectedToolchain::Devbox);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let resources: Vec<&str> = plan
            .network_rules
            .iter()
            .map(|r| r.resource.as_str())
            .collect();
        assert_eq!(resources, TOOLCHAIN_RESOURCES);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn toolchain_endpoints_are_allowed_under_allow_since_the_global_policy_may_block_them() {
        // sbx's balanced global policy refuses the Nix installer, the binary
        // cache and Devbox's downloads.
        let config = with_network(NetworkPolicy::Allow, domains(&["api.anthropic.com"]));
        let p = with_toolchain(DetectedToolchain::Devbox);
        let plan = launch_plan(&config, &target(&p), "claude", &missing(), &exec()).unwrap();

        let resources: Vec<&str> = plan
            .network_rules
            .iter()
            .map(|r| r.resource.as_str())
            .collect();
        assert_eq!(resources, TOOLCHAIN_RESOURCES);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn turning_the_toolchain_off_removes_its_endpoints() {
        let mut p = paths();
        p.record = vec![recorded(RuleDecision::Allow, "cache.nixos.org:443")];
        let state = running_with(vec![rule(SCOPE, "cache.nixos.org:443")]);
        let plan = launch_plan(&config(), &target(&p), "claude", &state, &exec()).unwrap();

        assert!(
            policy_commands(&plan).contains(&policy("rm", "cache.nixos.org:443")),
            "{:?}",
            policy_commands(&plan)
        );
        assert!(plan.network_rules.is_empty(), "{:?}", plan.network_rules);
    }
}
