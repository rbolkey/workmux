"""Shared fixtures and skip logic for cmux learning tests.

These tests must be run from inside a cmux terminal where CMUX_SOCKET_PATH
is set and the cmux binary is available.
"""

import os
import shutil
import subprocess

import pytest

from .helpers import get_workspace_refs, run_cmux_json


def pytest_collection_modifyitems(config, items):
    """Skip all learning tests if not running inside a cmux terminal."""
    if not shutil.which("cmux") or not os.environ.get("CMUX_SOCKET_PATH"):
        skip = pytest.mark.skip(
            reason="Learning tests require a cmux terminal (CMUX_SOCKET_PATH not set)"
        )
        for item in items:
            item.add_marker(skip)


def _create_workspace():
    """Create a new workspace and return its refs dict."""
    before = get_workspace_refs()
    result = subprocess.run(
        ["cmux", "new-workspace"], capture_output=True, text=True, check=True
    )
    uuid = result.stdout.strip().split()[-1]  # "OK <UUID>" -> UUID

    after = get_workspace_refs()
    new_refs = after - before
    if len(new_refs) != 1:
        raise RuntimeError(
            f"Expected 1 new workspace after creation, got {len(new_refs)}: {new_refs}"
        )
    workspace_ref = new_refs.pop()

    surfaces = run_cmux_json("list-pane-surfaces", "--workspace", workspace_ref)
    surface_ref = surfaces["surfaces"][0]["ref"]

    return {"workspace_ref": workspace_ref, "surface_ref": surface_ref, "uuid": uuid}


@pytest.fixture
def cmux_workspace():
    """Create a temporary cmux workspace, yield refs, close on teardown."""
    ws = _create_workspace()
    yield ws
    subprocess.run(
        ["cmux", "close-workspace", "--workspace", ws["workspace_ref"]],
        capture_output=True,
        check=False,
    )


@pytest.fixture
def cmux_split(cmux_workspace):
    """Create a split in the test workspace, yield both surface refs."""
    ws = cmux_workspace
    result = subprocess.run(
        [
            "cmux",
            "new-split",
            "right",
            "--workspace",
            ws["workspace_ref"],
            "--surface",
            ws["surface_ref"],
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    # Parse "OK surface:N workspace:N"
    parts = result.stdout.strip().split()
    new_surface_ref = parts[1]  # surface:N

    yield {**ws, "second_surface_ref": new_surface_ref}


@pytest.fixture
def cmux_two_workspaces(cmux_workspace):
    """Create a second workspace for cross-workspace targeting tests."""
    second = _create_workspace()
    yield {"primary": cmux_workspace, "secondary": second}
    subprocess.run(
        ["cmux", "close-workspace", "--workspace", second["workspace_ref"]],
        capture_output=True,
        check=False,
    )
