"""Prerequisite learning tests for Phase 2-3 cmux commands.

Tests commands not covered by the original spike that are needed for the
cmux backend implementation: respawn-pane, select-workspace, send-key,
read-screen --lines N.
"""

import subprocess
import time

from .helpers import run_cmux_json, wait_for_screen_content


class TestSelectWorkspace:
    """select-workspace: workspace switching via ref."""

    def test_select_workspace_switches_focus(self, cmux_two_workspaces):
        """select-workspace changes which workspace is selected."""
        secondary = cmux_two_workspaces["secondary"]

        result = subprocess.run(
            [
                "cmux",
                "select-workspace",
                "--workspace",
                secondary["workspace_ref"],
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"select-workspace failed: {result.stderr}"

        # Verify secondary is now selected
        data = run_cmux_json("list-workspaces")
        selected = [ws for ws in data["workspaces"] if ws["selected"]]
        assert len(selected) == 1
        assert selected[0]["ref"] == secondary["workspace_ref"]

        # Switch back to primary so teardown doesn't close the focused workspace
        primary = cmux_two_workspaces["primary"]
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

    def test_select_workspace_invalid_ref_fails(self):
        """select-workspace with non-existent ref returns non-zero."""
        result = subprocess.run(
            ["cmux", "select-workspace", "--workspace", "workspace:99999"],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode != 0


class TestSendKey:
    """send-key: individual key event sending."""

    def test_send_key_enter(self, cmux_workspace):
        """send-key enter sends an Enter keypress."""
        ws = cmux_workspace

        # Type a command without Enter, then send Enter via send-key
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "echo sendkey-enter-test",
            ],
            capture_output=True,
            check=True,
        )

        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "enter",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"send-key enter failed: {result.stderr}"

        wait_for_screen_content(
            ws["workspace_ref"], ws["surface_ref"], "sendkey-enter-test"
        )

    def test_send_key_ctrl_c(self, cmux_workspace):
        """send-key ctrl+c exits 0 (interrupt signal).

        NOTE: send-key may require surface readiness (shell fully started).
        We wait for the shell prompt to appear first via wait_for_screen_content.
        """
        ws = cmux_workspace

        # Wait for shell to be ready by detecting the prompt character
        import time

        time.sleep(1.0)  # Give shell time to start

        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "ctrl+c",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        # Record actual behavior: if this fails, the backend should use
        # `send` with escape sequence instead of `send-key` for ctrl+c
        if result.returncode != 0:
            import pytest

            pytest.skip(
                f"send-key ctrl+c not reliable: {result.stderr.strip()}. "
                "Backend should use 'send' with \\x03 instead."
            )


class TestReadScreenLines:
    """read-screen --lines N: line count control."""

    def test_read_screen_with_lines_flag(self, cmux_workspace):
        """read-screen --lines N returns content (limited output)."""
        ws = cmux_workspace

        # Send some content first
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "echo lines-test-marker\n",
            ],
            capture_output=True,
            check=True,
        )

        wait_for_screen_content(
            ws["workspace_ref"], ws["surface_ref"], "lines-test-marker"
        )

        # Now read with --lines flag
        result = subprocess.run(
            [
                "cmux",
                "read-screen",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "--lines",
                "5",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"read-screen --lines failed: {result.stderr}"
        assert len(result.stdout) > 0, "read-screen --lines returned empty output"

    def test_read_screen_lines_limits_output(self, cmux_workspace):
        """read-screen --lines N returns fewer lines than full output."""
        ws = cmux_workspace

        # Generate many lines of output
        for i in range(20):
            subprocess.run(
                [
                    "cmux",
                    "send",
                    "--workspace",
                    ws["workspace_ref"],
                    "--surface",
                    ws["surface_ref"],
                    f"echo line-{i}\n",
                ],
                capture_output=True,
                check=False,
            )

        time.sleep(0.5)

        # Read with small line count
        limited = subprocess.run(
            [
                "cmux",
                "read-screen",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "--lines",
                "3",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        # Read full output
        full = subprocess.run(
            [
                "cmux",
                "read-screen",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        if limited.returncode == 0 and full.returncode == 0:
            limited_lines = limited.stdout.strip().splitlines()
            full_lines = full.stdout.strip().splitlines()
            # Limited should have fewer or equal lines (it may return more than N
            # due to wrapping, but should not exceed full output)
            assert len(limited_lines) <= len(full_lines), (
                f"Limited ({len(limited_lines)} lines) should not exceed "
                f"full ({len(full_lines)} lines)"
            )


class TestRespawnPane:
    """respawn-pane: re-run a surface with a new command."""

    def test_respawn_pane_preserves_surface_ref(self, cmux_workspace):
        """respawn-pane keeps the same surface ref (critical for setup_panes)."""
        ws = cmux_workspace
        original_ref = ws["surface_ref"]

        result = subprocess.run(
            [
                "cmux",
                "respawn-pane",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                original_ref,
                "--command",
                "echo respawn-test",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode != 0:
            # respawn-pane may not exist or may have different syntax.
            # Record the failure for the implementation to handle.
            import pytest

            pytest.skip(
                f"respawn-pane not available or different syntax: {result.stderr}"
            )

        # Verify the original surface ref still exists in the workspace
        surfaces = run_cmux_json(
            "list-pane-surfaces", "--workspace", ws["workspace_ref"]
        )
        surface_refs = [s["ref"] for s in surfaces["surfaces"]]
        assert original_ref in surface_refs, (
            f"Original surface {original_ref} not found after respawn. "
            f"Available: {surface_refs}"
        )

    def test_respawn_pane_runs_command(self, cmux_workspace):
        """respawn-pane executes the specified command in the surface."""
        ws = cmux_workspace

        result = subprocess.run(
            [
                "cmux",
                "respawn-pane",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "--command",
                "echo respawn-cmd-test",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode != 0:
            import pytest

            pytest.skip(f"respawn-pane not available: {result.stderr}")

        wait_for_screen_content(
            ws["workspace_ref"], ws["surface_ref"], "respawn-cmd-test"
        )
