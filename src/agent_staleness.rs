//! Shared stale-agent classification used by dashboard and sidebar.

use crate::multiplexer::AgentStatus;

/// Whether an agent is stale under the shared timeout and activity rules.
pub(crate) fn is_stale(
    activity_ts: Option<u64>,
    status: Option<AgentStatus>,
    now_secs: u64,
    stale_after_secs: u64,
    is_sleeping: bool,
    is_interrupted: bool,
) -> bool {
    if is_sleeping {
        return true;
    }

    // Done remains actionable until focus acknowledges and clears the status.
    if !is_interrupted
        && matches!(
            status,
            Some(AgentStatus::Working | AgentStatus::Waiting | AgentStatus::Done)
        )
    {
        return false;
    }

    activity_ts
        .map(|ts| now_secs.saturating_sub(ts) > stale_after_secs)
        .unwrap_or(true)
}

/// Whether `agent` counts as stale: a remote agent whose remote is
/// disconnected (it keeps its last status), or one not visibly progressing,
/// per [`is_stale`]. The sidebar and dashboard share this.
pub fn agent_is_stale(
    agent: &crate::multiplexer::AgentPane,
    now_secs: u64,
    stale_threshold_secs: u64,
    is_sleeping: bool,
    is_interrupted: bool,
) -> bool {
    agent.is_remote_stale()
        || is_stale(
            agent.activity_ts(),
            agent.status,
            now_secs,
            stale_threshold_secs,
            is_sleeping,
            is_interrupted,
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_threshold_strictly_after_the_boundary() {
        assert!(!is_stale(Some(100), None, 200, 100, false, false));
        assert!(is_stale(Some(100), None, 201, 100, false, false));
    }

    #[test]
    fn missing_activity_is_stale_unless_status_is_active() {
        assert!(is_stale(None, None, 100, 60, false, false));
        assert!(!is_stale(
            None,
            Some(AgentStatus::Working),
            100,
            60,
            false,
            false,
        ));
        assert!(!is_stale(
            None,
            Some(AgentStatus::Waiting),
            100,
            60,
            false,
            false,
        ));
    }

    #[test]
    fn unacknowledged_completion_does_not_age_out() {
        for activity_ts in [Some(100), None] {
            assert!(!is_stale(
                activity_ts,
                Some(AgentStatus::Done),
                10_000,
                60,
                false,
                false,
            ));
            assert!(is_stale(activity_ts, None, 10_000, 60, false, false));
        }
    }

    #[test]
    fn sleeping_and_interrupted_statuses_override_active_status() {
        assert!(is_stale(
            Some(100),
            Some(AgentStatus::Working),
            100,
            60,
            true,
            false,
        ));
        assert!(is_stale(
            Some(100),
            Some(AgentStatus::Working),
            201,
            100,
            false,
            true,
        ));
        assert!(!is_stale(
            Some(150),
            Some(AgentStatus::Working),
            201,
            100,
            false,
            true,
        ));
    }

    #[test]
    fn a_disconnected_remotes_agent_is_stale_with_fresh_activity() {
        let mut agent: crate::multiplexer::AgentPane = serde_json::from_value(serde_json::json!({
            "session": "api", "window_name": "wm-feature", "pane_id": "%1",
            "path": "/remote/api__worktrees/feature", "pane_title": null,
            "status": "working", "status_ts": 100, "activity_ts": 100,
            "remote": {"remote": "lb", "project": "api", "worktree": "feature", "stale": false},
        }))
        .unwrap();
        assert!(!agent_is_stale(&agent, 110, 3600, false, false));
        agent.remote.as_mut().unwrap().stale = true;
        assert!(agent_is_stale(&agent, 110, 3600, false, false));
    }
}
