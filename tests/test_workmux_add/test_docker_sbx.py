"""`workmux add` and `remove` with the docker_sbx sandbox backend, against a fake `sbx`."""

import os
import platform
from pathlib import Path

import pytest
import yaml

from ..conftest import (
    MuxEnvironment,
    get_window_name,
    run_workmux_remove,
    write_workmux_config,
)
from ..support.fake_sbx import FakeSbx
from .conftest import add_branch_and_get_worktree


def _sbx_supports_host() -> bool:
    """Whether workmux lets sbx run here, as its host check decides."""
    if platform.system() == "Darwin":
        return platform.machine() == "arm64"
    return platform.system() == "Linux" and os.path.exists("/dev/kvm")


pytestmark = [
    pytest.mark.tmux_only,
    pytest.mark.skipif(
        not _sbx_supports_host(),
        reason="workmux refuses docker_sbx off Apple silicon, or Linux without /dev/kvm",
    ),
]


def write_docker_sbx_config(env: MuxEnvironment) -> None:
    """Global config turning on docker_sbx, with an image that's never pulled."""
    config = {
        "sandbox": {
            "enabled": True,
            "backend": "docker_sbx",
            "image": "workmux-test/agent:latest",
        }
    }
    config_dir = env.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.yaml").write_text(yaml.dump(config))


def created_sandboxes(fake_sbx: FakeSbx) -> list[str]:
    return [
        call.argv[call.argv.index("--name") + 1]
        for call in fake_sbx.verb_calls("create")
    ]


def add_claude_and_codex(
    env: MuxEnvironment, workmux_exe_path: Path, repo_path: Path, branch_name: str
) -> None:
    write_docker_sbx_config(env)
    write_workmux_config(
        repo_path,
        panes=[{"command": "claude"}, {"command": "codex", "split": "horizontal"}],
    )
    add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)


def test_add_creates_each_agents_sandbox_before_the_window_opens(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    branch_name = "feature-sbx-agents"

    add_claude_and_codex(mux_server, workmux_exe_path, mux_repo_path, branch_name)

    created = created_sandboxes(fake_sbx)
    assert len(created) == 2, created
    assert created[0].endswith("-claude") and created[1].endswith("-codex"), created
    for call in fake_sbx.verb_calls("create"):
        assert call.windows is not None
        assert get_window_name(branch_name) not in call.windows, call.windows


def test_remove_deletes_each_agents_sandbox(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    branch_name = "feature-sbx-remove"
    add_claude_and_codex(mux_server, workmux_exe_path, mux_repo_path, branch_name)
    created = created_sandboxes(fake_sbx)

    run_workmux_remove(
        mux_server, workmux_exe_path, mux_repo_path, branch_name, force=True
    )

    removed = [call.argv[-1] for call in fake_sbx.verb_calls("rm")]
    assert sorted(removed) == sorted(created)
    assert fake_sbx.sandboxes == []
