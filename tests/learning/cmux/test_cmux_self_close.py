"""Learning tests for cmux workspace self-close behavior.

These tests verify that a process inside a workspace can close its own
workspace via `cmux close-workspace`. cmux handles this asynchronously —
the server processes the close after the calling process exits. This is
why workmux's `schedule_window_close` calls `kill_window` directly instead
of using the nohup deferred-script pattern.

Discovery: 2026-04-03, during investigation of duplicate window close bug.
The nohup approach fails with "Failed to configure socket receive timeout"
when the background process tries to connect during workspace teardown.
"""

import subprocess
import time

from .helpers import run_cmux, run_cmux_json


class TestSelfClose:
    """A workspace can close itself via cmux close-workspace."""

    def test_self_close_via_send(self):
        """Sending close-workspace to a workspace's own shell closes it."""
        # Create a workspace manually (not via fixture — we're closing it ourselves)
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", "wm-selfclose-test"],
            capture_output=True,
            text=True,
            check=True,
        )
        ws_ref = result.stdout.strip().split()[-1]

        surfaces = run_cmux_json("list-pane-surfaces", "--workspace", ws_ref)
        surf_ref = surfaces["surfaces"][0]["ref"]

        # Wait for shell readiness
        time.sleep(1.5)

        # Send the close command from inside the workspace
        run_cmux(
            "send",
            "--workspace",
            ws_ref,
            "--surface",
            surf_ref,
            f"cmux close-workspace --workspace {ws_ref}\n",
        )

        # cmux handles close asynchronously — wait for it to process
        deadline = time.time() + 8.0
        closed = False
        while time.time() < deadline:
            data = run_cmux_json("list-workspaces")
            refs = {ws["ref"] for ws in data["workspaces"]}
            if ws_ref not in refs:
                closed = True
                break
            time.sleep(0.5)

        assert closed, f"Workspace {ws_ref} should be closed after self-close command"

    def test_close_workspace_from_outside(self):
        """Closing a workspace from outside (normal case) works immediately."""
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", "wm-extclose-test"],
            capture_output=True,
            text=True,
            check=True,
        )
        ws_ref = result.stdout.strip().split()[-1]

        # Close from outside (our process, not the workspace's shell)
        run_cmux("close-workspace", "--workspace", ws_ref)

        # Should be gone quickly
        time.sleep(0.5)
        data = run_cmux_json("list-workspaces")
        refs = {ws["ref"] for ws in data["workspaces"]}
        assert ws_ref not in refs, "Workspace should be closed"


class TestCloseWorkspaceSelected:
    """close-workspace when the target is the currently selected workspace.

    In cmux, "selected" is the canonical term — select-workspace sets which
    workspace receives input and is shown in the UI. This is global UI state,
    not per-process state (unlike identify.caller which is per-process).

    This complements:
    - test_close_workspace_from_outside (closes non-selected workspace)
    - test_self_close_via_send (sends close into workspace's own shell)

    Motivated by the duplicate window close bug where current_window_name()
    returned None because it used the global "selected" state instead of
    per-process identify.caller.
    """

    def test_close_selected_workspace_succeeds(self):
        """close-workspace on the currently selected workspace succeeds."""
        # Create workspace manually (we'll close it ourselves)
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", "wm-close-selected-test"],
            capture_output=True,
            text=True,
            check=True,
        )
        ws_ref = result.stdout.strip().split()[-1]

        # Select it to make it the globally focused/selected workspace
        subprocess.run(
            ["cmux", "select-workspace", "--workspace", ws_ref],
            capture_output=True,
            check=True,
        )

        # Close the selected workspace from our test process
        run_cmux("close-workspace", "--workspace", ws_ref)

        time.sleep(0.5)
        data = run_cmux_json("list-workspaces")
        refs = {ws["ref"] for ws in data["workspaces"]}
        assert ws_ref not in refs, "Selected workspace should be closed"

    def test_close_selected_workspace_preserves_caller(self):
        """After closing the selected workspace, identify returns the test process's workspace."""
        import json

        # Get our own workspace ref first
        identify_before = run_cmux("identify")
        our_data = json.loads(identify_before.stdout)
        our_ws = our_data["caller"]["workspace_ref"]

        # Create and select a different workspace
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", "wm-close-preserve-test"],
            capture_output=True,
            text=True,
            check=True,
        )
        other_ws = result.stdout.strip().split()[-1]

        subprocess.run(
            ["cmux", "select-workspace", "--workspace", other_ws],
            capture_output=True,
            check=True,
        )

        # Close the selected workspace
        run_cmux("close-workspace", "--workspace", other_ws)
        time.sleep(0.5)

        # Verify our process's workspace is still intact
        identify_after = run_cmux("identify")
        after_data = json.loads(identify_after.stdout)
        assert after_data["caller"]["workspace_ref"] == our_ws, (
            f"Test process workspace changed from {our_ws} to "
            f"{after_data['caller']['workspace_ref']} after closing selected workspace"
        )

    def test_close_selected_workspace_auto_selects_adjacent(self):
        """Document: does closing the selected workspace auto-select an adjacent one?"""
        # Create two workspaces
        result1 = subprocess.run(
            ["cmux", "new-workspace", "--name", "wm-autosel-1"],
            capture_output=True,
            text=True,
            check=True,
        )
        ws1 = result1.stdout.strip().split()[-1]

        result2 = subprocess.run(
            ["cmux", "new-workspace", "--name", "wm-autosel-2"],
            capture_output=True,
            text=True,
            check=True,
        )
        ws2 = result2.stdout.strip().split()[-1]

        try:
            # Select ws2
            subprocess.run(
                ["cmux", "select-workspace", "--workspace", ws2],
                capture_output=True,
                check=True,
            )

            # Close ws2 (the selected one)
            run_cmux("close-workspace", "--workspace", ws2)
            time.sleep(0.5)

            # Check which workspace is now selected
            data = run_cmux_json("list-workspaces")
            selected = [ws for ws in data["workspaces"] if ws.get("selected")]
            assert len(selected) == 1, (
                f"Expected exactly one selected workspace, got {len(selected)}"
            )
            # Document: cmux should auto-select some workspace after closing
            # the selected one (the specific choice is implementation-dependent)
        finally:
            # Clean up ws1 if it still exists
            subprocess.run(
                ["cmux", "close-workspace", "--workspace", ws1],
                capture_output=True,
                check=False,
            )


class TestCmuxHomeIndependence:
    """cmux uses CMUX_SOCKET_PATH, not HOME, for socket discovery."""

    def test_cmux_works_with_different_home(self):
        """cmux commands work even when HOME points elsewhere."""
        import os

        env = os.environ.copy()
        env["HOME"] = "/tmp"

        result = subprocess.run(
            ["cmux", "ping"],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        assert result.returncode == 0, (
            f"cmux ping should work with HOME=/tmp, got: {result.stderr}"
        )

    def test_find_window_works_with_different_home(self, cmux_workspace):
        """find-window works with a non-default HOME."""
        import os

        ws = cmux_workspace
        run_cmux(
            "rename-workspace",
            "--workspace",
            ws["workspace_ref"],
            "wm-home-test",
        )

        env = os.environ.copy()
        env["HOME"] = "/tmp"

        result = subprocess.run(
            ["cmux", "find-window", "wm-home-test"],
            capture_output=True,
            text=True,
            check=True,
            env=env,
        )
        assert ws["workspace_ref"] in result.stdout
