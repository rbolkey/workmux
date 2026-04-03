"""Shared helpers for cmux learning tests and CmuxEnvironment."""

import json
import re
import subprocess
import time


def run_cmux(*args):
    """Run cmux <args> and return the CompletedProcess."""
    return subprocess.run(["cmux", *args], capture_output=True, text=True, check=True)


def run_cmux_json(*args):
    """Run cmux --json <args> and return parsed JSON."""
    result = run_cmux("--json", *args)
    return json.loads(result.stdout)


def get_workspace_refs():
    """Return set of current workspace refs."""
    data = run_cmux_json("list-workspaces")
    return {ws["ref"] for ws in data["workspaces"]}


def parse_workspace_ref(stdout: str) -> str:
    """Extract workspace ref from ``cmux new-workspace`` output.

    Parses ``OK workspace:N`` format (cmux >= 0.63.1).
    Raises RuntimeError if the output doesn't contain a workspace ref.
    """
    match = re.search(r"workspace:\d+", stdout)
    if not match:
        raise RuntimeError(
            f"Failed to parse workspace ref from new-workspace output: {stdout!r}"
        )
    return match.group(0)


def get_initial_surface_ref(workspace_ref: str) -> str:
    """Return the first surface ref in a workspace."""
    surfaces = run_cmux_json("list-pane-surfaces", "--workspace", workspace_ref)
    return surfaces["surfaces"][0]["ref"]


def wait_for_screen_content(
    workspace_ref, surface_ref, expected, timeout=3.0, interval=0.1
):
    """Poll read-screen until expected string appears or timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        result = subprocess.run(
            [
                "cmux",
                "read-screen",
                "--workspace",
                workspace_ref,
                "--surface",
                surface_ref,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if expected in result.stdout:
            return result.stdout
        time.sleep(interval)
    raise AssertionError(f"'{expected}' not found in screen output after {timeout}s")
