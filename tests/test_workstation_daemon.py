import json
import os
import shutil
import signal
import tempfile
from pathlib import Path

import pytest

from .conftest import MuxEnvironment, run_workmux_command


@pytest.mark.tmux_only
def test_cli_starts_the_workstation_daemon_on_demand_and_pings_it(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """A CLI request starts the workstation daemon once and gets its version."""
    env = mux_server
    # The daemon socket lives in TMPDIR; pytest's own temp path is too long
    # for a Unix socket address.
    short_tmp = tempfile.mkdtemp(prefix="wm-", dir="/tmp")
    tmp_env = {"TMPDIR": short_tmp}

    first = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "_workstation-daemon-ping",
        pre_run_env=tmp_env,
    )
    first_ping = json.loads(first.stdout)
    try:
        second = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "_workstation-daemon-ping",
            pre_run_env=tmp_env,
        )
        second_ping = json.loads(second.stdout)
        version = env.run_command([str(workmux_exe_path), "--version"]).stdout
        assert first_ping["version"] in version
        assert first_ping["protocol"] == 1
        assert second_ping["pid"] == first_ping["pid"]
        recorded_pid = env.mux_command(
            ["show-option", "-gqv", "@workmux_sidebar_daemon_pid"]
        ).stdout.strip()
        assert recorded_pid == str(first_ping["pid"])
    finally:
        os.kill(first_ping["pid"], signal.SIGTERM)
        shutil.rmtree(short_tmp, ignore_errors=True)
