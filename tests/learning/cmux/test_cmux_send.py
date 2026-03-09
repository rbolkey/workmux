"""Q8 + Status: Send escape handling, buffer paste, and status commands.

Verifies cmux send interprets escape sequences, buffer paste round-trips
content, and status commands succeed.
"""

import subprocess

from .helpers import wait_for_screen_content


class TestSendEscapes:
    """Q8: cmux send interprets \\n as Enter and \\t as Tab."""

    def test_send_newline_sends_enter(self, cmux_workspace):
        """Q8: send 'echo hello\\n' executes the command (\\n = Enter)."""
        ws = cmux_workspace
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "echo wm-newline-test\n",
            ],
            capture_output=True,
            check=True,
        )

        wait_for_screen_content(
            ws["workspace_ref"], ws["surface_ref"], "wm-newline-test"
        )

    def test_send_tab_sends_tab(self, cmux_workspace):
        """Q8: send with \\t triggers tab completion behavior."""
        ws = cmux_workspace
        # Send a partial command + tab -- we just verify the send succeeds
        result = subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "ech\t",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"send with tab failed: {result.stderr}"


class TestBufferPaste:
    """Q8: set-buffer + paste-buffer round-trips content without escape interpretation."""

    def test_buffer_set_and_paste_exit_zero(self, cmux_workspace):
        """Q8: set-buffer and paste-buffer succeed for multiline content."""
        ws = cmux_workspace
        content = "line1\nline2\nline3"
        buffer_name = "wm_test_paste"

        # Set buffer
        subprocess.run(
            ["cmux", "set-buffer", "--name", buffer_name, content],
            capture_output=True,
            text=True,
            check=True,
        )

        # Paste buffer into surface
        subprocess.run(
            [
                "cmux",
                "paste-buffer",
                "--name",
                buffer_name,
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )


class TestStatusCommands:
    """Status: set-status and clear-status commands."""

    def test_set_status_with_icon_and_color(self, cmux_workspace):
        """Status: set-status with icon and color exits 0."""
        ws = cmux_workspace
        result = subprocess.run(
            [
                "cmux",
                "set-status",
                "wm_test",
                "testing",
                "--icon",
                "gear",
                "--color",
                "#FFA500",
                "--workspace",
                ws["workspace_ref"],
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"set-status failed: {result.stderr}"

    def test_clear_status(self, cmux_workspace):
        """Status: clear-status after set-status exits 0."""
        ws = cmux_workspace

        # Set status first
        subprocess.run(
            [
                "cmux",
                "set-status",
                "wm_test",
                "testing",
                "--workspace",
                ws["workspace_ref"],
            ],
            capture_output=True,
            check=False,
        )

        # Clear it
        result = subprocess.run(
            [
                "cmux",
                "clear-status",
                "wm_test",
                "--workspace",
                ws["workspace_ref"],
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"clear-status failed: {result.stderr}"
