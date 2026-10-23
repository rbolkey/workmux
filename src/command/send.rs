use std::io::{IsTerminal, Read};

use anyhow::{Result, anyhow};

use crate::config;
use crate::multiplexer::AgentPane;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow;

pub fn run(name: &str, text: Option<&str>, file: Option<&str>) -> Result<()> {
    let cfg = config::Config::load(None).unwrap_or_default();
    let mux = create_backend(detect_backend());
    let (_path, agent) = workflow::resolve_worktree_agent(name, mux.as_ref())?;
    ensure_reachable(name, &agent)?;

    // Determine content: positional arg > --file > stdin
    let content = if let Some(t) = text {
        t.to_string()
    } else if let Some(f) = file {
        std::fs::read_to_string(f)?
    } else {
        // Guard: don't block on interactive TTY
        if std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "No content to send. Provide text argument, --file, or pipe stdin"
            ));
        }
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    // Strip trailing newline
    let content = content.trim_end_matches('\n');

    if content.is_empty() {
        return Err(anyhow!("No content to send"));
    }

    let agent_identity = agent
        .agent_kind
        .as_deref()
        .or(cfg.agent.as_deref())
        .or(agent.agent_command.as_deref());
    mux.send_prompt_to_agent(&agent.pane_id, content, agent_identity)?;

    Ok(())
}

/// Refuse to type into the view of a remote agent whose remote is
/// disconnected: the text would reach the reconnecting view, not the agent.
fn ensure_reachable(name: &str, agent: &AgentPane) -> Result<()> {
    match &agent.remote {
        Some(remote) if remote.stale => Err(anyhow!(
            "{} is disconnected; cannot send to {name} until it reconnects",
            remote.remote
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sending_to_a_stale_remote_agent_fails() {
        let mut agent: AgentPane = serde_json::from_value(serde_json::json!({
            "session": "api",
            "window_name": "wm-feature",
            "pane_id": "%3",
            "path": "/remote/api__worktrees/feature",
        }))
        .unwrap();
        assert!(ensure_reachable("feature", &agent).is_ok());
        let mut remote = crate::multiplexer::RemoteAgentPane {
            remote: "devbox".to_string(),
            project: "api".to_string(),
            worktree: "feature".to_string(),
            stale: false,
        };
        agent.remote = Some(remote.clone());
        assert!(ensure_reachable("feature", &agent).is_ok());
        remote.stale = true;
        agent.remote = Some(remote);
        assert_eq!(
            ensure_reachable("feature", &agent).unwrap_err().to_string(),
            "devbox is disconnected; cannot send to feature until it reconnects"
        );
    }
}
