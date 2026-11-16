"""`workmux add` and `remove` with the docker_sbx sandbox backend, against a fake `sbx`."""

import json
import os
import platform
from pathlib import Path
from typing import Any

import pytest
import yaml

from ..conftest import (
    MuxEnvironment,
    WorkmuxCommandResult,
    get_window_name,
    run_workmux_command,
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


def write_docker_sbx_config(env: MuxEnvironment, **sandbox: Any) -> None:
    """Global config turning on docker_sbx, with an image that's never pulled,
    and any other `sandbox` settings."""
    config = {
        "sandbox": {
            "enabled": True,
            "backend": "docker_sbx",
            "image": "workmux-test/agent:latest",
            **sandbox,
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


def test_add_merges_status_hooks_into_the_config_each_kit_wrote(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    fake_sbx.kit_files = {
        ".claude/settings.json": json.dumps({"model": "opus"}),
        ".codex/config.toml": 'model_provider = "sandboxd"\n',
    }

    add_claude_and_codex(
        mux_server, workmux_exe_path, mux_repo_path, "feature-sbx-hooks"
    )

    claude, codex = created_sandboxes(fake_sbx)
    settings = json.loads(fake_sbx.guest_file(claude, ".claude/settings.json") or "")
    assert settings["model"] == "opus"
    stop = [
        hook["command"]
        for group in settings["hooks"]["Stop"]
        for hook in group["hooks"]
    ]
    assert "workmux set-window-status done" in stop

    config = fake_sbx.guest_file(codex, ".codex/config.toml") or ""
    assert 'model_provider = "sandboxd"' in config
    assert "hooks = true" in config
    # The codex kit wrote no hooks.json; workmux's is created.
    assert "workmux set-window-status" in (
        fake_sbx.guest_file(codex, ".codex/hooks.json") or ""
    )


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


def network_rule(scope: str, decision: str, resources: list[str]) -> dict[str, Any]:
    """A network rule as `sbx policy ls --json` lists it."""
    return {
        "scope": scope,
        "decision": decision,
        "resource_type": "network",
        "resources": resources,
        "provenance": {"created_via": "default"},
    }


# sbx's `deny-all` global policy: everything is denied.
DENY_ALL = [network_rule("global", "deny", ["**"])]

# A global policy wider than `deny-all`, as `balanced` is.
WIDER = [
    network_rule("global", "allow", ["github.com:443", "**.npmjs.org:443"])
] + DENY_ALL

# `policy: deny` with nothing reaching a private address sbx would let through.
DENY = {
    "policy": "deny",
    "allowed_domains": [{"host": "api.anthropic.com", "allow_private_ips": True}],
}


def acknowledgements(env: MuxEnvironment) -> list[Path]:
    """The acceptances workmux recorded, one per sandbox."""
    directory = (
        env.home_path
        / ".local/state/workmux/docker-sbx-acknowledgements/wider-network-policy-v1"
    )
    return sorted(directory.iterdir()) if directory.exists() else []


def add_claude_without_a_terminal(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: str,
    expect_fail: bool = False,
) -> WorkmuxCommandResult:
    """`workmux add` with a Claude pane, stdin piped so nobody can be asked."""
    write_workmux_config(repo_path, panes=[{"command": "claude"}])
    return run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {branch_name}",
        expect_fail=expect_fail,
        stdin_input="",
    )


def test_deny_under_a_wider_global_policy_fails_without_a_terminal(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    fake_sbx.global_rules = WIDER
    write_docker_sbx_config(mux_server, network=DENY)

    result = add_claude_without_a_terminal(
        mux_server, workmux_exe_path, mux_repo_path, "feature-sbx-wider", True
    )

    assert "warning: sandbox.network.policy is deny, but sbx's global" in result.stderr
    for expected in [
        "sandbox.docker_sbx.accept_wider_network_policy: true",
        "sbx policy init deny-all",
    ]:
        assert expected in result.stderr, result.stderr
    assert fake_sbx.verb_calls("create") == []
    assert fake_sbx.sandboxes == []
    assert acknowledgements(mux_server) == []


def test_kit_hosts_found_at_create_fail_without_a_terminal_and_remove_the_sandbox(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    fake_sbx.global_rules = DENY_ALL
    fake_sbx.kit_resources = ["api.anthropic.com:443", "downloads.claude.ai:443"]
    write_docker_sbx_config(mux_server, network=DENY)

    result = add_claude_without_a_terminal(
        mux_server, workmux_exe_path, mux_repo_path, "feature-sbx-kit-hosts", True
    )

    assert "also allow downloads.claude.ai:443, which" in result.stderr
    assert "accept_wider_network_policy" in result.stderr, result.stderr
    created = created_sandboxes(fake_sbx)
    assert len(created) == 1
    assert [call.argv for call in fake_sbx.verb_calls("rm")] == [
        ["rm", "-f", created[0]]
    ]
    assert fake_sbx.sandboxes == []
    assert acknowledgements(mux_server) == []


def test_the_setting_accepts_a_wider_policy_and_records_it_until_remove(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    branch_name = "feature-sbx-accepted"
    fake_sbx.global_rules = WIDER
    write_docker_sbx_config(
        mux_server,
        network=DENY,
        docker_sbx={"accept_wider_network_policy": True},
    )

    result = add_claude_without_a_terminal(
        mux_server, workmux_exe_path, mux_repo_path, branch_name
    )

    assert "warning: sandbox.network.policy is deny" in result.stderr
    created = created_sandboxes(fake_sbx)
    assert len(created) == 1
    [acknowledgement] = acknowledgements(mux_server)
    assert acknowledgement.name == created[0]
    assert acknowledgement.read_text() == "accepted-v1\n"
    assert acknowledgement.stat().st_mode & 0o777 == 0o600

    run_workmux_remove(
        mux_server, workmux_exe_path, mux_repo_path, branch_name, force=True
    )

    assert fake_sbx.sandboxes == []
    assert acknowledgements(mux_server) == []


def test_sandbox_prune_removes_the_acceptance(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    fake_sbx.global_rules = WIDER
    write_docker_sbx_config(
        mux_server,
        network=DENY,
        docker_sbx={"accept_wider_network_policy": True},
    )
    add_claude_without_a_terminal(
        mux_server, workmux_exe_path, mux_repo_path, "feature-sbx-prune"
    )
    assert len(acknowledgements(mux_server)) == 1

    run_workmux_command(
        mux_server, workmux_exe_path, mux_repo_path, "sandbox prune --force"
    )

    assert fake_sbx.sandboxes == []
    assert acknowledgements(mux_server) == []


def test_deny_all_with_the_kit_hosts_covered_never_asks(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    fake_sbx: FakeSbx,
):
    fake_sbx.global_rules = DENY_ALL
    fake_sbx.kit_resources = ["api.anthropic.com:443"]
    write_docker_sbx_config(mux_server, network=DENY)

    result = add_claude_without_a_terminal(
        mux_server, workmux_exe_path, mux_repo_path, "feature-sbx-deny-all"
    )

    assert "warning:" not in result.stderr, result.stderr
    assert len(created_sandboxes(fake_sbx)) == 1
    assert fake_sbx.verb_calls("rm") == []
    assert acknowledgements(mux_server) == []
