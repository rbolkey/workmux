"""Q1 + Additional Discoveries: Workspace creation, listing, renaming, and environment.

Verifies cmux workspace lifecycle commands behave as documented in the spike.
"""

import os
import re
import subprocess

from .helpers import run_cmux_json


class TestPingAndEnvironment:
    """Additional Discoveries: Basic cmux connectivity and env vars."""

    def test_ping(self):
        """cmux ping exits 0 when cmux is running."""
        result = subprocess.run(
            ["cmux", "ping"], capture_output=True, text=True, check=False
        )
        assert result.returncode == 0

    def test_env_vars_present(self):
        """CMUX_SOCKET_PATH, CMUX_WORKSPACE_ID, CMUX_SURFACE_ID are set in cmux terminals."""
        assert os.environ.get("CMUX_SOCKET_PATH"), "CMUX_SOCKET_PATH not set"
        assert os.environ.get("CMUX_WORKSPACE_ID"), "CMUX_WORKSPACE_ID not set"
        assert os.environ.get("CMUX_SURFACE_ID"), "CMUX_SURFACE_ID not set"


class TestWorkspaceCreation:
    """Q1: Workspace creation and surface discovery."""

    def test_new_workspace_returns_uuid(self, cmux_workspace):
        """new-workspace stdout matches 'OK <UUID>' format."""
        uuid = cmux_workspace["uuid"]
        uuid_pattern = re.compile(
            r"^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$"
        )
        assert uuid_pattern.match(uuid), f"Expected UUID, got: {uuid}"

    def test_list_workspaces_json_schema(self, cmux_workspace):
        """Q1: list-workspaces JSON has window_ref string + workspaces array with expected fields."""
        data = run_cmux_json("list-workspaces")
        assert "window_ref" in data
        assert isinstance(data["window_ref"], str)
        assert "workspaces" in data
        assert isinstance(data["workspaces"], list)
        assert len(data["workspaces"]) > 0

        ws = data["workspaces"][0]
        for field in ("ref", "title", "selected", "pinned", "index"):
            assert field in ws, f"Missing field '{field}' in workspace entry"

    def test_list_pane_surfaces_json_schema(self, cmux_workspace):
        """Q1: list-pane-surfaces JSON has surfaces array with ref field."""
        data = run_cmux_json(
            "list-pane-surfaces", "--workspace", cmux_workspace["workspace_ref"]
        )
        assert "surfaces" in data
        assert isinstance(data["surfaces"], list)
        assert len(data["surfaces"]) > 0
        assert "ref" in data["surfaces"][0]

    def test_new_workspace_surface_discovery_flow(self, cmux_workspace):
        """Q1: create -> list-workspaces -> list-pane-surfaces pipeline yields valid surface ref."""
        ws_ref = cmux_workspace["workspace_ref"]
        surface_ref = cmux_workspace["surface_ref"]

        assert ws_ref.startswith("workspace:")
        assert surface_ref.startswith("surface:")


class TestWorkspaceManagement:
    """Additional Discoveries: Renaming and flag behavior."""

    def test_rename_workspace_positional_title(self, cmux_workspace):
        """Additional: rename-workspace uses positional title arg (not --title flag)."""
        ws_ref = cmux_workspace["workspace_ref"]
        title = "wm-learning-test-rename"

        subprocess.run(
            ["cmux", "rename-workspace", "--workspace", ws_ref, title],
            capture_output=True,
            text=True,
            check=True,
        )

        # Verify title appears in list-workspaces
        data = run_cmux_json("list-workspaces")
        titles = [ws["title"] for ws in data["workspaces"]]
        assert title in titles, f"Renamed title not found. Available: {titles}"

    def test_new_workspace_no_title_flag(self):
        """Additional: new-workspace --title fails (only --command accepted)."""
        result = subprocess.run(
            ["cmux", "new-workspace", "--title", "should-fail"],
            capture_output=True,
            text=True,
            check=False,
        )
        # If cmux ever accepts --title, clean up the leaked workspace
        if result.returncode == 0:
            uuid = result.stdout.strip().split()[-1]
            from .helpers import get_workspace_refs, run_cmux_json

            data = run_cmux_json("list-workspaces")
            for ws in data["workspaces"]:
                if ws.get("title") == "should-fail":
                    subprocess.run(
                        ["cmux", "close-workspace", "--workspace", ws["ref"]],
                        capture_output=True,
                        check=False,
                    )
                    break
        assert result.returncode != 0, "new-workspace --title should fail"
