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


@pytest.mark.tmux_only
def test_remote_project_is_cloned_on_first_use_and_answers_list_and_path(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """A project registered on a loopback remote is cloned on first use;
    list and path run on the remote and keep their output and exit codes."""
    env = mux_server
    root = env.tmp_path / "remote-root"
    root.mkdir()
    origin = env.tmp_path / "origin"
    origin.mkdir()
    env.run_command(["git", "init", "-q", "-b", "main"], cwd=origin)
    env.run_command(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
        + ["--allow-empty", "-m", "initial"],
        cwd=origin,
    )

    def wm(command, expect_fail=False):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=30.0,
        )

    wm(f"remote add lb --transport loopback --target {root}")
    added = wm(f"project add api --remote lb --repo {origin} --path code/api")
    assert "Added project api on remote lb" in added.stdout
    duplicate = wm(f"project add api --remote lb --repo {origin}", expect_fail=True)
    assert "project api already exists" in duplicate.stderr

    clone = root / "home" / "code" / "api"
    daemon_pid = None
    try:
        listing = wm("list --project api")
        assert clone.joinpath(".git").is_dir()
        assert "Cloning" in listing.stderr
        header, main_row = listing.stdout.splitlines()[:2]
        assert header.split()[:2] == ["PROJECT", "BRANCH"]
        assert main_row.split()[:2] == ["api@lb", "main"]

        # The command session ran over the workstation daemon's channel.
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        served = subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "remote serve"],
            capture_output=True,
            text=True,
        ).stdout.split()
        assert len(served) == 1

        worktree = root / "home" / "code" / "api__worktrees" / "feature"
        env.run_command(
            ["git", "worktree", "add", "-q", "-b", "feature", str(worktree)], cwd=clone
        )
        assert wm("path --project api feature").stdout.strip() == str(worktree)
        assert json.loads(wm("path --project api feature --json").stdout) == {
            "remote": "lb",
            "path": str(worktree),
        }

        missing = wm("path --project api nope", expect_fail=True)
        assert missing.exit_code == 1
        assert "Worktree 'nope' not found" in missing.stderr

        entries = json.loads(wm("list --project api --json").stdout)
        assert {(e["remote"], e["project"], e["branch"]) for e in entries} == {
            ("lb", "api", "main"),
            ("lb", "api", "feature"),
        }

        rows = {
            line.split()[1]: line.split()
            for line in wm("list --project api").stdout.splitlines()[1:]
        }
        assert rows["feature"][-1] == str(worktree)

        everything = wm("list --all").stdout
        assert any(
            line.split()[:2] == ["api@lb", "feature"]
            and line.split()[-1] == str(worktree)
            for line in everything.splitlines()
        ), everything

        # Local projects are unchanged: no PROJECT column, no remote field.
        local = wm("list")
        assert local.stdout.splitlines()[0].split()[0] == "BRANCH"
        assert "remote" not in json.loads(wm("list --json").stdout)[0]
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)
