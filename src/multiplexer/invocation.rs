//! The invocation context: where a command was run from.
//!
//! Placement and "current pane/window/session" resolution consult this value
//! instead of reading `$TMUX_PANE`, `$TMUX` or the process working directory,
//! so a caller that is not running in the invoking terminal (the workstation
//! daemon answering a remote) can supply the context it was sent.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The pane, session, multiplexer instance and working directory a command
/// was run from.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InvocationContext {
    /// The invoking pane (`$TMUX_PANE`).
    pub pane_id: Option<String>,
    /// The invoking tmux client (`$TMUX`): `socket_path,server_pid,session_index`.
    pub tmux: Option<String>,
    /// The working directory the command was run from.
    pub cwd: Option<PathBuf>,
}

static CURRENT: OnceLock<InvocationContext> = OnceLock::new();

impl InvocationContext {
    /// Build the invocation context from this process's environment.
    pub fn from_env() -> Self {
        Self {
            pane_id: std::env::var("TMUX_PANE").ok(),
            tmux: std::env::var("TMUX").ok(),
            cwd: std::env::current_dir().ok(),
        }
    }

    /// The invocation context of this CLI invocation, captured from the
    /// environment on first use. The CLI entry calls this before running any
    /// command, so later changes to the process cwd do not affect it.
    pub fn current() -> &'static Self {
        CURRENT.get_or_init(Self::from_env)
    }

    /// Socket path of the invoking tmux server, from `$TMUX`.
    pub fn tmux_socket_path(&self) -> Option<&str> {
        self.tmux
            .as_deref()?
            .split(',')
            .next()
            .filter(|socket_path| !socket_path.is_empty())
    }

    /// Server PID and session ID (`$N`) of the invoking tmux client, from `$TMUX`.
    pub fn tmux_client_session(&self) -> Option<(u32, String)> {
        let mut fields = self.tmux.as_deref()?.rsplitn(3, ',');
        let session = fields.next()?.parse::<u64>().ok()?;
        let pid = fields.next()?.parse::<u32>().ok()?;
        let socket = fields.next()?;
        if socket.is_empty() {
            return None;
        }
        Some((pid, format!("${session}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_client_fields() {
        let context = InvocationContext {
            tmux: Some("/tmp/tmux-501/default,4242,3".to_string()),
            ..InvocationContext::default()
        };
        assert_eq!(context.tmux_socket_path(), Some("/tmp/tmux-501/default"));
        assert_eq!(
            context.tmux_client_session(),
            Some((4242, "$3".to_string()))
        );
    }

    #[test]
    fn rejects_tmux_client_without_socket() {
        let context = InvocationContext {
            tmux: Some(",4242,3".to_string()),
            ..InvocationContext::default()
        };
        assert_eq!(context.tmux_socket_path(), None);
        assert_eq!(context.tmux_client_session(), None);
    }
}
