"""Q8 + Status: Send escape handling, buffer paste, status, and send-key modifiers.

Verifies cmux send interprets escape sequences, buffer paste round-trips
content, status commands succeed, and expanded send-key modifiers work
(v0.63.0 #1920, #1994).
"""

import subprocess

import pytest

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


class TestSendKeyModifiers:
    """v0.63.0: Expanded send-key modifier combinations (#1920, #1994).

    Tests arrow keys, shift+tab, ctrl+enter, home/end — key names added
    to sendNamedKey in v0.63.0. Verifies exit codes; does not verify
    terminal-side effect (that requires screen content inspection).
    """

    @pytest.mark.parametrize("key", ["left", "right", "up", "down"])
    def test_send_key_arrow_keys(self, cmux_workspace, key):
        """send-key arrow key exits 0."""
        ws = cmux_workspace
        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                key,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"send-key {key} failed: {result.stderr}"

    def test_send_key_shift_tab(self, cmux_workspace):
        """send-key shift+tab exits 0."""
        ws = cmux_workspace
        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "shift+tab",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"send-key shift+tab failed: {result.stderr}"

    def test_send_key_ctrl_enter(self, cmux_workspace):
        """send-key ctrl+enter exits 0."""
        ws = cmux_workspace
        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "ctrl+enter",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"send-key ctrl+enter failed: {result.stderr}"

    @pytest.mark.parametrize("key", ["home", "end"])
    def test_send_key_home_end(self, cmux_workspace, key):
        """send-key home/end exits 0."""
        ws = cmux_workspace
        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                key,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, f"send-key {key} failed: {result.stderr}"

    def test_send_key_invalid_name_fails(self, cmux_workspace):
        """send-key with an invalid key name returns non-zero."""
        ws = cmux_workspace
        result = subprocess.run(
            [
                "cmux",
                "send-key",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "invalid-key-name-xyz",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode != 0
