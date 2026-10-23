import json
import os
import shutil
import signal
import subprocess
import tempfile
from pathlib import Path

import pytest

from .conftest import MuxEnvironment, run_workmux_command


@pytest.fixture
def short_tmp():
    """A short TMPDIR: the workstation daemon socket lives there, and
    pytest's own temp path is too long for a Unix socket address."""
    path = tempfile.mkdtemp(prefix="wm-", dir="/tmp")
    yield {"TMPDIR": path}
    shutil.rmtree(path, ignore_errors=True)


def remote(env, exe, repo, command, tmp_env, expect_fail=False):
    return run_workmux_command(
        env,
        exe,
        repo,
        f"remote {command}",
        pre_run_env=tmp_env,
        expect_fail=expect_fail,
        timeout=30.0,
    )


@pytest.mark.tmux_only
def test_remote_add_list_status_rm_against_a_loopback_remote(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """A loopback remote is reachable through the workstation daemon's control
    channel; an unreachable one shows the transport's error."""
    env = mux_server
    root = env.tmp_path / "remote-root"
    root.mkdir()
    missing = env.tmp_path / "no-such-root"
    version = env.run_command([str(workmux_exe_path), "--version"]).stdout.split()[-1]

    remote(
        env,
        workmux_exe_path,
        repo_path,
        f"add lb --transport loopback --target {root}",
        short_tmp,
    )
    remote(
        env,
        workmux_exe_path,
        repo_path,
        f"add gone --transport loopback --target {missing}",
        short_tmp,
    )
    daemon_pid = None
    try:
        listing = remote(env, workmux_exe_path, repo_path, "list", short_tmp).stdout
        rows = {
            line.split()[0]: line for line in listing.splitlines()[1:] if line.strip()
        }
        assert rows["lb"].rstrip().endswith("reachable")
        assert f"unreachable: loopback root {missing} does not exist" in rows["gone"]

        status = remote(env, workmux_exe_path, repo_path, "status lb", short_tmp).stdout
        assert "status:   reachable" in status
        assert f"workmux:  {version}" in status
        assert "protocol: 1" in status

        # The workstation daemon holds the channel: remote workmux runs as
        # its child, rooted in the loopback root.
        ping = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)
        daemon_pid = ping["pid"]
        children = subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "remote serve"],
            capture_output=True,
            text=True,
        ).stdout.split()
        assert len(children) == 1

        failed = remote(
            env, workmux_exe_path, repo_path, "status gone", short_tmp, expect_fail=True
        )
        assert "status:   unreachable" in failed.stdout
        assert f"loopback root {missing} does not exist" in failed.stdout

        remote(env, workmux_exe_path, repo_path, "rm gone", short_tmp)
        listing = remote(env, workmux_exe_path, repo_path, "list", short_tmp).stdout
        assert "gone" not in listing
        assert "lb" in listing
        config = (env.home_path / ".config" / "workmux" / "config.yaml").read_text()
        assert "gone" not in config
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


def remote_daemon_ping(env, exe, repo, tmp_env):
    result = run_workmux_command(
        env, exe, repo, "_workstation-daemon-ping", pre_run_env=tmp_env
    )
    return json.loads(result.stdout)
