"""Learning tests for cmux find-window behavior.

These tests verify find-window's substring matching semantics and result
ordering. workmux's `shell_kill_window_cmd` uses find-window to resolve
workspace names to refs at execution time.

Discovery: 2026-04-03, during investigation of duplicate window close bug.
"""

import subprocess

import pytest

from .helpers import run_cmux


class TestFindWindow:
    """cmux find-window does substring matching on workspace titles."""

    def test_find_window_exact_match(self, cmux_workspace):
        """find-window with an exact title returns the workspace."""
        ws = cmux_workspace

        # Rename to a known title
        run_cmux(
            "rename-workspace",
            "--workspace",
            ws["workspace_ref"],
            "wm-findtest-exact",
        )

        result = run_cmux("find-window", "wm-findtest-exact")
        lines = result.stdout.strip().splitlines()

        assert len(lines) >= 1
        assert ws["workspace_ref"] in lines[0]

    def test_find_window_substring_matches_multiple(self, cmux_two_workspaces):
        """find-window with a base name returns both base and suffixed workspaces."""
        primary = cmux_two_workspaces["primary"]
        secondary = cmux_two_workspaces["secondary"]

        # Rename to similar names (base + duplicate suffix pattern)
        run_cmux(
            "rename-workspace",
            "--workspace",
            primary["workspace_ref"],
            "wm-findtest-dup",
        )
        run_cmux(
            "rename-workspace",
            "--workspace",
            secondary["workspace_ref"],
            "wm-findtest-dup-2",
        )

        # Search for the base name — should match both
        result = run_cmux("find-window", "wm-findtest-dup")
        lines = result.stdout.strip().splitlines()

        assert len(lines) == 2, (
            f"Expected 2 matches for substring 'wm-findtest-dup', got {len(lines)}: {lines}"
        )

        # Both workspace refs should appear
        refs_found = {line.split()[0] for line in lines}
        assert primary["workspace_ref"] in refs_found
        assert secondary["workspace_ref"] in refs_found

    def test_find_window_specific_suffix_matches_one(self, cmux_two_workspaces):
        """find-window with the full suffixed name returns only that workspace."""
        primary = cmux_two_workspaces["primary"]
        secondary = cmux_two_workspaces["secondary"]

        run_cmux(
            "rename-workspace",
            "--workspace",
            primary["workspace_ref"],
            "wm-findtest-suf",
        )
        run_cmux(
            "rename-workspace",
            "--workspace",
            secondary["workspace_ref"],
            "wm-findtest-suf-2",
        )

        # Search for the suffixed name — should match only the suffixed workspace
        result = run_cmux("find-window", "wm-findtest-suf-2")
        lines = result.stdout.strip().splitlines()

        assert len(lines) == 1, (
            f"Expected 1 match for 'wm-findtest-suf-2', got {len(lines)}: {lines}"
        )
        assert secondary["workspace_ref"] in lines[0]

    def test_find_window_output_format(self, cmux_workspace):
        """find-window returns 'workspace:N  "title"' format."""
        ws = cmux_workspace
        run_cmux(
            "rename-workspace",
            "--workspace",
            ws["workspace_ref"],
            "wm-findtest-fmt",
        )

        result = run_cmux("find-window", "wm-findtest-fmt")
        line = result.stdout.strip().splitlines()[0]

        # Format: workspace:N  "title"
        parts = line.split(None, 1)
        assert parts[0] == ws["workspace_ref"]
        assert '"wm-findtest-fmt"' in parts[1]

    def test_find_window_json_flag(self, cmux_workspace):
        """Q: Does find-window support --json output?

        FINDING (2026-04-06): --json flag is accepted (exit 0) but output
        is still the text format 'workspace:N  "title"', not JSON.
        Must parse text format.
        """
        ws = cmux_workspace
        run_cmux(
            "rename-workspace",
            "--workspace",
            ws["workspace_ref"],
            "wm-findtest-json",
        )

        result = subprocess.run(
            ["cmux", "find-window", "--json", "wm-findtest-json"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"find-window --json not supported: {result.stderr.strip()}")

        import json

        try:
            data = json.loads(result.stdout)
            # If we get here, --json is now genuinely supported
            assert isinstance(data, (dict, list)), (
                f"Expected dict or list, got {type(data).__name__}: {result.stdout[:200]}"
            )
        except json.JSONDecodeError:
            # --json flag silently ignored — output is text format.
            # Verify it's the expected text format instead.
            line = result.stdout.strip().splitlines()[0]
            assert ws["workspace_ref"] in line, (
                f"Expected workspace ref in text output: {line}"
            )

    def test_find_window_exact_flag_is_ignored(self, cmux_two_workspaces):
        """Q: Does find-window support --exact matching?

        FINDING (2026-04-06): --exact flag is accepted (exit 0) but IGNORED.
        Searching for "wm-findtest-ex" with --exact still returns both
        "wm-findtest-ex" and "wm-findtest-ex-2". Client-side exact filtering
        is REQUIRED after find-window calls.
        """
        primary = cmux_two_workspaces["primary"]
        secondary = cmux_two_workspaces["secondary"]

        run_cmux(
            "rename-workspace",
            "--workspace",
            primary["workspace_ref"],
            "wm-findtest-ex",
        )
        run_cmux(
            "rename-workspace",
            "--workspace",
            secondary["workspace_ref"],
            "wm-findtest-ex-2",
        )

        result = subprocess.run(
            ["cmux", "find-window", "--exact", "wm-findtest-ex"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"find-window --exact not supported: {result.stderr.strip()}")

        lines = result.stdout.strip().splitlines()
        # --exact is silently ignored — still returns substring matches.
        # Client-side exact filtering is required.
        assert len(lines) == 2, (
            f"Expected 2 substring matches (--exact ignored), got {len(lines)}: {lines}"
        )

    def test_find_window_no_match_fails(self):
        """find-window with a non-existent title returns non-zero."""
        result = subprocess.run(
            ["cmux", "find-window", "wm-nonexistent-window-xyz-999"],
            capture_output=True,
            text=True,
            check=False,
        )
        # find-window returns exit 0 with "No matches" text
        assert result.stdout.strip() == "No matches" or result.returncode != 0
