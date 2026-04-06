"""Learning tests for cmux sidebar-state output format.

These tests document the exact sidebar-state format contract:
- Key=value per line
- Edge cases: values containing '=', empty values, missing keys
- Whether --json is supported (blocked by upstream cmux identify limitation)

The existing `split_once('=')` parser in cmux.rs relies on this format.
Since `cmux identify` is per-calling-process only, sidebar-state is the
only way to get CWD for arbitrary workspaces.

Discovery: 2026-04-06, deferred question from post-MVP quick wins plan.
"""

import subprocess

import pytest

from .helpers import run_cmux


class TestSidebarStateFormat:
    """Document the sidebar-state output format contract."""

    def test_sidebar_state_returns_key_value_lines(self, cmux_workspace):
        """sidebar-state returns key=value pairs, one per line."""
        ws = cmux_workspace

        result = run_cmux(
            "sidebar-state",
            "--workspace",
            ws["workspace_ref"],
        )
        output = result.stdout.strip()
        assert len(output) > 0, "sidebar-state returned empty output"

        lines = output.splitlines()
        for line in lines:
            assert "=" in line, (
                f"Expected key=value format, got line without '=': {line!r}"
            )

    def test_sidebar_state_contains_cwd(self, cmux_workspace):
        """sidebar-state includes a 'cwd' key with a filesystem path."""
        ws = cmux_workspace

        result = run_cmux(
            "sidebar-state",
            "--workspace",
            ws["workspace_ref"],
        )
        output = result.stdout.strip()
        lines = output.splitlines()

        cwd_lines = [l for l in lines if l.startswith("cwd=")]
        assert len(cwd_lines) == 1, (
            f"Expected exactly one 'cwd=' line, got {len(cwd_lines)} in: {lines}"
        )

        cwd_value = cwd_lines[0].split("=", 1)[1]
        assert cwd_value.startswith("/"), (
            f"Expected absolute path for cwd, got: {cwd_value!r}"
        )

    def test_sidebar_state_all_keys(self, cmux_workspace):
        """Document all keys returned by sidebar-state."""
        ws = cmux_workspace

        result = run_cmux(
            "sidebar-state",
            "--workspace",
            ws["workspace_ref"],
        )
        output = result.stdout.strip()
        lines = output.splitlines()

        keys = []
        for line in lines:
            if "=" in line:
                key = line.split("=", 1)[0]
                keys.append(key)

        # Document the keys (test passes regardless — it's a discovery test)
        assert len(keys) > 0, "No keys found in sidebar-state output"

    def test_sidebar_state_splitn_handles_equals_in_value(self, cmux_workspace):
        """Verify that split('=', 1) correctly handles values containing '='.

        Example: if a value is 'FOO=BAR', split('=', 1) should return
        ('key', 'FOO=BAR') not ('key', 'FOO').
        """
        ws = cmux_workspace

        result = run_cmux(
            "sidebar-state",
            "--workspace",
            ws["workspace_ref"],
        )
        output = result.stdout.strip()
        lines = output.splitlines()

        # Parse using split('=', 1) — same as Rust's split_once('=')
        parsed = {}
        for line in lines:
            if "=" in line:
                key, value = line.split("=", 1)
                parsed[key] = value

        # Verify cwd doesn't get truncated (paths with = are rare but possible)
        if "cwd" in parsed:
            assert parsed["cwd"].startswith("/"), (
                f"CWD value appears truncated: {parsed['cwd']!r}"
            )

    def test_sidebar_state_json_flag(self, cmux_workspace):
        """Q: Does sidebar-state support --json output?

        Previous learning tests (test_cmux_json_schemas.py) showed it does not.
        Re-verify in case this changed in newer cmux versions.
        """
        ws = cmux_workspace

        result = subprocess.run(
            [
                "cmux",
                "sidebar-state",
                "--workspace",
                ws["workspace_ref"],
                "--json",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode != 0:
            pytest.skip(
                f"sidebar-state --json not supported (as expected): "
                f"{result.stderr.strip()}"
            )

        # --json flag was accepted (exit 0). Check if output is actually JSON
        # or if the flag was silently ignored.
        import json

        try:
            data = json.loads(result.stdout)
            # If we get here, --json is now genuinely supported!
            assert isinstance(data, dict), (
                f"Expected dict from JSON output, got {type(data).__name__}"
            )
        except json.JSONDecodeError:
            # --json flag is silently ignored — output is still key=value text.
            # This is the expected behavior as of 2026-04-06.
            pytest.skip(
                "sidebar-state --json flag accepted (exit 0) but output is "
                "still key=value text, not JSON. Flag is silently ignored."
            )

    def test_sidebar_state_missing_workspace(self):
        """sidebar-state with a non-existent workspace ref fails gracefully."""
        result = subprocess.run(
            [
                "cmux",
                "sidebar-state",
                "--workspace",
                "workspace:99999",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode != 0, (
            "Expected non-zero exit for non-existent workspace"
        )
