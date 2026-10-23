//! Shims on remotes: the guest shims of the sandbox backends, for remote
//! panes. A shim runs a command on the workstation through the shell
//! socket: sound playback (`afplay`) and clipboard reads (`wl-paste`,
//! `xclip`) are built in and on unless the remote turns them off; any other
//! host command must be on the remote's allowlist. The workstation decides
//! and installs the shims when it opens the channel, and checks every
//! request against the remote's settings again.

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

use crate::config::RemoteConfig;
use crate::sandbox::shims::{
    BUILTIN_HOST_COMMANDS, create_shim_directory, is_clipboard_shim, validate_command_name,
};

/// Request from the workstation installing a remote's shims:
/// `{commands}`, replacing any installed before.
pub const INSTALL_METHOD: &str = "shims.install";

#[derive(Debug, Serialize, Deserialize)]
pub struct Install {
    pub commands: Vec<String>,
}

/// Request from a remote shell for the workstation clipboard's image:
/// `{mime}`, answered with `{png}`, base64 or `null` when there is none.
pub const CLIPBOARD_METHOD: &str = "clipboard.read";

/// Request from a remote shell to run a host command on the workstation:
/// `{command, args}`, answered with its exit code and output once it exits.
pub const EXEC_METHOD: &str = "host.exec";

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipboardRead {
    pub mime: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Clipboard {
    pub png: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostExec {
    pub command: String,
    pub args: Vec<String>,
}

/// A host command's exit code and output (base64).
#[derive(Debug, Serialize, Deserialize)]
pub struct Exited {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Exited {
    pub fn stdout(&self) -> Result<Vec<u8>> {
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.stdout)?)
    }

    pub fn stderr(&self) -> Result<Vec<u8>> {
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.stderr)?)
    }
}

/// The commands `remote` lets its remote panes run on the workstation.
pub fn allowed(remote: &RemoteConfig) -> Vec<String> {
    let builtin = BUILTIN_HOST_COMMANDS
        .iter()
        .map(|command| command.to_string())
        .filter(|_| remote.builtin_shims());
    let mut commands: Vec<String> = Vec::new();
    for command in builtin.chain(remote.host_commands.iter().cloned()) {
        if validate_command_name(&command) && !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands
}

/// Remote workmux's side of `shims.install`: replace this remote's shims.
pub fn serve_install(params: Value) -> Result<Value> {
    let Install { commands } = serde_json::from_value(params)?;
    let state = crate::xdg::state_dir()?;
    let dir = super::shell::shims_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to clear {}", dir.display()))?;
    }
    create_shim_directory(&state, &commands)?;
    Ok(Value::Null)
}

/// Read the workstation clipboard's `mime` image from a remote shell
/// through the shell socket at `socket`; `None` when there is none.
pub fn clipboard_read(socket: &Path, mime: &str) -> Result<Option<Vec<u8>>> {
    let clipboard: Clipboard = serde_json::from_value(super::forwarding::request_once(
        socket,
        CLIPBOARD_METHOD,
        serde_json::to_value(ClipboardRead {
            mime: mime.to_string(),
        })?,
    )?)?;
    clipboard
        .png
        .map(|png| Ok(base64::engine::general_purpose::STANDARD.decode(png)?))
        .transpose()
}

/// Run host command `command` on the workstation from a remote shell,
/// through the shell socket at `socket`.
pub fn host_exec(socket: &Path, command: &str, args: &[String]) -> Result<Exited> {
    Ok(serde_json::from_value(super::forwarding::request_once(
        socket,
        EXEC_METHOD,
        serde_json::to_value(HostExec {
            command: command.to_string(),
            args: args.to_vec(),
        })?,
    )?)?)
}

/// The workstation's answer to `clipboard.read` from remote `name`, reading
/// the clipboard with `read`.
pub fn answer_clipboard(
    name: &str,
    remote: &RemoteConfig,
    read: fn() -> Result<Option<Vec<u8>>>,
    params: Value,
) -> Result<Value> {
    let ClipboardRead { mime } = serde_json::from_value(params)?;
    if !remote.builtin_shims() {
        bail!("refused: the built-in shims are off for remote {name}");
    }
    if mime != "image/png" {
        bail!("unsupported clipboard MIME type: {mime}");
    }
    let png = read()?.map(|png| base64::engine::general_purpose::STANDARD.encode(png));
    Ok(serde_json::to_value(Clipboard { png })?)
}

