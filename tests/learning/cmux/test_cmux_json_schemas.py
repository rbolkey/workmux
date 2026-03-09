"""Q4 + Q6: JSON output schemas and surface-health limitations.

Verifies identify, surface-health, and sidebar-state output formats.
"""

import json

import pytest

from .helpers import run_cmux, run_cmux_json


class TestIdentify:
    """Q4: cmux identify JSON schema."""

    def test_identify_json_schema(self):
        """Q4: identify returns JSON with caller and focused sections containing refs."""
        # identify emits JSON by default (does not require --json flag)
        result = run_cmux("identify")
        data = json.loads(result.stdout)

        assert "caller" in data
        assert "focused" in data

        for section_name in ("caller", "focused"):
            section = data[section_name]
            for field in ("surface_ref", "workspace_ref", "pane_ref"):
                assert field in section, f"Missing '{field}' in identify.{section_name}"


class TestSurfaceHealth:
    """Q4 + Q6: surface-health JSON schema and limitations."""

    def test_surface_health_json_schema(self):
        """Q4: surface-health returns surfaces array with ref/type/in_window/index."""
        data = run_cmux_json("surface-health")
        assert "surfaces" in data
        assert isinstance(data["surfaces"], list)
        assert len(data["surfaces"]) > 0

        surface = data["surfaces"][0]
        for field in ("ref", "type", "in_window", "index"):
            assert field in surface, f"Missing '{field}' in surface-health entry"

    def test_surface_health_no_pid(self):
        """Q6: surface-health does NOT include PID (documents the gap)."""
        data = run_cmux_json("surface-health")
        for surface in data["surfaces"]:
            assert "pid" not in surface, (
                f"Unexpected 'pid' field in surface-health: {surface}"
            )


class TestSidebarState:
    """Q6: sidebar-state output format."""

    def test_sidebar_state_has_cwd(self):
        """Q6: sidebar-state output contains cwd= line."""
        result = run_cmux("sidebar-state")
        assert "cwd=" in result.stdout, (
            f"Expected 'cwd=' in sidebar-state output: {result.stdout}"
        )

    def test_sidebar_state_no_json(self):
        """Q6: sidebar-state output is key=value format, not JSON."""
        result = run_cmux("sidebar-state")
        with pytest.raises(json.JSONDecodeError):
            json.loads(result.stdout)
