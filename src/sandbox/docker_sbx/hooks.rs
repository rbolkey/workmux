//! Merging workmux's status hooks into the config an sbx kit wrote.
//!
//! The kit's files are copied out of the sandbox, merged on the workstation
//! by the code `workmux setup` uses, and the changed ones written back as the
//! agent user.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::agent_setup::{self, Agent, opencode};

use super::instance::SbxOutput;
use super::plan::{SbxCommand, StatusHooks};

/// Prints each file of `$2...` under `$1` that exists, as `<size> <path>\n`
/// and its content.
const READ_SCRIPT: &str = r#"cd "$1" 2>/dev/null || exit 0
shift
for f; do
  if [ -f "$f" ]; then
    printf '%s %s\n' "$(wc -c < "$f")" "$f" && cat "$f" || exit 1
  fi
done"#;

/// Writes stdin to `$1`, creating its parent directories.
const WRITE_SCRIPT: &str = r#"mkdir -p "$(dirname "$1")" && cat > "$1""#;

const REMOVE_SCRIPT: &str = r#"rm -f -- "$@""#;

/// Merge workmux's status hooks into the config in sandbox `name`.
pub(super) fn install(
    name: &str,
    hooks: &StatusHooks,
    run: &mut impl FnMut(&SbxCommand) -> Result<SbxOutput>,
) -> Result<()> {
    let version = match hooks.agent {
        Agent::OpenCode => {
            let output = run(&exec(name, &["opencode", "--version"], None))?;
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => None,
    };
    let mut read = vec!["sh", "-c", READ_SCRIPT, "sh", &hooks.config_dir];
    read.extend(hooks.files);
    let before = parse_files(&run(&exec(name, &read, None))?.stdout)?;
    let after = merge(hooks.agent, version.as_deref(), &before)?;

    let mut removed = Vec::new();
    for path in before.keys().filter(|path| !after.contains_key(*path)) {
        removed.push(format!("{}/{path}", hooks.config_dir));
    }
    for (path, content) in &after {
        if before.get(path) != Some(content) {
            let path = format!("{}/{path}", hooks.config_dir);
            let args = ["sh", "-c", WRITE_SCRIPT, "sh", &path];
            run(&exec(name, &args, Some(content.clone())))?;
        }
    }
    if !removed.is_empty() {
        let mut args = vec!["sh", "-c", REMOVE_SCRIPT, "sh"];
        args.extend(removed.iter().map(String::as_str));
        run(&exec(name, &args, None))?;
    }
    Ok(())
}

/// Run `command` as the agent user in sandbox `name`.
fn exec(name: &str, command: &[&str], stdin: Option<Vec<u8>>) -> SbxCommand {
    let mut args = vec!["exec".to_string()];
    if stdin.is_some() {
        args.push("-i".to_string());
    }
    args.push(name.to_string());
    args.extend(command.iter().map(|word| word.to_string()));
    SbxCommand {
        args,
        env: Vec::new(),
        stdin,
    }
}

/// Parse [`READ_SCRIPT`]'s output into each file's content by path.
fn parse_files(mut output: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    while !output.is_empty() {
        let end = output
            .iter()
            .position(|&b| b == b'\n')
            .context("Unexpected end of the agent config read from the sandbox")?;
        let header = String::from_utf8_lossy(&output[..end]);
        let (size, path) = header
            .trim_start()
            .split_once(' ')
            .and_then(|(size, path)| Some((size.parse::<usize>().ok()?, path.to_string())))
            .with_context(|| format!("Unexpected agent config header {header:?}"))?;
        output = &output[end + 1..];
        anyhow::ensure!(
            output.len() >= size,
            "Agent config file {path} read from the sandbox is truncated"
        );
        files.insert(path, output[..size].to_vec());
        output = &output[size..];
    }
    Ok(files)
}

/// Install `agent`'s status hooks into a copy of `files` and return the
/// files after. `version` is what OpenCode's `--version` printed.
fn merge(
    agent: Agent,
    version: Option<&str>,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let dir = tempfile::tempdir()?;
    for (path, content) in files {
        let path = dir.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
    }
    match version {
        Some(version) => opencode::install_dir_for(dir.path(), version),
        None => agent_setup::install_in(agent, dir.path()),
    }?;
    let mut after = BTreeMap::new();
    collect_files(dir.path(), dir.path(), &mut after)?;
    Ok(after)
}

fn collect_files(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else {
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            files.insert(relative, std::fs::read(&path)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const GUEST_HOME: &str = "/home/agent";

    /// A fake sbx whose sandbox home is `home`: `sh -c` execs run there,
    /// `opencode --version` prints `opencode`, and every command is logged.
    struct FakeSandbox {
        home: tempfile::TempDir,
        opencode: &'static str,
        fail_writes: bool,
        ran: Vec<SbxCommand>,
    }

    impl FakeSandbox {
        fn new() -> Self {
            Self {
                home: tempfile::tempdir().unwrap(),
                opencode: "",
                fail_writes: false,
                ran: Vec::new(),
            }
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.home.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn read(&self, path: &str) -> Option<String> {
            std::fs::read_to_string(self.home.path().join(path)).ok()
        }

        fn run(&mut self, command: &SbxCommand) -> Result<SbxOutput> {
            self.ran.push(command.clone());
            let start = command.args.iter().position(|a| a == "sh");
            if command
                .args
                .ends_with(&["opencode".into(), "--version".into()])
            {
                return Ok(SbxOutput {
                    stdout: self.opencode.as_bytes().to_vec(),
                    ..Default::default()
                });
            }
            if command.stdin.is_some() && self.fail_writes {
                anyhow::bail!("sbx exec failed: read-only file system");
            }
            let home = self.home.path().to_string_lossy();
            let args: Vec<String> = command.args[start.unwrap() + 1..]
                .iter()
                .map(|a| a.replace(GUEST_HOME, &home))
                .collect();
            let mut child = Command::new("sh")
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(command.stdin.as_deref().unwrap_or_default())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            anyhow::ensure!(output.status.success(), "sbx exec failed");
            Ok(SbxOutput {
                stdout: output.stdout,
                ..Default::default()
            })
        }

        fn install(
            &mut self,
            agent: Agent,
            subpath: &str,
            files: &'static [&'static str],
        ) -> Result<()> {
            let hooks = StatusHooks {
                agent,
                config_dir: format!("{GUEST_HOME}/{subpath}"),
                files,
            };
            install("wm-test", &hooks, &mut |command| self.run(command))
        }

        /// The guest paths written, each by the agent user.
        fn writes(&self) -> Vec<String> {
            self.ran
                .iter()
                .filter(|command| command.stdin.is_some())
                .map(|command| {
                    assert_eq!(
                        command.args[..3],
                        ["exec", "-i", "wm-test"],
                        "as the agent user"
                    );
                    command.args.last().unwrap().clone()
                })
                .collect()
        }
    }

    #[test]
    fn claudes_settings_are_merged_and_written_back() {
        let mut sandbox = FakeSandbox::new();
        sandbox.write(".claude/settings.json", r#"{"model": "opus"}"#);

        sandbox
            .install(Agent::Claude, ".claude", &["settings.json"])
            .unwrap();

        assert_eq!(sandbox.writes(), ["/home/agent/.claude/settings.json"]);
        let settings: serde_json::Value =
            serde_json::from_str(&sandbox.read(".claude/settings.json").unwrap()).unwrap();
        assert_eq!(settings["model"], "opus");
        assert!(
            settings["hooks"]["Stop"]
                .to_string()
                .contains("workmux set-window-status done"),
            "{settings}"
        );
    }

    #[test]
    fn a_file_the_kit_didnt_write_is_created_with_its_directory() {
        let mut sandbox = FakeSandbox::new();
        sandbox.write(".codex/config.toml", "model_provider = \"sandboxd\"\n");

        sandbox
            .install(Agent::Codex, ".codex", &["hooks.json", "config.toml"])
            .unwrap();

        let hooks = sandbox.read(".codex/hooks.json").unwrap();
        assert!(hooks.contains("workmux set-window-status"), "{hooks}");
        let config = sandbox.read(".codex/config.toml").unwrap();
        assert!(config.contains("model_provider = \"sandboxd\""), "{config}");
        assert!(config.contains("hooks = true"), "{config}");

        let mut sandbox = FakeSandbox::new();
        sandbox
            .install(Agent::Copilot, ".copilot", &["hooks/workmux-status.json"])
            .unwrap();
        assert!(
            sandbox
                .read(".copilot/hooks/workmux-status.json")
                .unwrap()
                .contains("workmux set-window-status")
        );
    }

    #[test]
    fn files_the_merge_leaves_unchanged_arent_written() {
        let mut sandbox = FakeSandbox::new();
        sandbox.write(".gemini/settings.json", "{}");
        sandbox
            .install(Agent::Gemini, ".gemini", &["settings.json"])
            .unwrap();
        sandbox.ran.clear();

        sandbox
            .install(Agent::Gemini, ".gemini", &["settings.json"])
            .unwrap();

        assert_eq!(sandbox.writes(), Vec::<String>::new());
    }

    #[test]
    fn opencodes_plugin_layout_follows_the_sandboxs_opencode() {
        let files: &[&str] = &[
            ".workmux-status.json",
            "plugins/workmux-status.ts",
            "plugin/workmux-status.ts",
            "plugins/workmux-status/index.ts",
            "plugins/workmux-status/tui.ts",
            "plugins/workmux-status/status.ts",
        ];
        let mut sandbox = FakeSandbox::new();
        sandbox.opencode = "1.14.2\n";
        sandbox
            .install(Agent::OpenCode, ".config/opencode", files)
            .unwrap();
        assert!(
            sandbox
                .read(".config/opencode/plugins/workmux-status.ts")
                .is_some()
        );

        sandbox.opencode = "2.0.18\n";
        sandbox
            .install(Agent::OpenCode, ".config/opencode", files)
            .unwrap();
        assert!(
            sandbox
                .read(".config/opencode/plugins/workmux-status/index.ts")
                .is_some()
        );
        assert_eq!(
            sandbox.read(".config/opencode/plugins/workmux-status.ts"),
            None
        );
    }

    #[test]
    fn a_failed_write_fails_the_install() {
        let mut sandbox = FakeSandbox::new();
        sandbox.fail_writes = true;

        let error = sandbox
            .install(Agent::Claude, ".claude", &["settings.json"])
            .unwrap_err();

        assert!(format!("{error:#}").contains("read-only"), "{error:#}");
    }
}
