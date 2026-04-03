"""Shared fixtures and skip logic for cmux learning tests.

These tests must be run from inside a cmux terminal where CMUX_SOCKET_PATH
is set and the cmux binary is available.
"""

import os
import shutil
import subprocess
import uuid as uuid_mod

import pytest

from .helpers import get_initial_surface_ref, parse_workspace_ref, run_cmux_json


def pytest_collection_modifyitems(config, items):
    """Skip all learning tests if not running inside a cmux terminal."""
    if not shutil.which("cmux") or not os.environ.get("CMUX_SOCKET_PATH"):
        skip = pytest.mark.skip(
            reason="Learning tests require a cmux terminal (CMUX_SOCKET_PATH not set)"
        )
        for item in items:
            item.add_marker(skip)


def _create_workspace(name=None):
    """Create a new workspace and return its refs dict.

    Uses ``cmux new-workspace --name <name>`` and parses ``OK workspace:N``
    from stdout to obtain the workspace ref directly (cmux >= 0.63.1).
    """
    if name is None:
        name = f"wm-test-{uuid_mod.uuid4().hex[:8]}"

    result = subprocess.run(
        ["cmux", "new-workspace", "--name", name],
        capture_output=True,
        text=True,
        check=True,
    )
    workspace_ref = parse_workspace_ref(result.stdout)
    surface_ref = get_initial_surface_ref(workspace_ref)

    return {"workspace_ref": workspace_ref, "surface_ref": surface_ref}


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
