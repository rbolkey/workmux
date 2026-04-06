"""Learning tests for cmux identify per-process semantics.

These tests verify that `cmux identify` returns the caller's workspace
(per-process), not the globally selected workspace. This distinction is
critical for workmux's `current_window_name()` implementation — using
the wrong primitive causes close/remove to target the wrong window.

Discovery: 2026-04-03, during investigation of duplicate window close bug.
"""

import json
import subprocess
import time

from .helpers import run_cmux, run_cmux_json, wait_for_screen_content


class TestIdentifyPerProcess:
    """cmux identify returns per-process caller info, not global selection."""

    def test_identify_caller_matches_workspace(self, cmux_workspace, tmp_path):
        """identify run inside a workspace returns that workspace's ref."""
        ws = cmux_workspace
        ws_ref = ws["workspace_ref"]
        surf_ref = ws["surface_ref"]

        # Wait for shell readiness
        time.sleep(1.5)

        # Write identify output to a temp file (avoids quoting issues with cmux send)
        output_file = tmp_path / "identify_output.json"
        run_cmux(
            "send",
            "--workspace",
            ws_ref,
            "--surface",
            surf_ref,
            f"cmux identify > {output_file}\n",
        )

        # Wait for the file to be written
        deadline = time.time() + 5.0
        while time.time() < deadline:
            if output_file.exists() and output_file.stat().st_size > 0:
                break
            time.sleep(0.3)

        assert output_file.exists(), "identify output file should be created"
        data = json.loads(output_file.read_text())

        assert data["caller"]["workspace_ref"] == ws_ref, (
            f"identify caller workspace_ref should be {ws_ref}, "
            f"got {data['caller']['workspace_ref']}"
        )

    def test_identify_caller_differs_from_focused(self, cmux_two_workspaces):
        """identify.caller != identify.focused when caller is not the focused workspace."""
        primary = cmux_two_workspaces["primary"]
        secondary = cmux_two_workspaces["secondary"]

        # Focus the secondary workspace
        run_cmux("select-workspace", "--workspace", secondary["workspace_ref"])
        time.sleep(0.5)

        # Run identify from our process (which is in the primary/test workspace area)
        result = run_cmux("identify")
        data = json.loads(result.stdout)

        # The focused workspace should be the secondary (we just selected it)
        assert data["focused"]["workspace_ref"] == secondary["workspace_ref"], (
            "focused should be the workspace we just selected"
        )

        # The caller should be our process's workspace (not the focused one)
        # Note: the caller is the workspace of the process running this test,
        # which is the cmux workspace where pytest is running — NOT necessarily
        # the primary fixture workspace.
        assert data["caller"]["workspace_ref"] != data["focused"]["workspace_ref"], (
            "caller and focused should differ when a non-caller workspace is selected"
        )

    def test_identify_from_outside_returns_caller_process_workspace(self):
        """identify from the test process returns the test workspace, not any fixture workspace."""
        result = run_cmux("identify")
        data = json.loads(result.stdout)

        # The caller should have a workspace_ref (proves per-process tracking)
        assert "workspace_ref" in data["caller"]
        assert data["caller"]["workspace_ref"].startswith("workspace:")

        # caller.surface_ref should also be present
        assert "surface_ref" in data["caller"]
        assert data["caller"]["surface_ref"].startswith("surface:")
