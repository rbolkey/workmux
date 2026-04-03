"""Exploratory: tree, set-hook, capture-pane, pipe-pane capabilities.

These tests explore cmux capabilities not currently used by workmux but
potentially useful for solving known limitations:

- tree: per-surface TTY output (#2040) — may solve the surface-health PID gap
  documented in test_cmux_json_schemas.py::test_surface_health_no_pid
- set-hook: event-driven workspace monitoring — could replace polling patterns
- capture-pane / pipe-pane: alternatives to read-screen polling

Findings are documented in test docstrings and assertions. "Gap remains
unsolved" is a valid exploratory outcome.
"""

import json
import os
import subprocess
import tempfile
import time

import pytest

from .helpers import wait_for_screen_content


class TestTree:
    """tree: workspace/surface tree with per-surface TTY (#2040, v0.63.0)."""

    def test_tree_json_returns_valid_json(self, cmux_workspace):
        """cmux tree --json returns valid JSON with surface entries."""
        result = subprocess.run(
            ["cmux", "tree", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"tree --json not available: {result.stderr.strip()}")

        data = json.loads(result.stdout)
        # tree should return some structure — document the top-level keys
        assert isinstance(data, (dict, list)), (
            f"Expected dict or list, got {type(data).__name__}"
        )

    def test_tree_includes_tty_per_surface(self, cmux_split):
        """tree output includes TTY path per surface (v0.63.0 #2040)."""
        result = subprocess.run(
            ["cmux", "tree", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"tree --json not available: {result.stderr.strip()}")

        data = json.loads(result.stdout)
        stdout = json.dumps(data)

        # Search for TTY-related fields anywhere in the output
        tty_found = any(
            key in stdout.lower() for key in ("tty", "pty", "/dev/pts", "/dev/ttys")
        )
        if not tty_found:
            pytest.skip(
                "tree output does not contain TTY information. "
                f"Top-level keys: {list(data.keys()) if isinstance(data, dict) else 'list'}"
            )

    def test_tree_pid_availability(self, cmux_split):
        """KEY DISCOVERY: does tree include PID per surface?

        The surface-health command lacks PID (see test_cmux_json_schemas.py).
        If tree exposes PID, it could fill that gap for workmux.
        """
        result = subprocess.run(
            ["cmux", "tree", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"tree --json not available: {result.stderr.strip()}")

        data = json.loads(result.stdout)
        stdout = json.dumps(data)

        pid_found = "pid" in stdout.lower()
        if pid_found:
            # PID IS available in tree — document where
            pass  # Test passes — PID gap can be filled via tree
        else:
            # PID is NOT in tree — gap remains unsolved
            pytest.skip(
                "tree does NOT include PID. The surface-health PID gap remains. "
                f"Available data: {stdout[:500]}"
            )

    def test_tree_text_output(self, cmux_workspace):
        """tree without --json returns human-readable text output."""
        result = subprocess.run(
            ["cmux", "tree"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"tree not available: {result.stderr.strip()}")

        assert len(result.stdout.strip()) > 0, "tree returned empty output"
        # Should not be valid JSON (that's what --json is for)
        try:
            json.loads(result.stdout)
            # If it IS valid JSON even without --json, document that
        except json.JSONDecodeError:
            pass  # Expected: text output is not JSON


class TestSetHook:
    """set-hook: event-driven workspace monitoring (tmux compat layer).

    Exploratory: document which event names are valid and whether hooks fire.
    """

    def test_set_hook_exits_zero(self, cmux_workspace):
        """set-hook with a plausible event name exits 0."""
        # Try common tmux-style hook events
        for event in ("after-new-window", "after-new-session", "window-renamed"):
            result = subprocess.run(
                [
                    "cmux",
                    "set-hook",
                    "-g",
                    event,
                    "run-shell 'echo hook-test'",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                return  # At least one event name is valid
        pytest.skip(
            "No tested hook event names were accepted by set-hook. "
            "set-hook may not be fully implemented."
        )

    def test_set_hook_and_unhook(self, cmux_workspace):
        """set-hook followed by set-hook -u (unhook) both exit 0."""
        event = "after-new-window"

        set_result = subprocess.run(
            [
                "cmux",
                "set-hook",
                "-g",
                event,
                "run-shell 'echo test'",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if set_result.returncode != 0:
            pytest.skip(f"set-hook not supported: {set_result.stderr.strip()}")

        # Unhook
        unset_result = subprocess.run(
            ["cmux", "set-hook", "-gu", event],
            capture_output=True,
            text=True,
            check=False,
        )
        assert unset_result.returncode == 0, (
            f"set-hook -gu (unhook) failed: {unset_result.stderr}"
        )


class TestCapturePanePipePane:
    """capture-pane and pipe-pane: alternatives to read-screen polling.

    Document semantic differences from read-screen.
    """

    def test_capture_pane_returns_content(self, cmux_workspace):
        """capture-pane returns screen content similar to read-screen."""
        ws = cmux_workspace

        # Send content first
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "echo capture-test-marker\n",
            ],
            capture_output=True,
            check=False,
        )
        wait_for_screen_content(
            ws["workspace_ref"], ws["surface_ref"], "capture-test-marker"
        )

        # Try capture-pane
        result = subprocess.run(
            [
                "cmux",
                "capture-pane",
                "-p",  # print to stdout (tmux compat)
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"capture-pane not available: {result.stderr.strip()}")

        assert "capture-test-marker" in result.stdout, (
            "capture-pane output should contain screen content"
        )

    def test_capture_pane_vs_read_screen(self, cmux_workspace):
        """Document differences between capture-pane and read-screen output."""
        ws = cmux_workspace

        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "echo compare-test\n",
            ],
            capture_output=True,
            check=False,
        )
        wait_for_screen_content(ws["workspace_ref"], ws["surface_ref"], "compare-test")

        read_screen = subprocess.run(
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
            check=True,
        )

        capture_pane = subprocess.run(
            [
                "cmux",
                "capture-pane",
                "-p",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if capture_pane.returncode != 0:
            pytest.skip("capture-pane not available")

        # Both should contain the test content
        assert "compare-test" in read_screen.stdout
        assert "compare-test" in capture_pane.stdout

        # Document: are they identical or different?
        # (trailing whitespace, history depth, etc.)

    def test_pipe_pane_streams_to_file(self, cmux_workspace):
        """pipe-pane streams pane output to a file."""
        ws = cmux_workspace

        with tempfile.NamedTemporaryFile(mode="r", suffix=".txt", delete=False) as f:
            pipe_path = f.name

        try:
            result = subprocess.run(
                [
                    "cmux",
                    "pipe-pane",
                    "--workspace",
                    ws["workspace_ref"],
                    "--surface",
                    ws["surface_ref"],
                    f"cat >> {pipe_path}",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                pytest.skip(f"pipe-pane not available: {result.stderr.strip()}")

            # Send some content
            subprocess.run(
                [
                    "cmux",
                    "send",
                    "--workspace",
                    ws["workspace_ref"],
                    "--surface",
                    ws["surface_ref"],
                    "echo pipe-test-marker\n",
                ],
                capture_output=True,
                check=False,
            )

            # Wait for pipe to capture
            time.sleep(1.0)

            # Stop the pipe
            subprocess.run(
                [
                    "cmux",
                    "pipe-pane",
                    "--workspace",
                    ws["workspace_ref"],
                    "--surface",
                    ws["surface_ref"],
                ],
                capture_output=True,
                check=False,
            )

            # Check if content was captured
            with open(pipe_path) as f:
                content = f.read()
            if "pipe-test-marker" in content:
                pass  # pipe-pane works as expected
            else:
                pytest.skip(
                    f"pipe-pane did not capture content. File contents: {content[:200]}"
                )
        finally:
            os.unlink(pipe_path)
