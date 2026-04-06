"""Q1 + Additional Discoveries: Workspace creation, listing, renaming, and environment.

Verifies cmux workspace lifecycle commands behave as documented in the spike.
"""

import os
import re
import subprocess

import pytest

from .helpers import run_cmux, run_cmux_json


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

    @pytest.mark.xfail(
        not os.environ.get("CMUX_SOCKET"),
        reason="CMUX_SOCKET is a v0.62.2+ alias; not present in older versions",
    )
    def test_cmux_socket_env_var(self):
        """v0.62.2: CMUX_SOCKET exported alongside CMUX_SOCKET_PATH (#1991)."""
        assert os.environ.get("CMUX_SOCKET"), "CMUX_SOCKET not set"


class TestWorkspaceCreation:
    """Q1: Workspace creation and surface discovery."""

    def test_new_workspace_returns_ref(self, cmux_workspace):
        """new-workspace stdout matches 'OK workspace:N' format (cmux >= 0.63.1)."""
        workspace_ref = cmux_workspace["workspace_ref"]
        ref_pattern = re.compile(r"^workspace:\d+$")
        assert ref_pattern.match(workspace_ref), (
            f"Expected workspace:N ref, got: {workspace_ref}"
        )

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

    def test_list_workspaces_description_field(self, cmux_workspace):
        """v0.63.0: list-workspaces JSON includes description field (#2475)."""
        data = run_cmux_json("list-workspaces")
        ws = data["workspaces"][0]
        # description may be null/empty for workspaces created without --description
        assert "description" in ws, (
            f"Missing 'description' field in workspace entry. Fields: {list(ws.keys())}"
        )

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
        """Additional: new-workspace --title fails; use --name to set title (cmux >= 0.63.1)."""
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


class TestNewWorkspaceFlags:
    """cmux >= 0.63.1: --name and --cwd flags on new-workspace."""

    def test_new_workspace_name_sets_title(self):
        """new-workspace --name sets the workspace title and returns OK workspace:N."""
        title = "test-name-flag"
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", title],
            capture_output=True,
            text=True,
            check=True,
        )
        stdout = result.stdout.strip()
        assert stdout.startswith("OK "), f"Expected 'OK workspace:N', got: {stdout}"

        workspace_ref = stdout.split()[-1]
        ref_pattern = re.compile(r"^workspace:\d+$")
        assert ref_pattern.match(workspace_ref), (
            f"Expected workspace:N, got: {workspace_ref}"
        )

        try:
            data = run_cmux_json("list-workspaces")
            titles = {ws["ref"]: ws["title"] for ws in data["workspaces"]}
            assert titles.get(workspace_ref) == title, (
                f"Expected title '{title}' for {workspace_ref}, "
                f"got '{titles.get(workspace_ref)}'"
            )
        finally:
            subprocess.run(
                ["cmux", "close-workspace", "--workspace", workspace_ref],
                capture_output=True,
                check=False,
            )

    def test_new_workspace_description_sets_description(self):
        """v0.63.0: new-workspace --description sets workspace description (#2475)."""
        title = "test-desc-flag"
        desc = "A test description"
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", title, "--description", desc],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"--description flag not supported: {result.stderr.strip()}")

        workspace_ref = result.stdout.strip().split()[-1]

        try:
            data = run_cmux_json("list-workspaces")
            for ws in data["workspaces"]:
                if ws["ref"] == workspace_ref:
                    assert ws.get("description") == desc, (
                        f"Expected description '{desc}', got '{ws.get('description')}'"
                    )
                    break
            else:
                pytest.fail(f"Workspace {workspace_ref} not found in list-workspaces")
        finally:
            subprocess.run(
                ["cmux", "close-workspace", "--workspace", workspace_ref],
                capture_output=True,
                check=False,
            )

    def test_new_workspace_cwd_sets_directory(self):
        """new-workspace --cwd /tmp sets the initial working directory."""
        title = "test-cwd"
        result = subprocess.run(
            ["cmux", "new-workspace", "--name", title, "--cwd", "/tmp"],
            capture_output=True,
            text=True,
            check=True,
        )
        workspace_ref = result.stdout.strip().split()[-1]

        try:
            # sidebar-state outputs key=value pairs, not JSON
            sidebar = run_cmux("sidebar-state", "--workspace", workspace_ref)
            kv = dict(
                line.split("=", 1)
                for line in sidebar.stdout.strip().splitlines()
                if "=" in line
            )
            cwd = kv.get("cwd", "")
            # /tmp may resolve to /private/tmp on macOS
            assert cwd in ("/tmp", "/private/tmp"), (
                f"Expected CWD '/tmp' (or '/private/tmp'), got: {cwd}"
            )
        finally:
            subprocess.run(
                ["cmux", "close-workspace", "--workspace", workspace_ref],
                capture_output=True,
                check=False,
            )
