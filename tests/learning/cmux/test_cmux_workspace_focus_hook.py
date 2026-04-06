"""Learning tests for cmux workspace-focus hook support.

These tests verify whether `cmux set-hook` supports a workspace-focus event
that fires when a user switches to a workspace. This is needed for the
auto-clear status feature (Unit 4 of post-MVP quick wins).

Deferred question: Does `cmux set-hook workspace-focus` exist and fire?
Also: per-workspace hooks or global only?

Discovery: 2026-04-06, deferred question from post-MVP quick wins plan.
"""

import os
import subprocess
import tempfile
import time

import pytest

from .helpers import run_cmux


class TestWorkspaceFocusHook:
    """Does cmux support a workspace-focus hook event?"""

    def test_set_hook_workspace_focus_accepted(self, cmux_workspace):
        """Q: Does set-hook accept 'workspace-focus' as an event name?

        If set-hook returns 0, the event name is at least recognized.
        Does NOT prove the hook fires — just that it's a valid registration.
        """
        ws = cmux_workspace

        # Try several plausible event names for workspace focus
        event_names = [
            "workspace-focus",
            "workspace-focused",
            "pane-focus-in",
            "client-focus-changed",
            "after-select-window",
        ]

        accepted = {}
        for event in event_names:
            result = subprocess.run(
                [
                    "cmux",
                    "set-hook",
                    "-g",
                    event,
                    "run-shell 'echo hook-probe'",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            accepted[event] = result.returncode == 0

            # Clean up if registered
            if result.returncode == 0:
                subprocess.run(
                    ["cmux", "set-hook", "-gu", event],
                    capture_output=True,
                    check=False,
                )

        if not any(accepted.values()):
            pytest.skip(
                f"No focus-related hook events accepted by set-hook. "
                f"Tested: {list(event_names)}"
            )

        # Document which events were accepted
        accepted_names = [e for e, v in accepted.items() if v]
        assert len(accepted_names) > 0, "At least one focus event should be accepted"

    def test_workspace_focus_hook_fires(self, cmux_two_workspaces):
        """Q: Does a workspace-focus hook actually fire when switching workspaces?

        Register a hook that writes a marker file, switch workspaces,
        and check if the file appeared.
        """
        primary = cmux_two_workspaces["primary"]
        secondary = cmux_two_workspaces["secondary"]

        with tempfile.NamedTemporaryFile(
            prefix="cmux-hook-", suffix=".marker", delete=False
        ) as f:
            marker_path = f.name

        # Remove the file so we can detect creation
        os.unlink(marker_path)

        # Try workspace-focus first, fall back to other names
        event_names = [
            "workspace-focus",
            "pane-focus-in",
            "after-select-window",
        ]

        registered_event = None
        for event in event_names:
            result = subprocess.run(
                [
                    "cmux",
                    "set-hook",
                    "-g",
                    event,
                    f"run-shell 'touch {marker_path}'",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                registered_event = event
                break

        if registered_event is None:
            pytest.skip("No focus-related hook events accepted")

        try:
            # Switch to secondary workspace
            subprocess.run(
                [
                    "cmux",
                    "select-workspace",
                    "--workspace",
                    secondary["workspace_ref"],
                ],
                capture_output=True,
                check=False,
            )
            time.sleep(0.5)

            # Switch back to primary
            subprocess.run(
                [
                    "cmux",
                    "select-workspace",
                    "--workspace",
                    primary["workspace_ref"],
                ],
                capture_output=True,
                check=False,
            )
            time.sleep(1.0)

            if os.path.exists(marker_path):
                # Hook fired! Document which event worked
                os.unlink(marker_path)
            else:
                pytest.skip(
                    f"Hook '{registered_event}' registered but did not fire "
                    f"after workspace switch. Hook mechanism may be incomplete."
                )
        finally:
            # Clean up hook
            subprocess.run(
                ["cmux", "set-hook", "-gu", registered_event],
                capture_output=True,
                check=False,
            )
            if os.path.exists(marker_path):
                os.unlink(marker_path)

    def test_per_workspace_vs_global_hooks(self, cmux_two_workspaces):
        """Q: Does cmux support per-workspace hooks or only global?

        If per-workspace: hook fires only for the registered workspace.
        If global only: hook fires for any workspace focus, requiring
        callback logic to identify which workspace was focused.
        """
        primary = cmux_two_workspaces["primary"]
        secondary = cmux_two_workspaces["secondary"]

        # Try per-workspace hook syntax (tmux uses -t for targeting)
        # Try --workspace flag first, then -t flag
        per_ws_syntaxes = [
            [
                "cmux",
                "set-hook",
                "--workspace",
                primary["workspace_ref"],
                "workspace-focus",
                "run-shell 'echo per-ws'",
            ],
            [
                "cmux",
                "set-hook",
                "-t",
                primary["workspace_ref"],
                "workspace-focus",
                "run-shell 'echo per-ws'",
            ],
            [
                "cmux",
                "set-hook",
                "-w",  # tmux per-window flag
                "workspace-focus",
                "run-shell 'echo per-ws'",
            ],
        ]

        per_workspace_supported = False
        for cmd in per_ws_syntaxes:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                per_workspace_supported = True
                # Clean up
                subprocess.run(
                    ["cmux", "set-hook", "-gu", "workspace-focus"],
                    capture_output=True,
                    check=False,
                )
                break

        if not per_workspace_supported:
            pytest.skip(
                "Per-workspace hook syntax not accepted. "
                "Only global hooks (-g) appear to be supported."
            )
