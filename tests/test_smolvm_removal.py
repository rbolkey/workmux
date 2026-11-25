"""Removing a worktree deletes its smol machines, after the pre_remove hooks."""

import hashlib
import json
import subprocess
from pathlib import Path

import yaml

from .conftest import (
    MuxEnvironment,
    create_commit,
    get_worktree_path,
    run_workmux_add,
    run_workmux_merge,
    run_workmux_remove,
)

FAKE_SMOLVM = """#!/bin/sh
echo "smolvm $*" >> "{log}"
case "$*" in
    *"--version"*) echo "smolvm 1.36.1" ;;
    *"machine ls --json"*) cat "{listing}" ;;
esac
exit 0
"""


def worktree_label(path: Path) -> str:
    """The `wm-worktree` label workmux gives a worktree's machines."""
    return hashlib.sha256(str(path.resolve()).encode()).hexdigest()[:16]


def setup_smolvm(env: MuxEnvironment, repo_path: Path, log: Path, listing: Path):
    """Select the smolvm backend (sandboxing off, so panes run on the host),
    log pre_remove hooks and smolvm calls to one file, and commit the config."""
    config = {
        "nerdfont": False,
        "sandbox": {"backend": "smolvm"},
        "pre_remove": [f'echo "hook" >> "{log}"'],
    }
    (repo_path / ".workmux.yaml").write_text(yaml.dump(config))
    subprocess.run(
        ["git", "add", ".workmux.yaml"], cwd=repo_path, check=True, env=env.env
    )
    subprocess.run(
        ["git", "commit", "-m", "Add workmux config"],
        cwd=repo_path,
        check=True,
        env=env.env,
    )
    listing.write_text("[]")
    env.install_script(
        env.fake_bin_dir / "smolvm", FAKE_SMOLVM.format(log=log, listing=listing)
    )


def list_machines(listing: Path, worktree: Path):
    """smolvm reports one machine for the worktree and one for another."""
    listing.write_text(
        json.dumps(
            [
                {
                    "name": "wm-mine",
                    "state": "running",
                    "labels": {"workmux": "1", "wm-worktree": worktree_label(worktree)},
                },
                {
                    "name": "wm-other",
                    "state": "running",
                    "labels": {"workmux": "1", "wm-worktree": "0000000000000000"},
                },
            ]
        )
    )


def removal_calls(log: Path) -> list[str]:
    return [
        line
        for line in log.read_text().splitlines()
        if line == "hook" or "machine" in line
    ]


def test_remove_deletes_the_worktrees_machines_after_pre_remove_hooks(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    log, listing = env.tmp_path / "calls.log", env.tmp_path / "ls.json"
    setup_smolvm(env, repo_path, log, listing)

    run_workmux_add(env, workmux_exe_path, repo_path, "feature-smol")
    worktree = get_worktree_path(repo_path, "feature-smol")
    list_machines(listing, worktree)
    run_workmux_remove(env, workmux_exe_path, repo_path, "feature-smol", force=True)

    calls = removal_calls(log)
    assert calls[0] == "hook", calls
    assert calls[1].endswith("machine ls --json"), calls
    assert calls[2].endswith("machine delete --name wm-mine -f"), calls
    assert len(calls) == 3, calls
    assert not worktree.exists()


def test_merge_deletes_the_worktrees_machines_after_pre_remove_hooks(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    log, listing = env.tmp_path / "calls.log", env.tmp_path / "ls.json"
    setup_smolvm(env, repo_path, log, listing)

    run_workmux_add(env, workmux_exe_path, repo_path, "feature-smol-merge")
    worktree = get_worktree_path(repo_path, "feature-smol-merge")
    create_commit(env, worktree, "feat: smol")
    list_machines(listing, worktree)
    run_workmux_merge(env, workmux_exe_path, repo_path, "feature-smol-merge")

    calls = removal_calls(log)
    assert calls[0] == "hook", calls
    assert calls[-1].endswith("machine delete --name wm-mine -f"), calls
    assert not any("wm-other" in call for call in calls), calls
