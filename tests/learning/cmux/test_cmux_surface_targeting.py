"""Q3 + Q5: Surface targeting and new-split return format.

Verifies cross-workspace send/read and new-split behavior.
"""

import re
import subprocess

from .helpers import wait_for_screen_content


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