/// The workstation's answer to `host.exec` from remote `name`: run the
/// command, if the remote allows it, in the workstation's host-exec
/// sandbox.
pub fn answer_exec(name: &str, remote: &RemoteConfig, params: Value) -> Result<Value> {
    let HostExec { command, args } = serde_json::from_value(params)?;
    if is_clipboard_shim(&command) || !allowed(remote).contains(&command) {
        bail!("refused: host command {command} is not allowed for remote {name}");
    }
    tracing::info!(
        remote = name,
        command,
        ?args,
        "host-exec request from a remote"
    );
    let workdir = std::env::temp_dir().join("workmux-host-exec").join(name);
    std::fs::create_dir_all(&workdir)
        .with_context(|| format!("failed to create {}", workdir.display()))?;
    let child = crate::sandbox::host_exec_sandbox::spawn_sandboxed(
        &command,
        &args,
        &workdir,
        &crate::sandbox::rpc::sanitized_env(),
        false,
    )?;
    let output = child.wait_with_output()?;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(serde_json::to_value(Exited {
        exit_code: output.status.code().unwrap_or(1),
        stdout: encode(&output.stdout),
        stderr: encode(&output.stderr),
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, TransportKind};
    use crate::remote::channels::{ChannelPolicy, Channels};
    use crate::remote::testing::ServeTransport;
    use crate::remote::transport::Transport;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn remote(builtin_shims: Option<bool>, host_commands: &[&str]) -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Loopback,
            target: "/remote".to_string(),
            builtin_shims,
            host_commands: host_commands.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn channels(transport: Arc<ServeTransport>) -> Channels {
        Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            ChannelPolicy {
                registered: |_, project| project == "api",
                clipboard: || Ok(Some(b"\x89PNG image".to_vec())),
                ..ChannelPolicy::default()
            },
        )
    }

    /// A shell on a remote with these settings, its channel open.
    fn shell_on(remote: RemoteConfig) -> (Arc<ServeTransport>, Channels) {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels(transport.clone());
        channels.channel("devbox", &remote).unwrap();
        (transport, channels)
    }

    #[test]
    fn a_remote_shell_reads_the_workstation_clipboard_image() {
        let (transport, _channels) = shell_on(remote(None, &[]));

        let image = clipboard_read(&transport.shell_socket(), "image/png").unwrap();

        assert_eq!(image, Some(b"\x89PNG image".to_vec()));
    }

    #[test]
    fn with_the_built_in_shims_off_a_remote_shell_can_neither_read_the_clipboard_nor_play_sounds() {
        let (transport, _channels) = shell_on(remote(Some(false), &[]));

        let clipboard = clipboard_read(&transport.shell_socket(), "image/png")
            .unwrap_err()
            .to_string();
        let sound = host_exec(
            &transport.shell_socket(),
            "afplay",
            &["ping.aiff".to_string()],
        )
        .unwrap_err()
        .to_string();

        assert!(
            clipboard.contains("refused: the built-in shims are off for remote devbox"),
            "{clipboard}"
        );
        assert!(
            sound.contains("refused: host command afplay is not allowed for remote devbox"),
            "{sound}"
        );
    }

    #[test]
    fn a_remote_shell_runs_only_allowlisted_host_commands_on_the_workstation() {
        let (transport, _channels) = shell_on(remote(None, &["echo"]));

        let ran = host_exec(&transport.shell_socket(), "echo", &["hello".to_string()]).unwrap();
        let refused = host_exec(&transport.shell_socket(), "ls", &[])
            .unwrap_err()
            .to_string();
        // The clipboard shims read through the clipboard, never as commands.
        let clipboard_write = host_exec(&transport.shell_socket(), "xclip", &["-i".to_string()])
            .unwrap_err()
            .to_string();

        assert_eq!(
            (ran.exit_code, ran.stdout().unwrap()),
            (0, b"hello\n".to_vec())
        );
        assert!(
            refused.contains("refused: host command ls is not allowed for remote devbox"),
            "{refused}"
        );
        assert!(clipboard_write.contains("refused"), "{clipboard_write}");
    }

    fn installed(transport: &ServeTransport, expected: usize) -> BTreeSet<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let names: BTreeSet<String> = std::fs::read_dir(transport.shims())
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .filter(|name| !name.starts_with('_') && !name.starts_with('.'))
                        .collect()
                })
                .unwrap_or_default();
            if names.len() == expected || Instant::now() > deadline {
                return names;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_remote_gets_the_built_in_shims_and_its_allowlisted_host_commands() {
        let transport = Arc::new(ServeTransport::default());
        channels(transport.clone())
            .channel("devbox", &remote(None, &["just"]))
            .unwrap();

        assert_eq!(
            installed(&transport, 4),
            BTreeSet::from(["afplay", "just", "wl-paste", "xclip"].map(String::from))
        );
    }

    #[test]
    fn a_remote_with_the_built_in_shims_off_gets_only_its_allowlist() {
        let transport = Arc::new(ServeTransport::default());
        channels(transport.clone())
            .channel("devbox", &remote(Some(false), &["just"]))
            .unwrap();

        assert_eq!(
            installed(&transport, 1),
            BTreeSet::from(["just".to_string()])
        );
    }
}
