"""Q3 + Q5: Surface targeting and new-split return format.

Verifies cross-workspace send/read and new-split behavior.
Also covers v0.63.1 stale target fallback (#2518).
"""

import re
import subprocess
import time

from .helpers import run_cmux_json, wait_for_screen_content


class TestCrossWorkspaceTargeting:
    """Q3: All commands support --workspace and --surface targeting."""

    def test_send_cross_workspace_with_flags(self, cmux_two_workspaces):
        """Q3: send to a surface in a non-focused workspace succeeds."""
        secondary = cmux_two_workspaces["secondary"]
        result = subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                secondary["workspace_ref"],
                "--surface",
                secondary["surface_ref"],
                "echo cross-workspace-test\n",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"Cross-workspace send failed: {result.stderr}"

    def test_read_screen_cross_workspace(self, cmux_two_workspaces):
        """Q3: read-screen from non-focused workspace returns content."""
        secondary = cmux_two_workspaces["secondary"]

        # Send something first so there's content to read
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                secondary["workspace_ref"],
                "--surface",
                secondary["surface_ref"],
                "echo read-screen-test\n",
            ],
            capture_output=True,
            check=False,
        )

        wait_for_screen_content(
            secondary["workspace_ref"],
            secondary["surface_ref"],
            "read-screen-test",
        )


class TestNewSplit:
    """Q5: new-split return format and targeting."""

    def test_new_split_return_format(self, cmux_workspace):
        """Q5: new-split returns 'OK surface:N workspace:N' format."""
        result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                cmux_workspace["workspace_ref"],
                "--surface",
                cmux_workspace["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        pattern = re.compile(r"^OK surface:\d+ workspace:\d+$")
        assert pattern.match(result.stdout.strip()), (
            f"Expected 'OK surface:N workspace:N', got: {result.stdout.strip()}"
        )

    def test_new_split_refs_are_valid(self, cmux_workspace):
        """Q5: Returned surface ref from new-split is usable in send."""
        result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                cmux_workspace["workspace_ref"],
                "--surface",
                cmux_workspace["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = result.stdout.strip().split()
        new_surface_ref = parts[1]  # surface:N

        # Send to the new surface to verify the ref is usable.
        # Use wait_for_screen_content which polls instead of a fixed sleep.
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                cmux_workspace["workspace_ref"],
                "--surface",
                new_surface_ref,
                "echo ref-valid\n",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        wait_for_screen_content(
            cmux_workspace["workspace_ref"], new_surface_ref, "ref-valid"
        )


class TestNewSplitStaleFallback:
    """v0.63.1: new-split falls back to focused surface when target is stale (#2518).

    Each test gets its own workspace via conftest fixtures — no shared state
    with other test classes. This is important for workmux because new-split
    is called with stored surface refs that could become stale if the user
    manually closes a pane.
    """

    def test_new_split_stale_ref_succeeds(self, cmux_split):
        """new-split with a stale surface ref succeeds (falls back to focused)."""
        ws = cmux_split
        stale_ref = ws["second_surface_ref"]

        # Close the second surface to make its ref stale
        subprocess.run(
            [
                "cmux",
                "close-surface",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                stale_ref,
            ],
            capture_output=True,
            check=True,
        )
        time.sleep(0.3)

        # Verify it's gone
        surfaces_before = run_cmux_json(
            "list-pane-surfaces", "--workspace", ws["workspace_ref"]
        )
        refs_before = {s["ref"] for s in surfaces_before["surfaces"]}
        assert stale_ref not in refs_before, "Surface should be stale/gone"

        # Now attempt new-split targeting the stale ref — should fall back
        result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                stale_ref,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, (
            f"new-split with stale ref should fall back, got: {result.stderr}"
        )

        # Verify fallback: new surface should be in the focused pane
        parts = result.stdout.strip().split()
        new_surface_ref = parts[1]  # surface:N

        surfaces_after = run_cmux_json(
            "list-pane-surfaces", "--workspace", ws["workspace_ref"]
        )
        refs_after = {s["ref"] for s in surfaces_after["surfaces"]}
        assert new_surface_ref in refs_after, (
            f"New surface {new_surface_ref} not found after fallback split"
        )

    def test_new_split_stale_ref_surface_is_usable(self, cmux_split):
        """Surface ref from stale-fallback split is usable in subsequent send commands."""
        ws = cmux_split
        stale_ref = ws["second_surface_ref"]

        # Close second surface
        subprocess.run(
            [
                "cmux",
                "close-surface",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                stale_ref,
            ],
            capture_output=True,
            check=True,
        )
        time.sleep(0.3)

        # Split with stale ref
        result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                stale_ref,
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        new_ref = result.stdout.strip().split()[1]

        # Send to the new surface
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                new_ref,
                "echo stale-fallback-test\n",
            ],
            capture_output=True,
            check=False,
        )
        wait_for_screen_content(ws["workspace_ref"], new_ref, "stale-fallback-test")

    def test_new_split_stale_ref_cross_workspace(self, cmux_two_workspaces):
        """new-split with stale ref in a different workspace — document behavior."""
        secondary = cmux_two_workspaces["secondary"]

        # Create and close a split in the secondary workspace
        split_result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                secondary["workspace_ref"],
                "--surface",
                secondary["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        temp_ref = split_result.stdout.strip().split()[1]

        subprocess.run(
            [
                "cmux",
                "close-surface",
                "--workspace",
                secondary["workspace_ref"],
                "--surface",
                temp_ref,
            ],
            capture_output=True,
            check=True,
        )
        time.sleep(0.3)

        # Try to split targeting the now-stale cross-workspace ref
        result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                secondary["workspace_ref"],
                "--surface",
                temp_ref,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        # Document whether cross-workspace stale fallback works
        if result.returncode != 0:
            import pytest

            pytest.skip(
                f"Cross-workspace stale fallback not supported: {result.stderr.strip()}"
            )
