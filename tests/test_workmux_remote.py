import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import pytest

from .conftest import MuxEnvironment, TmuxEnvironment
from .conftest import run_workmux_command as run_in_test_session


def run_workmux_command(env, *args, **kwargs):
    """Run workmux in the test's own pane. A remote project's new window
    takes focus in the session, as a local project's does, and the harness
    types into the session's active window."""
    if isinstance(env, TmuxEnvironment) and env.runner_pane_id:
        env.tmux(["select-window", "-t", env.runner_pane_id])
    return run_in_test_session(env, *args, **kwargs)


@pytest.fixture
def short_tmp():
    """A short TMPDIR: the workstation daemon socket lives there, and
    pytest's own temp path is too long for a Unix socket address."""
    path = tempfile.mkdtemp(prefix="wm-", dir="/tmp")
    yield {"TMPDIR": path}
    shutil.rmtree(path, ignore_errors=True)


@pytest.fixture
def remote_root():
    """A short loopback root: remote panes' dtach sockets live under its
    HOME, and a Unix socket path holds only about 104 bytes."""
    path = Path(tempfile.mkdtemp(prefix="wmr", dir="/tmp"))
    (path / "home").mkdir()
    yield path
    # End the remote panes left running: their holders, then their shells.
    subprocess.run(["pkill", "-f", f"dtach -n {path}/"], check=False)
    shutil.rmtree(path, ignore_errors=True)


@pytest.fixture
def outage(mux_server: MuxEnvironment, short_tmp: dict):
    """A file that, while it exists, makes every loopback remote unreachable,
    standing in for a network outage. The test-only
    WORKMUX_TEST_LOOPBACK_OFFLINE names it to workmux run from the test and
    from tmux panes."""
    marker = Path(short_tmp["TMPDIR"]) / "outage"
    short_tmp["WORKMUX_TEST_LOOPBACK_OFFLINE"] = str(marker)
    assert isinstance(mux_server, TmuxEnvironment)
    mux_server.set_session_env("WORKMUX_TEST_LOOPBACK_OFFLINE", str(marker))
    yield marker
    marker.unlink(missing_ok=True)


requires_dtach = pytest.mark.skipif(
    shutil.which("dtach") is None,
    reason=(
        "remote panes need the session holder dtach on PATH: use `devbox shell`, "
        "`nix shell nixpkgs#dtach`, or `apt-get install dtach`"
    ),
)


def sandbox(env, exe, repo, command, tmp_env, expect_fail=False):
    return run_workmux_command(
        env,
        exe,
        repo,
        f"sandbox {command}",
        pre_run_env=tmp_env,
        expect_fail=expect_fail,
        timeout=30.0,
    )


def clone_of(env, name: str) -> Path:
    """The workstation clone `project_in_sandbox` made for project `name`:
    commands run there target the project."""
    return env.tmp_path / "workstation" / name


def project_in_sandbox(
    env,
    exe,
    name: str,
    origin,
    tmp_env: dict,
    *,
    root: Path | None = None,
    remote: str = "lb",
    path: str | None = None,
    config: str = "",
) -> Path:
    """A workstation repository named `name` with `origin` as its origin,
    whose `.workmux.yaml` puts it in a sandbox (ADR 0004): on the hidden
    loopback backend rooted in `root`, as remote `remote` with its clone at
    `path` (default code/<name>), or with no `root` in an OpenShell
    sandbox. `config` adds lines to that file."""
    repo = env.tmp_path / "workstation" / name
    repo.mkdir(parents=True)
    env.run_command(["git", "init", "-q", "-b", "main"], cwd=repo)
    env.run_command(["git", "remote", "add", "origin", str(origin)], cwd=repo)
    if root is None:
        sandbox = "sandbox:\n  enabled: true\n  backend: openshell\n"
    else:
        sandbox = (
            "sandbox:\n  enabled: true\n  backend: loopback\n  loopback:\n"
            f"    root: {root}\n    remote: {remote}\n"
            f"    path: {path or f'code/{name}'}\n"
        )
    (repo / ".workmux.yaml").write_text("nerdfont: false\n" + sandbox + config)
    return repo


def project_tag(env, remote: str, name: str) -> str:
    """The tag on project `name`'s workstation windows and session: the
    project and the path of the clone `project_in_sandbox` made for it."""
    return f"{remote}:{name}:{os.path.realpath(clone_of(env, name))}"


def openshell_sandbox_name(clone: Path) -> str:
    """The OpenShell sandbox name workmux derives from a clone's path, in
    the format of a project's Lima VM name: `wm-<project>-<hash8>`, the
    project cut to fit OpenShell's 19 characters, the hash from sha256."""
    root = os.path.realpath(clone)
    digest = hashlib.sha256(root.encode()).hexdigest()[:8]
    project = re.sub(r"[^a-z0-9]+", "-", os.path.basename(root).lower()).strip("-")
    project = project[:7].rstrip("-")
    return f"wm-{project}-{digest}" if project else f"wm-{digest}"


@pytest.mark.tmux_only
def test_sandbox_status_against_a_loopback_remote(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """`sandbox status` in a repository whose config puts it on a loopback
    remote shows that remote reachable through the workstation daemon's
    control channel, and an unreachable one with the transport's error. A
    repository with no sandbox runs on the workstation."""
    env = mux_server
    root = env.tmp_path / "remote-root"
    root.mkdir()
    missing = env.tmp_path / "no-such-root"
    origin = make_origin(env, env.tmp_path / "origin", "panes: []\n")
    version = env.run_command([str(workmux_exe_path), "--version"]).stdout.split()[-1]

    here = sandbox(env, workmux_exe_path, repo_path, "status", short_tmp).stdout
    assert "This repository runs on the workstation" in here
    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    project_in_sandbox(
        env,
        workmux_exe_path,
        "web",
        origin,
        short_tmp,
        root=missing,
        remote="gone",
    )
    daemon_pid = None
    try:
        status = sandbox(
            env, workmux_exe_path, clone_of(env, "api"), "status", short_tmp
        ).stdout
        assert status.startswith("lb (loopback ")
        assert "status:   reachable" in status
        assert f"workmux:  {version}" in status
        assert "protocol: 1" in status

        # A shell opens in the sandbox's clone (here, under the loopback
        # root), creating the clone on first use.
        shell = sandbox(
            env, workmux_exe_path, clone_of(env, "api"), "shell -- pwd", short_tmp
        )
        assert shell.stdout.strip().splitlines()[-1] == os.path.realpath(
            root / "home" / "code" / "api"
        )

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

        web = clone_of(env, "web")
        failed = sandbox(
            env, workmux_exe_path, web, "status", short_tmp, expect_fail=True
        )
        assert "status:   unreachable" in failed.stdout
        assert f"Loopback root {missing} does not exist" in failed.stdout

        refused = sandbox(
            env, workmux_exe_path, web, "rebuild", short_tmp, expect_fail=True
        )
        assert "only those are rebuilt" in refused.stderr
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@pytest.mark.tmux_only
def test_a_global_projects_registry_is_refused_naming_what_replaced_it(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """A config still holding the withdrawn `projects:` registry fails with a
    message naming the repository config that replaced it (ADR 0004)."""
    env = mux_server
    config = env.home_path / ".config" / "workmux" / "config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        "projects:\n  api:\n    git_url: https://example.com/api.git\n    openshell: {}\n"
    )

    failed = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "list",
        pre_run_env=short_tmp,
        expect_fail=True,
    )

    assert "projects: is no longer read" in failed.stderr
    assert "backend: openshell" in failed.stderr


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
    """A repository whose config puts it on a loopback remote is cloned
    there by the first command run in it, with no registration; list and
    path run on the remote and keep their output and exit codes."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
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

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=30.0,
        )

    workstation = env.tmp_path / "workstation" / "api"
    workstation.mkdir(parents=True)
    env.run_command(["git", "init", "-q", "-b", "main"], cwd=workstation)
    env.run_command(["git", "remote", "add", "origin", str(origin)], cwd=workstation)
    (workstation / ".workmux.yaml").write_text(
        "nerdfont: false\nsandbox:\n  enabled: true\n  backend: loopback\n  loopback:\n"
        f"    root: {root}\n    remote: lb\n    path: code/api\n"
    )

    clone = root / "home" / "code" / "api"
    daemon_pid = None
    try:
        listing = wm("list", cwd=workstation)
        assert clone.joinpath(".git").is_dir()
        assert "Cloning" in listing.stderr
        target, header, main_row = listing.stdout.splitlines()[:3]
        assert target == "Worktrees in remote lb (project api)"
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
        assert wm("path feature", cwd=clone_of(env, "api")).stdout.strip() == str(
            worktree
        )
        assert json.loads(
            wm("path feature --json", cwd=clone_of(env, "api")).stdout
        ) == {
            "remote": "lb",
            "path": str(worktree),
        }

        missing = wm("path nope", cwd=clone_of(env, "api"), expect_fail=True)
        assert missing.exit_code == 1
        assert "Worktree 'nope' not found" in missing.stderr

        entries = json.loads(wm("list --json", cwd=clone_of(env, "api")).stdout)
        assert {(e["remote"], e["project"], e["branch"]) for e in entries} == {
            ("lb", "api", "main"),
            ("lb", "api", "feature"),
        }

        # Run in the project's clone, list says once where the project runs.
        listing = wm("list", cwd=clone_of(env, "api")).stdout.splitlines()
        assert listing[0] == "Worktrees in remote lb (project api)"
        rows = {line.split()[1]: line.split() for line in listing[2:]}
        assert rows["feature"][-1] == str(worktree)

        # list --all covers the remote projects with open workstation
        # windows; nothing about a project is stored.
        assert "api@lb" not in wm("list --all").stdout
        env.tmux(["new-window", "-d", "-t", "test:", "-n", "api-window"])
        env.tmux(
            [
                "set-option",
                "-w",
                "-t",
                "=test:api-window",
                "@workmux_project_window",
                f"lb:api:{os.path.realpath(workstation)}",
            ]
        )
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


def make_origin(env, path: Path, workmux_yaml: str) -> Path:
    """A git repository the loopback remote clones, with its own
    `.workmux.yaml` (which only the remote reads)."""
    path.mkdir()
    env.run_command(["git", "init", "-q", "-b", "main"], cwd=path)
    (path / ".workmux.yaml").write_text(workmux_yaml)
    env.run_command(["git", "add", ".workmux.yaml"], cwd=path)
    env.run_command(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
        + ["-m", "initial"],
        cwd=path,
    )
    return path


def run_in_pane(env, target: str, script_body: str, out_dir: Path, timeout=30.0):
    """Run a shell snippet in the tmux pane `target`, returning (exit code,
    stdout, stderr)."""
    out_dir.mkdir(exist_ok=True)
    for name in ["out", "err", "code"]:
        (out_dir / name).unlink(missing_ok=True)
    script = out_dir / "run.sh"
    script.write_text(
        f"trap 'echo $? > {out_dir}/code' EXIT\n"
        f"export PATH={shlex.quote(env.env['PATH'])} HOME={shlex.quote(str(env.home_path))}\n"
        f"export WORKMUX_TEST=1\n"
        f"({script_body}) > {out_dir}/out 2> {out_dir}/err\n"
    )
    env.send_keys(target, f"/bin/sh {script}", enter=True)
    deadline = time.monotonic() + timeout
    while not (out_dir / "code").exists() or not (out_dir / "code").read_text():
        assert time.monotonic() < deadline, env.capture_pane(target)
        time.sleep(0.05)
    return (
        int((out_dir / "code").read_text()),
        (out_dir / "out").read_text(),
        (out_dir / "err").read_text(),
    )


def tmux_value(env, target: str, fmt: str) -> str:
    return env.tmux(["display-message", "-p", "-t", target, fmt]).stdout.strip()


def session_tag(env, session: str) -> str:
    return env.tmux(
        ["show-options", "-qv", "-t", session, "@workmux_project"], check=False
    ).stdout.strip()


def wait_for(path: Path, timeout=15.0) -> str:
    deadline = time.monotonic() + timeout
    while not path.exists() or not path.read_text():
        assert time.monotonic() < deadline, f"{path} never written"
        time.sleep(0.05)
    return path.read_text()


@requires_dtach
@pytest.mark.tmux_only
def test_remote_add_places_windows_as_a_local_project_does(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`add` on a remote project creates the worktree on the remote, runs its
    post-create hooks there with output streamed back, and places the window
    as for a local project: in the session the command was typed in, focused.
    `window_placement` and `mode` in the workstation's global config apply."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    # The remote terminal is an interactive login shell in an empty HOME.
    (root / "home" / ".zshrc").write_text("")
    workmux_yaml = (
        "post_create:\n"
        '  - echo "post-create ran in $PWD"\n'
        "panes:\n"
        "  - command: pwd -P > pane-ran.txt\n"
    )
    origin = make_origin(env, env.tmp_path / "origin", workmux_yaml)
    global_config = env.home_path / ".config" / "workmux" / "config.yaml"
    global_config.parent.mkdir(parents=True, exist_ok=True)

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    daemon_pid = None
    try:
        # With no placement config, the window goes in the session the
        # command was typed in, after the window it was typed in, and takes
        # focus.
        added = wm("add feature", cwd=clone_of(env, "api"))
        assert tmux_value(env, "=test:wm-feature", "#{window_active}") == "1"
        assert tmux_value(env, "=test:wm-feature", "#{session_name}") == "test"
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert "post-create ran in" in added.stdout + added.stderr
        worktree = root / "home" / "code" / "api__worktrees" / "feature"
        assert worktree.is_dir()
        assert session_tag(env, "test") == ""
        assert not any(
            line == "api"
            for line in env.tmux(["list-sessions", "-F", "#{session_name}"])
            .stdout.strip()
            .splitlines()
        )
        # The branch lives in the remote's clone, not on the workstation.
        for repo in [origin, repo_path]:
            branches = env.run_command(
                ["git", "branch", "--list", "feature"], cwd=repo
            ).stdout
            assert branches.strip() == ""
        # The pane runs on the remote, in the remote worktree.
        ran = wait_for(worktree / "pane-ran.txt")
        assert ran.strip() == str(worktree.resolve())

        # window_placement in the workstation's global config applies.
        global_config.write_text("window_placement: rightmost\n")
        wm("add other", cwd=clone_of(env, "api"))

        def index(window):
            return int(tmux_value(env, f"=test:{window}", "#{window_index}"))

        assert index("wm-other") > index("wm-feature")

        # So does mode: session, giving the worktree its own session.
        global_config.write_text("mode: session\n")
        wm("add solo", cwd=clone_of(env, "api"))
        assert session_tag(env, "wm-solo") == project_tag(env, "lb", "api")

        # Typed outside any repository in that session (in a pane of the
        # user's own), a command targets the tagged project.
        env.tmux(["new-window", "-d", "-t", "wm-solo:", "-n", "mine"])
        code, out, err = run_in_pane(
            env,
            "=wm-solo:mine",
            f"cd /tmp && TMPDIR={short_tmp['TMPDIR']} {workmux_exe_path} add tagged",
            env.tmp_path / "pane-run",
            timeout=60.0,
        )
        assert code == 0, err
        assert (root / "home" / "code" / "api__worktrees" / "tagged").is_dir()
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_a_failed_remote_add_leaves_no_worktree_and_can_be_retried(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """When a post-create command fails, `add` on a remote project removes the
    worktree and the branch it created on the remote, says so, and a retry
    succeeds once the command does."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    (root / "home" / ".zshrc").write_text("")
    workmux_yaml = 'post_create:\n  - test -f "$HOME/setup-works"\n'
    origin = make_origin(env, env.tmp_path / "origin", workmux_yaml)

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    daemon_pid = None
    try:
        failed = wm("add feature", cwd=clone_of(env, "api"), expect_fail=True)
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        output = failed.stdout + failed.stderr
        assert "post-create command" in output, output
        assert "removed the new worktree and branch 'feature'" in output, output
        clone = root / "home" / "code" / "api"
        worktree = root / "home" / "code" / "api__worktrees" / "feature"
        assert not worktree.exists()
        branches = env.run_command(
            ["git", "branch", "--list", "feature"], cwd=clone
        ).stdout
        assert branches.strip() == ""
        windows = env.tmux(["list-windows", "-a", "-F", "#{window_name}"]).stdout
        assert "wm-feature" not in windows.split()

        (root / "home" / "setup-works").write_text("")
        wm("add feature", cwd=clone_of(env, "api"))
        assert worktree.is_dir()
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


def git_in(env, repo: Path, *args: str) -> str:
    return env.run_command(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", *args], cwd=repo
    ).stdout.strip()


@requires_dtach
@pytest.mark.tmux_only
def test_remote_git_commands_act_on_the_remote_repository(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`set-base`, `rebase`, `sync-files`, `rename` and `init` run on the
    remote for a remote project, with local output and exit codes. Typed in a
    worktree's window, they act on that worktree, as they would locally;
    `rename` also renames the workstation window."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    (root / "home").mkdir(parents=True, exist_ok=True)
    (root / "home" / ".zshrc").write_text("")
    origin = make_origin(
        env, env.tmp_path / "origin", "files:\n  copy:\n    - local.env\n"
    )

    def wm(command, expect_fail=False, from_pane=None, cwd=None):
        tmp_env = dict(short_tmp)
        if from_pane is not None:
            tmp_env["TMUX_PANE"] = from_pane
        result = run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=tmp_env,
            expect_fail=expect_fail,
            timeout=60.0,
        )
        assert "not supported on a remote" not in result.stderr, result.stderr
        return result

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    clone = root / "home" / "code" / "api"
    worktrees = root / "home" / "code" / "api__worktrees"
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        window = tmux_value(env, "=test:wm-feature", "#{pane_id}")

        # set-base typed in the worktree's window sets that branch's base.
        git_in(env, clone, "branch", "develop")
        set_base = wm("set-base develop", cwd=clone_of(env, "api"), from_pane=window)
        assert set_base.stdout.strip() == "Set base branch for 'feature' to 'develop'"
        assert git_in(env, clone, "config", "branch.feature.workmux-base") == "develop"

        # rebase with no name rebases the window's worktree onto its base.
        git_in(env, clone, "checkout", "-q", "develop")
        git_in(env, clone, "commit", "-q", "--allow-empty", "-m", "on develop")
        git_in(env, clone, "checkout", "-q", "main")
        rebased = wm("rebase", cwd=clone_of(env, "api"), from_pane=window)
        assert "✓ Rebased 'feature' onto 'develop'" in rebased.stdout
        assert git_in(env, clone, "log", "-1", "--format=%s", "feature") == (
            "on develop"
        )

        # sync-files, routed by the window's tag alone, copies the main
        # worktree's files into the window's worktree.
        (clone / "local.env").write_text("SECRET=1\n")
        synced = wm("sync-files", from_pane=window)
        assert "✓ Synced files to 'feature'" in synced.stdout
        assert (worktrees / "feature" / "local.env").read_text() == "SECRET=1\n"
        outside = wm("sync-files", cwd=clone_of(env, "api"), expect_fail=True)
        assert outside.exit_code == 1
        assert "Current directory is the main worktree" in outside.stderr

        # rename moves the remote worktree, its branch and its window, though
        # typed in another window.
        renamed = wm("rename feature renamed --branch", cwd=clone_of(env, "api"))
        assert "✓ Renamed worktree 'feature' -> 'renamed'" in renamed.stdout
        assert "✓ Renamed 1 tmux target" in renamed.stdout
        assert (worktrees / "renamed").is_dir()
        assert not (worktrees / "feature").exists()
        assert git_in(env, clone, "branch", "--list", "renamed") != ""
        assert git_in(env, clone, "branch", "--list", "feature") == ""
        assert tmux_value(env, window, "#{session_name}:#{window_name}") == (
            "test:wm-renamed"
        )
        # The window still leads to its worktree after the rename.
        again = wm("set-base main", cwd=clone_of(env, "api"), from_pane=window)
        assert again.stdout.strip() == "Set base branch for 'renamed' to 'main'"

        # init writes .workmux.yaml into the remote repository, refusing to
        # overwrite one exactly as locally.
        exists = wm("init", cwd=clone_of(env, "api"), expect_fail=True)
        assert exists.exit_code == 1
        assert ".workmux.yaml already exists" in exists.stderr
        (clone / ".workmux.yaml").unlink()
        created = wm("init", cwd=clone_of(env, "api"))
        assert "✓ Created .workmux.yaml" in created.stdout
        assert "workmux project configuration" in (clone / ".workmux.yaml").read_text()
        assert not (repo_path / ".workmux.yaml").exists()
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_remote_remove_cleans_up_the_remote_worktree_branch_and_window(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`close` on a remote project closes its window and keeps the worktree;
    `remove` removes the worktree and its branch in the remote's clone and
    the window on the workstation. The unmerged-branch confirmation is
    answered in the invoking terminal."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    (root / "home" / ".zshrc").write_text("")
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    clone = root / "home" / "code" / "api"
    worktrees = root / "home" / "code" / "api__worktrees"

    def wm(command, expect_fail=False, stdin_input=None, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            stdin_input=stdin_input,
            timeout=60.0,
        )

    def branches():
        return env.run_command(
            ["git", "branch", "--format=%(refname:short)"], cwd=clone
        ).stdout.split()

    def windows():
        return env.tmux(["list-windows", "-a", "-F", "#{window_name}"]).stdout.split()

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        assert "wm-feature" in windows()

        # `close` closes the window, ending its remote panes, and keeps the
        # worktree.
        holders = f"dtach -n {root}/home/.local/state/workmux/panes/api/feature/"
        assert subprocess.run(["pgrep", "-f", holders]).returncode == 0
        wm("close feature", cwd=clone_of(env, "api"))
        assert "wm-feature" not in windows()
        assert (worktrees / "feature").is_dir()
        wait_until(
            lambda: subprocess.run(["pgrep", "-f", holders]).returncode == 1,
            "the remote panes to end",
        )

        wm("remove feature", cwd=clone_of(env, "api"))
        assert not (worktrees / "feature").exists()
        assert "feature" not in branches()
        assert "wm-feature" not in windows()

        # An unmerged branch asks first; the answer comes from this terminal.
        wm("add risky", cwd=clone_of(env, "api"))
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        env.run_command(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
            + ["--allow-empty", "-m", "unmerged work"],
            cwd=worktrees / "risky",
        )
        declined = wm("remove risky", cwd=clone_of(env, "api"), stdin_input="n\n")
        assert "risky (base: main)" in declined.stdout
        assert "Are you sure you want to continue? [y/N]" in declined.stdout
        assert "Aborted." in declined.stdout
        assert (worktrees / "risky").is_dir()
        assert "risky" in branches()

        wm("remove risky", cwd=clone_of(env, "api"), stdin_input="y\n")
        assert not (worktrees / "risky").exists()
        assert "risky" not in branches()
        assert "wm-risky" not in windows()
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_remote_merge_merges_in_the_remotes_clone_without_pushing(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`merge` on a remote project merges the branch into main in the
    remote's clone, cleans up the worktree and window, and pushes nothing:
    the origin and the workstation repository are untouched."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    (root / "home" / ".zshrc").write_text("")
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    clone = root / "home" / "code" / "api"
    worktree = root / "home" / "code" / "api__worktrees" / "feature"

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def head(repo: Path) -> str:
        return env.run_command(["git", "rev-parse", "main"], cwd=repo).stdout.strip()

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    workstation_head = head(repo_path)
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        origin_head = head(origin)
        (worktree / "work.txt").write_text("done\n")
        env.run_command(["git", "add", "work.txt"], cwd=worktree)
        env.run_command(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
            + ["-m", "remote work"],
            cwd=worktree,
        )

        wm("merge feature", cwd=clone_of(env, "api"))

        log = env.run_command(
            ["git", "log", "--format=%s", "main"], cwd=clone
        ).stdout.split("\n")
        assert "remote work" in log
        assert not worktree.exists()
        windows = env.tmux(["list-windows", "-a", "-F", "#{window_name}"]).stdout
        assert "wm-feature" not in windows.split()
        assert head(origin) == origin_head
        assert head(repo_path) == workstation_head
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_commands_that_would_open_an_editor_refuse_for_a_remote_project(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`add --prompt-editor`, `add --auto-name` with no prompt, and a merge
    that would ask for a commit message refuse with a clear message and
    launch no editor; the merge leaves the worktree and its staged change
    in place."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    (root / "home" / ".zshrc").write_text("")
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    clone = root / "home" / "code" / "api"
    worktree = root / "home" / "code" / "api__worktrees" / "feature"
    opened = env.tmp_path / "editor-opened"
    editor = env.tmp_path / "editor.sh"
    editor.write_text(f'#!/bin/sh\ntouch {opened}\necho message > "$1"\n')
    editor.chmod(0o755)
    tmp_env = {**short_tmp, "EDITOR": str(editor), "GIT_EDITOR": str(editor)}

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=tmp_env,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    daemon_pid = None
    try:
        # Refused on the workstation: the remote is never even cloned.
        refused = wm(
            "add other --prompt-editor", cwd=clone_of(env, "api"), expect_fail=True
        )
        assert "cannot open an editor for remote project 'api'" in refused.stderr
        assert "-p" in refused.stderr
        refused = wm("add --auto-name", cwd=clone_of(env, "api"), expect_fail=True)
        assert "cannot open an editor for remote project 'api'" in refused.stderr
        assert not clone.exists()

        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        (worktree / "work.txt").write_text("done\n")
        env.run_command(["git", "add", "work.txt"], cwd=worktree)

        refused = wm("merge feature", cwd=clone_of(env, "api"), expect_fail=True)
        assert "has staged changes" in refused.stderr
        assert "cannot open an editor" in refused.stderr
        assert "git commit -m" in refused.stderr

        env.run_command(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
            + ["-m", "remote work"],
            cwd=worktree,
        )
        refused = wm(
            "merge feature --squash", cwd=clone_of(env, "api"), expect_fail=True
        )
        assert "--squash" in refused.stderr
        assert "cannot open an editor" in refused.stderr

        assert not opened.exists()
        assert worktree.exists()
        log = env.run_command(["git", "log", "--format=%s", "main"], cwd=clone)
        assert "remote work" not in log.stdout
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_a_prompt_file_is_read_on_the_workstation_for_a_remote_project(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`add -P FILE` and `open -P FILE` for a remote project use the
    workstation's FILE, though the remote has a file at the same path, and
    a missing FILE fails on the workstation without reaching the remote."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    clone = remote_root / "home" / "code" / "api"
    worktrees = remote_root / "home" / "code" / "api__worktrees"

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    def prompt_of(branch):
        return (worktrees / branch / ".workmux" / f"PROMPT-{branch}.md").read_text()

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    try:
        missing = wm(
            "add feature -P missing.md", cwd=clone_of(env, "api"), expect_fail=True
        )
        assert "Failed to read prompt file: missing.md" in missing.stderr
        assert not clone.exists()

        wm("add first", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        (clone_of(env, "api") / "prompt.md").write_text(
            "Use the workstation's prompt\n"
        )
        (clone / "prompt.md").write_text("Not the remote's prompt\n")

        wm("add feature -P prompt.md --prompt-file-only", cwd=clone_of(env, "api"))
        assert "Use the workstation's prompt" in prompt_of("feature")

        wm(
            "open first -n --prompt-file=prompt.md --prompt-file-only",
            cwd=clone_of(env, "api"),
        )
        assert "Use the workstation's prompt" in prompt_of("first")
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_a_workstation_agent_alias_runs_its_agent_type_on_a_remote_project(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`add -a ALIAS` for a remote project runs the alias's agent type on
    the remote, where the alias's workstation path means nothing. An alias
    with no known type is refused on the workstation."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    path = env.fake_bin_dir / "claude"
    path.write_text(FAKE_CLAUDE)
    path.chmod(0o755)
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: <agent>\n")
    clone = remote_root / "home" / "code" / "api"
    worktrees = remote_root / "home" / "code" / "api__worktrees"

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    config_path = env.home_path / ".config" / "workmux" / "config.yaml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    with config_path.open("a") as config:
        config.write(
            "agents:\n"
            "  cc: /nowhere/on/the/remote/claude --dangerously-skip-permissions\n"
            "  mine: /nowhere/on/the/remote/run-agent\n"
        )
    daemon_pid = None
    try:
        refused = wm("add feature -a mine", cwd=clone_of(env, "api"), expect_fail=True)
        assert "Agent 'mine' is a workstation alias" in refused.stderr
        assert not clone.exists()

        wm("add feature -a cc", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert wait_for(worktrees / "feature" / "agent-args.txt").strip() == "args:"
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


@requires_dtach
@pytest.mark.tmux_only
def test_ctrl_c_cancels_a_remote_command_during_a_long_hook(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """Ctrl-C in the invoking terminal while a remote post-create hook runs
    cancels the command on the remote, hook included, and the workstation
    command exits non-zero."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    out = env.tmp_path / "ctrl-c"
    out.mkdir()
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        f"post_create:\n  - echo $$ > {out}/hook.pid; exec sleep 60\n",
    )

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    daemon_pid = None
    try:
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        # The script outlives Ctrl-C (a handler, which workmux does not
        # inherit) to record workmux's exit code.
        script = out / "run.sh"
        script.write_text(
            "trap true INT\n"
            f"export PATH={shlex.quote(env.env['PATH'])}"
            f" HOME={shlex.quote(str(env.home_path))}"
            f" TMPDIR={short_tmp['TMPDIR']} WORKMUX_TEST=1\n"
            f"cd {clone_of(env, 'api')}\n"
            f"{workmux_exe_path} add slow > {out}/out 2> {out}/err\n"
            f"echo $? > {out}/code\n"
        )
        pane = env.runner_pane_id or "test:0"
        env.send_keys(pane, f"/bin/sh {script}", enter=True)
        hook = int(wait_for(out / "hook.pid", timeout=30).strip())
        assert pid_alive(hook)

        env.tmux(["send-keys", "-t", pane, "C-c"])

        code = int(wait_for(out / "code", timeout=15).strip())
        assert code != 0
        deadline = time.monotonic() + 10
        while pid_alive(hook):
            assert time.monotonic() < deadline, "the remote hook was not cancelled"
            time.sleep(0.05)
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


# A stand-in for an agent's full-screen interface. Like Claude Code, it
# repaints only on a real size change (or a key), so a plain reattach leaves
# the screen blank; "q" ends it, and with it the remote pane.
FAKE_TUI = r"""
import os, signal, sys, tty

size = None
key = "none"


def draw(force=False):
    global size
    now = os.get_terminal_size(0)
    if now == size and not force:
        return
    size = now
    sys.stdout.write(
        "\x1b[?1049h\x1b[2J\x1b[H"
        f"FAKE-TUI rows={now.lines} key={key}"
        f"\x1b[{now.lines};1HFAKE-TUI-FOOTER"
    )
    sys.stdout.flush()


signal.signal(signal.SIGWINCH, lambda *_: draw())
tty.setraw(0)
draw(force=True)
while True:
    try:
        ch = os.read(0, 1)
    except InterruptedError:
        continue
    if not ch or ch == b"q":
        sys.exit(7)
    key = ch.decode(errors="replace")
    draw(force=True)
"""


def wait_until(predicate, what: str, timeout=15.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


def pids(pattern: str) -> list[str]:
    return subprocess.run(
        ["pgrep", "-f", pattern], capture_output=True, text=True
    ).stdout.split()


@requires_dtach
@pytest.mark.tmux_only
def test_remote_panes_survive_drops_and_reattach(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """An agent pane runs on the remote inside the session holder. Killing the
    transport's attach process leaves the agent running: the workstation pane
    shows the reconnecting screen, reattaches on its own and redraws, however
    long the attach takes. "r" retries at once, "q" closes the view but not
    the remote pane, and the pane closes once the agent itself exits."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    agent = remote_root / "fake_tui.py"
    agent.write_text(FAKE_TUI)
    # While `slow` exists, attaching to the session holder takes longer than
    # any fixed redraw delay would wait (a passphrase prompt, a slow link).
    slow = remote_root / "slow"
    shim = env.fake_bin_dir / "dtach"
    shim.write_text(
        "#!/bin/sh\n"
        f'[ "$1" = -a ] && [ -e {slow} ] && sleep 4\n'
        f'exec {shutil.which("dtach")} "$@"\n'
    )
    shim.chmod(0o755)
    agent_pattern = str(agent)
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        f"panes:\n  - command: {shlex.quote(sys.executable)} {agent}\n",
    )

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        pane = tmux_value(env, "=test:wm-feature", "#{pane_id}")
        rows = tmux_value(env, pane, "#{pane_height}")

        def screen() -> str:
            return env.tmux(["capture-pane", "-p", "-t", pane], check=False).stdout

        def shows(text: str, timeout=15.0):
            try:
                wait_until(lambda: text in screen(), repr(text), timeout)
            except AssertionError:
                raise AssertionError(f"{text!r} not shown:\n{screen()}") from None

        def closed(target: str) -> bool:
            panes = env.tmux(["list-panes", "-a", "-F", "#{pane_id}"]).stdout
            return target not in panes.split()

        attach_pattern = f"dtach -a {re.escape(str(remote_root))}/.*/panes/api/feature/"

        def drop():
            clients = pids(attach_pattern)
            assert clients, "no attach process"
            for pid in clients:
                os.kill(int(pid), signal.SIGKILL)
            shows("reconnecting to lb")

        shows(f"FAKE-TUI rows={rows} key=none")
        env.tmux(["send-keys", "-t", pane, "-l", "x"])
        shows(f"FAKE-TUI rows={rows} key=x")
        agents = pids(agent_pattern)
        assert len(agents) == 1

        # A drop: the agent keeps running; the pane reattaches on its own
        # and the interface is redrawn at the pane's size.
        drop()
        shows(f"FAKE-TUI rows={rows} key=x")
        shows("FAKE-TUI-FOOTER")
        assert pids(agent_pattern) == agents

        # "r" retries without waiting out the (now longer) backoff.
        drop()
        env.tmux(["send-keys", "-t", pane, "r"])
        shows(f"FAKE-TUI rows={rows} key=x", timeout=1.5)

        # A slow reattach still redraws, once it is through.
        slow.write_text("")
        drop()
        env.tmux(["send-keys", "-t", pane, "r"])
        shows(f"FAKE-TUI rows={rows} key=x", timeout=10)
        shows("FAKE-TUI-FOOTER")
        slow.unlink()

        # "q" closes the workstation pane; the remote pane keeps running.
        drop()
        env.tmux(["send-keys", "-t", pane, "q"])
        wait_until(lambda: closed(pane), "the pane to close")
        assert pids(agent_pattern) == agents

        # When the remote pane's process exits, its pane closes as it would
        # locally. Like a local pane, the agent's pane falls back to a shell.
        wm("add second", cwd=clone_of(env, "api"))
        second = tmux_value(env, "=test:wm-second", "#{pane_id}")
        wait_until(
            lambda: (
                "FAKE-TUI"
                in env.tmux(["capture-pane", "-p", "-t", second], check=False).stdout
            ),
            "the second agent",
        )
        env.tmux(["send-keys", "-t", second, "-l", "q"])
        wait_until(lambda: pids(agent_pattern) == agents, "the agent to exit")
        env.tmux(["send-keys", "-t", second, "exit", "Enter"])
        wait_until(lambda: closed(second), "the ended pane to close")
    finally:
        for pid in pids(agent_pattern):
            os.kill(int(pid), signal.SIGKILL)
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


FAKE_NIX = """#!/bin/sh
# Records how it was called, then runs the devshell's command as is.
echo "$@" >> nix-args.txt
while [ "$#" -gt 0 ] && [ "$1" != "--command" ]; do shift; done
shift
exec "$@"
"""

FAKE_CLAUDE = """#!/bin/sh
echo "args:$*" >> agent-args.txt
"""


def claude_project_dir(home: Path, worktree: Path) -> Path:
    """Where Claude Code keeps a worktree's conversations under `home`."""
    encoded = "".join(
        c if c.isalnum() or c == "-" else "-" for c in str(worktree.resolve())
    )
    return home / ".claude" / "projects" / encoded


@requires_dtach
@pytest.mark.tmux_only
def test_remote_agents_start_in_the_devshell_and_resume_conversations(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """A remote project with `flake.nix` starts its agent through
    `nix develop` on the remote. `add --fork` forks a conversation stored on
    the remote, and `add --continue` resumes one."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    home = root / "home"
    (home / ".zshrc").write_text("")
    for name, script in [("nix", FAKE_NIX), ("claude", FAKE_CLAUDE)]:
        path = env.fake_bin_dir / name
        path.write_text(script)
        path.chmod(0o755)
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        "agent: claude\npanes:\n  - command: <agent>\n",
    )
    (origin / "flake.nix").write_text("{ outputs = _: { }; }\n")
    env.run_command(["git", "add", "flake.nix"], cwd=origin)
    env.run_command(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
        + ["-m", "flake"],
        cwd=origin,
    )

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    worktrees = home / "code" / "api__worktrees"
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        feature = worktrees / "feature"
        assert wait_for(feature / "nix-args.txt").strip() == (
            "develop --command bash -c claude"
        )
        assert wait_for(feature / "agent-args.txt").strip() == "args:"

        # A conversation Claude Code stored on the remote for the clone.
        session_id = "0a1b2c3d-1111-2222-3333-444455556666"
        stored = claude_project_dir(home, home / "code" / "api")
        stored.mkdir(parents=True)
        (stored / f"{session_id}.jsonl").write_text('{"type":"user"}\n')

        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        wm("add forked --fork", cwd=clone_of(env, "api"))
        forked = worktrees / "forked"
        assert wait_for(forked / "agent-args.txt").strip() == (
            f"args:--resume {session_id}"
        )
        copied = claude_project_dir(home, forked) / f"{session_id}.jsonl"
        assert copied.read_text() == '{"type":"user"}\n'

        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        wm("add again --continue", cwd=clone_of(env, "api"))
        assert wait_for(worktrees / "again" / "agent-args.txt").strip() == (
            "args:--continue"
        )
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_provisioning_installs_agent_hooks_and_status_warns_without_them(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """The first use of a remote project installs agent hooks on the remote
    without asking. `sandbox status` warns about a remote whose agents have
    no hooks."""
    env = mux_server
    provisioned = env.tmp_path / "provisioned"
    bare = env.tmp_path / "bare"
    for root in [provisioned, bare]:
        (root / "home" / ".claude").mkdir(parents=True)
    origin = make_origin(env, env.tmp_path / "origin", "panes: []\n")

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=provisioned
    )
    # A second project on a remote never provisioned for it.
    project_in_sandbox(
        env, workmux_exe_path, "other", origin, short_tmp, root=bare, remote="bare"
    )
    daemon_pid = None
    try:
        listing = wm("list", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert "Installed hooks to" in listing.stdout + listing.stderr
        settings = json.loads(
            (provisioned / "home" / ".claude" / "settings.json").read_text()
        )
        assert "hooks" in settings

        status = wm("sandbox status", cwd=clone_of(env, "api")).stdout
        assert "status:   reachable" in status
        # Agents found on the workstation's PATH (the loopback shares it)
        # may warn too; Claude Code's hooks were installed.
        assert "Claude Code has no status hooks" not in status, status
        assert "dtach" not in status, status

        status = wm("sandbox status", cwd=clone_of(env, "other")).stdout
        assert "status:   reachable" in status
        assert (
            "warning:  Claude Code has no status hooks on this remote,"
            " so its agents show no status; run `workmux setup --hooks` there"
        ) in status, status
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


# A stand-in agent: runs each status hook the test drops into `next`.
STATUS_AGENT = """\
while :; do
  if [ -e {next} ]; then
    status=$(cat {next}); rm {next}
    {workmux} set-window-status "$status"
    echo "$status" >> {done}
  fi
  sleep 0.05
done
"""


class SidebarFeed:
    """The workstation daemon's sidebar snapshots, as a sidebar receives them."""

    def __init__(self, tmpdir: str):
        sockets = list(Path(tmpdir).glob("workmux-sidebar-*.sock"))
        assert len(sockets) == 1, sockets
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(str(sockets[0]))
        self.sock.sendall(b'{"method":"subscribe"}\n')
        self.latest: dict = {}

    def _read(self, n: int) -> bytes:
        data = b""
        while len(data) < n:
            chunk = self.sock.recv(n - len(data))
            assert chunk, "the workstation daemon closed the sidebar feed"
            data += chunk
        return data

    def wait(self, predicate, what: str, timeout=20.0) -> dict:
        deadline = time.monotonic() + timeout
        while not predicate(self.latest):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"timed out waiting for {what}: {self.latest}")
            self.sock.settimeout(remaining)
            try:
                size = int.from_bytes(self._read(4), "big")
                self.latest = json.loads(self._read(size))
            except socket.timeout:
                continue
        return self.latest

    def close(self):
        self.sock.close()


def agent_in(snapshot: dict, pane: str) -> dict:
    return next((a for a in snapshot.get("agents", []) if a["pane_id"] == pane), {})


@requires_dtach
@pytest.mark.tmux_only
def test_remote_agent_status_shows_live_turns_stale_and_catches_up_on_reconnect(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
    outage: Path,
):
    """A remote agent's hooks reach the workstation sidebar, tagged with its
    remote. While the remote is unreachable its entry stays, stale, with its
    last status; work that started and finished meanwhile arrives on
    reconnect with the remote's times and wakes the snoozed agent."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    control = Path(short_tmp["TMPDIR"])
    next_status, done = control / "next", control / "done"
    agent = control / "agent.sh"
    # The hook never needs a multiplexer; point it at none, so a mistake
    # could never reach a real tmux server.
    hook = (
        f"env TMUX=/nonexistent/tmux,0,0 TMUX_TMPDIR={control} "
        f"{shlex.quote(str(workmux_exe_path))}"
    )
    agent.write_text(STATUS_AGENT.format(next=next_status, done=done, workmux=hook))
    origin = make_origin(
        env, env.tmp_path / "origin", f"panes:\n  - command: sh {agent}\n"
    )

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def hook_runs(status: str):
        count = len(done.read_text().split()) if done.exists() else 0
        next_status.write_text(status)
        wait_until(
            lambda: done.exists() and len(done.read_text().split()) > count,
            f"the {status} hook",
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    feed = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        pane = tmux_value(env, "=test:wm-feature", "#{pane_id}")
        feed = SidebarFeed(short_tmp["TMPDIR"])
        remote_pane = tmux_value(env, pane, "#{@workmux_remote_pane}")
        status_file = (
            remote_root / f"home/.local/state/workmux/status/{remote_pane}.json"
        )

        hook_runs("working")
        snapshot = feed.wait(
            lambda s: agent_in(s, pane).get("status") == "working",
            "the remote agent working",
        )
        entry = agent_in(snapshot, pane)
        assert entry["remote"] == {
            "remote": "lb",
            "project": "api",
            "worktree": "feature",
            "stale": False,
        }
        assert entry["session"] == "test"
        assert pane not in snapshot["stale_pane_ids"]
        assert tmux_value(env, pane, "#{@workmux_pane_status}") != ""

        # Snoozed on the workstation, then the remote becomes unreachable.
        env.tmux(["set-option", "-g", "@workmux_sleeping_panes", pane])
        outage.write_text("")
        serves = subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "remote serve"],
            capture_output=True,
            text=True,
        ).stdout.split()
        assert serves, "the daemon holds no control channel"
        for pid in serves:
            os.kill(int(pid), signal.SIGKILL)
        snapshot = feed.wait(
            lambda s: agent_in(s, pane).get("remote", {}).get("stale"),
            "the entry to turn stale",
        )
        assert agent_in(snapshot, pane)["status"] == "working"
        assert pane in snapshot["stale_pane_ids"]

        # Work finishes and starts again on the remote while it is
        # unreachable; it ends waiting for input.
        hook_runs("done")
        hook_runs("working")
        time.sleep(1.1)
        hook_runs("waiting")
        waiting_since = json.loads(status_file.read_text())["event_ts"]
        time.sleep(1.1)
        outage.unlink()

        snapshot = feed.wait(
            lambda s: (
                agent_in(s, pane).get("status") == "waiting"
                and not agent_in(s, pane)["remote"]["stale"]
            ),
            "the reconnected remote's status",
            timeout=45.0,
        )
        assert agent_in(snapshot, pane)["status_ts"] == waiting_since
        assert pane not in snapshot["stale_pane_ids"]
        assert (
            env.tmux(
                ["show-option", "-gqv", "@workmux_sleeping_panes"], check=False
            ).stdout.strip()
            == ""
        )
    finally:
        if feed is not None:
            feed.close()
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_open_and_resurrect_reattach_to_surviving_remote_panes(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """A remote worktree's window that goes away without workmux (killed in
    tmux, or lost with the whole tmux server) leaves its remote panes
    running. `open` and `resurrect` bring the window back showing those same
    remote panes in the configured layout, without starting the agent
    again."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    started = remote_root / "agent.pids"
    agent = remote_root / "agent.sh"
    agent.write_text(f"echo $$ >> {started}\nexec sleep 600\n")
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        f"panes:\n  - command: sh {agent}\n  - split: horizontal\n",
    )

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def remote_panes(window: str) -> list[str]:
        return env.tmux(
            ["list-panes", "-t", window, "-F", "#{@workmux_remote_pane}"]
        ).stdout.split()

    def window_gone(window: str) -> bool:
        return env.tmux(["list-panes", "-t", window], check=False).returncode != 0

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemons = []
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemons.append(
            remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)["pid"]
        )
        window = "=test:wm-feature"
        shown = remote_panes(window)
        assert len(shown) == 2, shown
        agent_pid = int(wait_for(started).strip())

        # The window is killed in tmux: its remote panes keep running.
        env.tmux(["kill-window", "-t", window])
        assert window_gone(window)
        assert pid_alive(agent_pid)

        reopened = wm("open feature", cwd=clone_of(env, "api"))
        assert "Opened tmux window for 'feature'" in reopened.stdout
        assert remote_panes(window) == shown
        assert started.read_text().split() == [str(agent_pid)]

        # Opening an open window only switches to it.
        again = wm("open feature", cwd=clone_of(env, "api"))
        assert "Switched to existing tmux window" in again.stdout
        assert remote_panes(window) == shown

        # The workstation's tmux server dies; the remote panes survive it.
        env.tmux(["kill-server"], check=False)
        env.start_server()
        assert pid_alive(agent_pid)

        # No window is left to name the project: resurrect is run in its
        # clone.
        planned = wm("resurrect --dry-run", cwd=clone_of(env, "api"))
        assert "feature" in planned.stdout and "restoring" in planned.stdout
        assert window_gone(window)

        restored = wm("resurrect", cwd=clone_of(env, "api"))
        assert "✓ Restored 1 worktree(s): feature" in restored.stdout
        assert remote_panes(window) == shown
        assert started.read_text().split() == [str(agent_pid)]
        daemons.append(
            remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)["pid"]
        )

        # Nothing survives unshown now: a second resurrect restores nothing.
        assert "Restored" not in wm("resurrect").stdout
    finally:
        for pid in daemons:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass


@requires_dtach
@pytest.mark.tmux_only
def test_remote_run_executes_in_the_remote_worktree(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`run` on a remote worktree runs its command on the remote in that
    worktree, in a pane of the worktree's window, streaming its output and
    exiting with its exit code."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    worktree = remote_root / "home" / "code" / "api__worktrees" / "feature"

    def wm(command, expect_fail=False, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])

        ran = wm(
            "run feature -- "
            "sh -c 'pwd -P; echo $WORKMUX_REMOTE_PANE; echo oops >&2; exit 3'",
            expect_fail=True,
            cwd=clone_of(env, "api"),
        )

        assert ran.exit_code == 3, ran
        lines = ran.stdout.splitlines()
        assert lines[0] == str(worktree.resolve())
        assert lines[1].startswith("api/feature/"), lines
        assert "oops" in ran.stderr
        assert "not supported on a remote" not in ran.stderr
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_reap_agents_reaps_aged_agents_on_the_remote(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`reap-agents` lists, and with --force exits, agents on a remote that
    have been idle past the threshold, reporting them as it does local ones.
    An agent's age comes from its status file (ADR 0002, remote status via files) when its hooks
    wrote one, else from its terminal's idle time. A recently active agent
    is left alone, and a reaped agent's pane stays open."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    started = remote_root / "agent.pids"
    agent = remote_root / "fake-agent"
    agent.write_text(f"#!/bin/sh\necho $$ >> {started}\nwhile :; do sleep 1; done\n")
    agent.chmod(0o755)
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        f"agent: {agent}\npanes:\n  - command: <agent>\n",
    )
    status_dir = remote_root / "home" / ".local" / "state" / "workmux" / "status"

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def add_agent(name: str) -> int:
        count = len(started.read_text().split()) if started.exists() else 0
        wm(f"add {name}", cwd=clone_of(env, "api"))
        wait_until(
            lambda: started.exists() and len(started.read_text().split()) > count,
            f"agent {name} to start",
        )
        return int(started.read_text().split()[-1])

    def idle_terminal(pid: int, since: float):
        tty = subprocess.run(
            ["ps", "-o", "tty=", "-p", str(pid)], capture_output=True, text=True
        ).stdout.strip()
        os.utime(f"/dev/{tty}", (since, since))

    def status_file(worktree: str, status: str, event_ts: float):
        pane = tmux_value(env, f"=test:wm-{worktree}", "#{@workmux_remote_pane}")
        slot = pane.rsplit("/", 1)[1]
        path = status_dir / "api" / worktree / f"{slot}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(
                {
                    "pane": pane,
                    "status": status,
                    "event_ts": int(event_ts),
                    "work_gen": 1,
                }
            )
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    try:
        busy = add_agent("busy")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        finished = add_agent("finished")
        unhooked = add_agent("unhooked")
        long_task = add_agent("longtask")
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        time.sleep(0.5)
        now = time.time()
        two_hours_ago = now - 2 * 3600
        # Its status is recent though its terminal is quiet: not reaped.
        idle_terminal(busy, two_hours_ago)
        status_file("busy", "working", now)
        # Done two hours ago by its status, though its terminal is fresh.
        status_file("finished", "done", two_hours_ago)
        # No hooks: its terminal was last used two hours ago.
        idle_terminal(unhooked, two_hours_ago)
        # Working on one task for two hours, still producing output: not old.
        status_file("longtask", "working", two_hours_ago)

        listed = wm("reap-agents --hours 1")
        assert "api (on lb):" in listed.stdout
        assert "Would exit api/finished/" in listed.stdout
        assert "in finished (2h, done, fake-agent)" in listed.stdout
        assert "Would exit api/unhooked/" in listed.stdout
        assert "in unhooked (2h, unknown, fake-agent)" in listed.stdout
        assert "busy" not in listed.stdout
        assert "longtask" not in listed.stdout
        assert pid_alive(finished) and pid_alive(unhooked)

        reaped = wm("reap-agents --hours 1 --force")
        assert "Exited api/finished/" in reaped.stdout
        assert "Exited api/unhooked/" in reaped.stdout
        wait_until(
            lambda: not pid_alive(finished) and not pid_alive(unhooked),
            "the idle agents to exit",
        )
        assert pid_alive(busy) and pid_alive(long_task)
        assert tmux_value(env, "=test:wm-finished", "#{window_name}") == "wm-finished"
    finally:
        for pid in started.read_text().split() if started.exists() else []:
            try:
                os.kill(int(pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_close_and_remove_end_the_worktrees_unshown_remote_panes_too(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`close` and `remove` end every remote pane of the worktree, not only
    those its window shows: a pane whose view was closed in tmux, and the
    survivors of a window killed outside workmux."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    holders = f"dtach -n {remote_root}/home/.local/state/workmux/panes/api/feature/"

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def running() -> int:
        return len(pids(holders))

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        window = "=test:wm-feature"
        [shown] = env.tmux(
            ["list-panes", "-t", window, "-F", "#{pane_id}"]
        ).stdout.split()

        # A long `run` gets a remote pane; its view is closed in tmux.
        wm("run feature --background -- sleep 600", cwd=clone_of(env, "api"))
        wait_until(lambda: running() == 2, "the run's remote pane")
        [run_view] = [
            pane
            for pane in env.tmux(
                ["list-panes", "-t", window, "-F", "#{pane_id}"]
            ).stdout.split()
            if pane != shown
        ]
        env.tmux(["kill-pane", "-t", run_view])
        time.sleep(0.5)
        assert running() == 2

        wm("close feature", cwd=clone_of(env, "api"))
        wait_until(lambda: running() == 0, "close to end every remote pane")

        # The window is killed outside workmux; `remove` ends its survivors.
        wm("open feature", cwd=clone_of(env, "api"))
        wait_until(lambda: running() == 1, "the reopened window's remote pane")
        env.tmux(["kill-window", "-t", window])
        time.sleep(0.5)
        assert running() == 1

        wm("remove feature", cwd=clone_of(env, "api"))
        wait_until(lambda: running() == 0, "remove to end every remote pane")
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


def shows_prompt(env, pane: str):
    """Wait until the agent pane's `sh` shows its prompt: keys typed before
    it starts are lost."""
    wait_until(
        lambda: "$" in env.tmux(["capture-pane", "-p", "-t", pane], check=False).stdout,
        f"a shell prompt in {pane}",
    )


def run_in_remote_pane(env, target: str, script_body: str, out_dir: Path, timeout=60.0):
    """Run a shell snippet in the remote pane the tmux pane `target` shows,
    in the remote pane's own environment, returning (exit code, stdout,
    stderr). Keys typed while the pane (re)attaches can be lost, so they are
    typed again until the script starts; it runs at most once."""
    out_dir.mkdir(exist_ok=True)
    for name in ["out", "err", "code"]:
        (out_dir / name).unlink(missing_ok=True)
    started = out_dir / "started"
    shutil.rmtree(started, ignore_errors=True)
    script = out_dir / "run.sh"
    script.write_text(
        f"mkdir {started} 2>/dev/null || exit 0\n"
        f"trap 'echo $? > {out_dir}/code' EXIT\n"
        f"({script_body}) > {out_dir}/out 2> {out_dir}/err\n"
    )
    deadline = time.monotonic() + timeout
    while not started.exists():
        assert time.monotonic() < deadline, env.capture_pane(target)
        env.send_keys(target, f"/bin/sh {script}", enter=True)
        retry = time.monotonic() + 3.0
        while not started.exists() and time.monotonic() < retry:
            time.sleep(0.05)
    while not (out_dir / "code").exists() or not (out_dir / "code").read_text():
        assert time.monotonic() < deadline, "\n".join(
            path.read_text() if path.exists() else f"(no {path.name})"
            for path in [out_dir / "out", out_dir / "err"]
        )
        time.sleep(0.05)
    return (
        int((out_dir / "code").read_text()),
        (out_dir / "out").read_text(),
        (out_dir / "err").read_text(),
    )


@requires_dtach
@pytest.mark.tmux_only
def test_commands_typed_in_a_remote_pane_reach_the_workstation(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """A remote pane's shell reaches the workstation through the shell
    socket, never a tmux of its own: `add` typed there creates a worktree on
    the remote and a window on the workstation, `close` typed in a window's
    own pane closes it (and `remove` removes it), the built-in and allowlisted shims run on the
    workstation and anything else is refused, and with the channel gone
    commands fail as not connected."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    root = remote_root
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    worktrees = root / "home" / "code" / "api__worktrees"
    bin_dir = root / "bin"
    bin_dir.mkdir()
    (bin_dir / "workmux").symlink_to(workmux_exe_path)
    # The shims stay first on PATH, then the workmux under test, ahead of
    # any installed one.
    shims = root / "home/.local/state/workmux/shims/bin"
    with_workmux = f'export PATH="{shims}:{bin_dir}:$PATH" WORKMUX_TEST=1; '

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def windows():
        return env.tmux(
            ["list-windows", "-a", "-F", "#{session_name}:#{window_name}"]
        ).stdout.split()

    # Global config allows the remote to run `pwd` on the workstation.
    global_config = env.home_path / ".config" / "workmux" / "config.yaml"
    global_config.parent.mkdir(parents=True, exist_ok=True)
    global_config.write_text("sandbox:\n  host_commands: [pwd]\n")
    project_in_sandbox(env, workmux_exe_path, "api", origin, short_tmp, root=root)
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        pane = tmux_value(env, "=test:wm-feature", "#{pane_id}")
        out = env.tmp_path / "pane-out"
        shows_prompt(env, pane)

        def in_pane(body: str):
            return run_in_remote_pane(env, pane, with_workmux + body, out)

        # The remote pane's shell has no tmux of its own.
        code, stdout, _ = in_pane('echo "tmux=${TMUX:-none}"; command -v afplay')
        assert code == 0
        assert stdout.splitlines() == [
            "tmux=none",
            f"{root}/home/.local/state/workmux/shims/bin/afplay",
        ]

        # `add` typed in the remote pane: the worktree on the remote, the
        # window in that pane's session on the workstation.
        code, stdout, stderr = in_pane("workmux add second")
        assert code == 0, stderr
        assert (worktrees / "second").is_dir()
        assert "test:wm-second" in windows()
        second = tmux_value(env, "=test:wm-second", "#{pane_id}")
        shows_prompt(env, second)
        assert env.tmux(
            ["show-options", "-wqv", "-t", second, "@workmux_project_window"]
        ).stdout.strip() == project_tag(env, "lb", "api")

        # An allowlisted host command runs on the workstation; anything
        # else is refused.
        code, stdout, stderr = in_pane("pwd -P; env pwd")
        assert code == 0, stderr
        here, there = stdout.splitlines()
        assert here == str((worktrees / "feature").resolve())
        assert there.endswith("/workmux-host-exec/lb"), stdout
        code, _, stderr = in_pane("workmux host-exec ls")
        assert code != 0
        assert "host command ls is not allowed for remote lb" in stderr

        # `close` typed in a window's own pane closes that window.
        code, _, stderr = in_pane("workmux close")
        assert code == 0, stderr
        wait_until(lambda: "test:wm-feature" not in windows(), "the window to close")
        assert (worktrees / "feature").is_dir()
        holders = f"dtach -n {root}/home/.local/state/workmux/panes/api/feature/"
        wait_until(
            lambda: subprocess.run(["pgrep", "-f", holders]).returncode == 1,
            "the window's remote panes to end",
        )

        # So does `remove`, which then removes the worktree.
        code, _, stderr = run_in_remote_pane(
            env, second, with_workmux + "workmux add third && workmux remove", out
        )
        assert code == 0, stderr
        wait_until(lambda: "test:wm-second" not in windows(), "the window to close")
        wait_until(lambda: not (worktrees / "second").exists(), "the worktree to go")
        third = tmux_value(env, "=test:wm-third", "#{pane_id}")
        shows_prompt(env, third)

        # With the control channel gone, commands fail as not connected.
        os.kill(daemon_pid, signal.SIGTERM)
        daemon_pid = None
        socket_path = root / "home/.local/state/workmux/shell.sock"
        wait_until(lambda: not socket_path.exists(), "the shell socket to go")
        code, _, stderr = run_in_remote_pane(
            env, third, with_workmux + "workmux add fourth", out
        )
        assert code != 0
        assert "Workstation not connected" in stderr
        assert not (worktrees / "fourth").exists()
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


# A stand-in agent that also takes input: each line typed into its pane is
# echoed back and recorded in `got`.
INPUT_AGENT = (
    "(\n"
    + STATUS_AGENT
    + """) &
echo "agent ready"
while IFS= read -r line; do
  echo "$line" >> {got}
  echo "got: $line"
done
"""
)


def drain(sock: socket.socket):
    try:
        while sock.recv(65536):
            pass
    except OSError:
        pass


def state_dir(env) -> Path:
    base = env.env.get("XDG_STATE_HOME") or str(env.home_path / ".local/state")
    return Path(base) / "workmux"


@requires_dtach
@pytest.mark.tmux_only
def test_agent_commands_reach_remote_agents_from_the_workstation(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
    outage: Path,
):
    """send, capture, status, wait, last-done and last-agent act on a remote
    agent through its workstation view and the state store. While its remote
    is unreachable the agent's status is stale: wait neither ends nor gives
    up on it, send refuses it, and last-done orders the status it finally
    delivers by the remote's time, among local agents."""
    from .test_popup_navigation import attached_client

    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    control = Path(short_tmp["TMPDIR"])
    next_status, done, got = control / "next", control / "done", control / "got"
    agent = control / "agent.sh"
    hook = (
        f"env TMUX=/nonexistent/tmux,0,0 TMUX_TMPDIR={control} "
        f"{shlex.quote(str(workmux_exe_path))}"
    )
    agent.write_text(
        INPUT_AGENT.format(next=next_status, done=done, got=got, workmux=hook)
    )
    origin = make_origin(
        env, env.tmp_path / "origin", f"panes:\n  - command: sh {agent}\n"
    )

    def wm(command, expect_fail=False, timeout=60.0, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=timeout,
        )

    def hook_runs(status: str):
        count = len(done.read_text().split()) if done.exists() else 0
        next_status.write_text(status)
        wait_until(
            lambda: done.exists() and len(done.read_text().split()) > count,
            f"the {status} hook",
        )

    def remote_entry() -> dict:
        report = json.loads(wm("status --all --json").stdout)
        return next(a for a in report["agents"] if a.get("remote"))

    def client_pane(client: str) -> str:
        return env.tmux(
            ["display-message", "-p", "-c", client, "#{pane_id}"]
        ).stdout.strip()

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    feed = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        # A sidebar keeps the workstation daemon running; read its feed so
        # the daemon never drops it as a slow client.
        feed = SidebarFeed(short_tmp["TMPDIR"])
        threading.Thread(target=drain, args=(feed.sock,), daemon=True).start()
        view = tmux_value(env, "=test:wm-feature", "#{pane_id}")
        wait_until(lambda: "agent ready" in (env.capture_pane(view) or ""), "the agent")

        # status and wait read the state store; send and capture use the view.
        hook_runs("working")
        wm("wait api:feature --status working --timeout 20")
        entry = remote_entry()
        assert entry["pane_id"] == view
        assert entry["worktree"] == "feature"
        assert entry["project"] == "api@lb"
        assert entry["status"] == "working"
        assert entry["remote"] == {
            "remote": "lb",
            "project": "api",
            "worktree": "feature",
            "stale": False,
        }
        assert "working" in wm("status api:feature").stdout

        wm("send feature 'hello from the workstation'")
        wait_until(
            lambda: got.exists() and "hello from the workstation" in got.read_text(),
            "the sent prompt",
        )
        wait_until(
            lambda: (
                "got: hello from the workstation" in wm("capture feature -n 20").stdout
            ),
            "the reply in the captured view",
        )

        hook_runs("done")
        wm("wait feature --timeout 20")

        # The remote becomes unreachable: its entry is stale, done as last seen.
        outage.write_text("")
        for pid in subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "remote serve"],
            capture_output=True,
            text=True,
        ).stdout.split():
            os.kill(int(pid), signal.SIGKILL)
        wait_until(lambda: remote_entry()["remote"]["stale"], "a stale entry")
        assert remote_entry()["status"] == "done"
        assert "done (stale)" in wm("status --all").stdout

        result = wm("wait feature --timeout 3", expect_fail=True)
        assert result.exit_code == 1, result.stderr
        assert "feature: lb is disconnected; waiting for it to reconnect" in (
            result.stderr
        )
        assert "Timeout waiting for: feature" in result.stderr
        result = wm("send feature 'lost'", expect_fail=True)
        assert "lb is disconnected; cannot send to feature" in result.stderr

        # Work restarts and finishes on the remote, before a local agent
        # finishes; the remote's status arrives last, on reconnect.
        hook_runs("working")
        time.sleep(1.1)
        hook_runs("done")
        time.sleep(1.1)
        # A local agent, whose hooks run on the workstation.
        local_next, local_done = control / "local-next", control / "local-done"
        local_agent = control / "local-agent.sh"
        local_agent.write_text(
            STATUS_AGENT.format(
                next=local_next,
                done=local_done,
                workmux=f"env TMPDIR={control} {shlex.quote(str(workmux_exe_path))}",
            )
        )
        # In a session of its own: switching to it leaves the test window
        # current in the test session.
        local = env.tmux(
            ["new-session", "-d", "-P", "-F", "#{pane_id}", "-s", "local"]
            + [f"sh {local_agent}"]
        ).stdout.strip()

        def set_local_status(status: str):
            count = len(local_done.read_text().split()) if local_done.exists() else 0
            local_next.write_text(status)
            wait_until(
                lambda: (
                    local_done.exists() and len(local_done.read_text().split()) > count
                ),
                f"the local {status} hook",
            )

        set_local_status("done")
        outage.unlink()
        wm("wait feature --timeout 60", timeout=75.0)
        assert not remote_entry()["remote"]["stale"]

        with attached_client(env) as (_, client):
            # The local agent finished last by the clock, though the
            # remote's status arrived later.
            wm("last-done")
            assert client_pane(client) == local
            set_local_status("working")
            wm("last-done")
            assert client_pane(client) == view

            # last-agent switches to the pane remembered last, and remembers
            # the pane it ran from.
            invoking = env.runner_pane_id
            settings = state_dir(env) / "settings.json"
            current = json.loads(settings.read_text()) if settings.exists() else {}
            settings.write_text(json.dumps({**current, "last_pane_id": local}))
            wm("last-agent")
            assert client_pane(client) == local
            assert json.loads(settings.read_text())["last_pane_id"] == invoking
    finally:
        if feed is not None:
            feed.close()
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


def dashboard_row(screen: str, text: str) -> str:
    return next((line for line in screen.splitlines() if text in line), "")


@requires_dtach
@pytest.mark.tmux_only
def test_dashboard_reviews_stages_and_closes_a_remote_worktree(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
    outage: Path,
):
    """The dashboard, filtered to its session, lists the remote
    agent with its remote worktree's changes and not an agent of another
    session. Its diff comes from the remote, staging a hunk changes the
    remote's index, and killing the entry ends the remote pane too."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    control = Path(short_tmp["TMPDIR"])
    next_status, done = control / "next", control / "done"
    agent = control / "agent.sh"
    hook = (
        f"env TMUX=/nonexistent/tmux,0,0 TMUX_TMPDIR={control} "
        f"{shlex.quote(str(workmux_exe_path))}"
    )
    agent.write_text(STATUS_AGENT.format(next=next_status, done=done, workmux=hook))
    origin = make_origin(
        env, env.tmp_path / "origin", f"panes:\n  - command: sh {agent}\n"
    )
    lines = "".join(f"line {n}\n" for n in range(1, 21))
    (origin / "notes.txt").write_text(lines)
    env.run_command(["git", "add", "notes.txt"], cwd=origin)
    env.run_command(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
        + ["-m", "notes"],
        cwd=origin,
    )

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    feed = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        pane = tmux_value(env, "=test:wm-feature", "#{pane_id}")
        remote_pane = tmux_value(env, pane, "#{@workmux_remote_pane}")
        next_status.write_text("waiting")
        wait_until(lambda: done.exists(), "the remote agent's status hook")
        worktree = remote_root / "home" / "code" / "api__worktrees" / "feature"
        (worktree / "notes.txt").write_text(
            lines.replace("line 2\n", "line two\n").replace(
                "line 19\n", "line nineteen\n"
            )
        )

        # An agent of the workstation, in another session.
        local_agent = control / "local-agent.sh"
        local_agent.write_text(
            f"export PATH={shlex.quote(env.env['PATH'])} "
            f"HOME={shlex.quote(str(env.home_path))}\n"
            f"cd {repo_path} && {workmux_exe_path} set-window-status waiting"
            f" && touch {control}/local-waiting\n"
            # Block in the shell itself, so the pane's command stays the agent's.
            f"mkfifo {control}/hold && read _ < {control}/hold\n"
        )
        env.tmux(
            ["new-session", "-d", "-s", "other", "-n", "wm-local", f"sh {local_agent}"]
        )
        wait_until(
            lambda: (control / "local-waiting").exists(), "the local agent's status"
        )

        env.tmux(["new-window", "-t", "test:", "-n", "dash"])
        env.tmux(["resize-window", "-t", "=test:dash", "-x", "220", "-y", "50"])
        launch = control / "dashboard.sh"
        launch.write_text(
            f"export PATH={shlex.quote(env.env['PATH'])} "
            f"HOME={shlex.quote(str(env.home_path))} "
            f"TMPDIR={short_tmp['TMPDIR']} WORKMUX_TEST=1\n"
            f"exec {workmux_exe_path} dashboard --session\n"
        )
        env.send_keys("=test:dash", f"sh {launch}", enter=True)

        def screen() -> str:
            return env.capture_pane("=test:dash") or ""

        def wait_for_screen(predicate, what: str, timeout=15.0):
            try:
                wait_until(lambda: predicate(screen()), what, timeout)
            except AssertionError as error:
                raise AssertionError(f"{error}\n{screen()}") from None

        wait_for_screen(
            lambda shown: (
                "+2" in dashboard_row(shown, "feature")
                and "-2" in dashboard_row(shown, "feature")
            ),
            "the remote worktree's changes in the dashboard",
            timeout=30.0,
        )
        assert "api@lb" in dashboard_row(screen(), "feature"), screen()

        # The sidebar has the same status from the remote, filed under the
        # remote's key and never under the remote path, which on a loopback
        # remote is also a workstation path.
        # The feed also keeps the workstation daemon alive throughout.
        feed = SidebarFeed(short_tmp["TMPDIR"])
        snapshot = feed.wait(
            lambda s: any(
                key.startswith("remote/lb/api/")
                and status.get("uncommitted_added") == 2
                for key, status in s.get("git_statuses", {}).items()
            ),
            "the remote worktree's git status in the sidebar",
        )
        path = agent_in(snapshot, pane)["path"]
        assert f"remote/lb/api/{path.lstrip('/')}" in snapshot["git_statuses"]
        # Workstation git, which would file it under the path, never runs.
        with pytest.raises(AssertionError, match="timed out"):
            feed.wait(
                lambda s: path in s.get("git_statuses", {}),
                "workstation git status of the remote path",
                timeout=6.0,
            )
        assert "local" not in screen(), screen()

        # The diff is the remote worktree's; stage its first hunk.
        env.send_keys("=test:dash", "d", enter=False)
        wait_for_screen(lambda shown: "line nineteen" in shown, "the remote diff")
        env.send_keys("=test:dash", "a", enter=False)
        wait_for_screen(lambda shown: "line two" in shown, "patch mode")
        env.send_keys("=test:dash", "y", enter=False)
        wait_until(
            lambda: "+line two" in git_in(env, worktree, "diff", "--cached"),
            "the hunk staged in the remote's index",
        )
        assert "nineteen" not in git_in(env, worktree, "diff", "--cached")
        env.send_keys("=test:dash", "q", enter=False)
        env.send_keys("=test:dash", "q", enter=False)

        # A disconnected remote's entry stays, stale, with its last status.
        outage.write_text("")
        for pid in subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "remote serve"],
            capture_output=True,
            text=True,
        ).stdout.split():
            os.kill(int(pid), signal.SIGKILL)
        stale_icon = "\U000f051b"
        wait_for_screen(
            lambda shown: stale_icon in dashboard_row(shown, "feature"),
            "the remote entry to turn stale",
        )
        assert "💬" in dashboard_row(screen(), "feature")
        outage.unlink()
        wait_for_screen(
            lambda shown: (
                "feature" in shown and stale_icon not in dashboard_row(shown, "feature")
            ),
            "the remote to reconnect",
            timeout=45.0,
        )

        # Killing the remote entry ends the remote pane, not just its view.
        socket_path = (
            remote_root / f"home/.local/state/workmux/panes/{remote_pane}.sock"
        )
        assert socket_path.exists()
        env.send_keys("=test:dash", "X", enter=False)
        wait_until(lambda: not socket_path.exists(), "the remote pane to end")
        panes = env.tmux(["list-panes", "-a", "-F", "#{pane_id}"]).stdout.split()
        assert pane not in panes
    finally:
        if feed is not None:
            feed.close()
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_dashboard_worktrees_tab_lists_adds_closes_and_removes_remote_worktrees(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """The Worktrees tab's project picker offers a project registered on a
    remote. Picked, the tab lists the remote's worktrees, and adding,
    closing and removing one act on the remote's clone and the project's
    workstation window."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    control = Path(short_tmp["TMPDIR"])
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    clone = remote_root / "home" / "code" / "api"
    worktrees = remote_root / "home" / "code" / "api__worktrees"
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]

        env.tmux(["new-window", "-t", "test:", "-n", "dash"])
        env.tmux(["resize-window", "-t", "=test:dash", "-x", "220", "-y", "50"])
        launch = control / "dashboard.sh"
        launch.write_text(
            f"export PATH={shlex.quote(env.env['PATH'])} "
            f"HOME={shlex.quote(str(env.home_path))} "
            f"TMPDIR={short_tmp['TMPDIR']} WORKMUX_TEST=1\n"
            f"cd {repo_path}\n"
            f"exec {workmux_exe_path} dashboard --tab worktrees\n"
        )
        env.send_keys("=test:dash", f"sh {launch}", enter=True)

        def screen() -> str:
            return env.capture_pane("=test:dash") or ""

        def wait_for_screen(predicate, what: str, timeout=20.0):
            try:
                wait_until(lambda: predicate(screen()), what, timeout)
            except AssertionError as error:
                raise AssertionError(f"{error}\n{screen()}") from None

        def keys(*sequence: str):
            for key in sequence:
                env.send_keys("=test:dash", key, enter=False)
                time.sleep(0.2)

        def select(handle: str):
            for _ in range(10):
                if dashboard_row(screen(), handle).lstrip().startswith("▌"):
                    return
                keys("j")
            raise AssertionError(f"could not select {handle}\n{screen()}")

        wait_for_screen(lambda shown: "Worktrees" in shown, "the Worktrees tab")
        keys("p")
        wait_for_screen(
            lambda shown: "api@lb" in shown, "the remote project in the picker"
        )
        keys("a", "p", "i", "@", "l", "b", "Enter")
        wait_for_screen(
            lambda shown: "api@lb" in dashboard_row(shown, "feature"),
            "the remote's worktrees",
        )

        # Add: remote workmux creates the worktree and its window.
        keys("a")
        wait_for_screen(lambda shown: "main" in shown, "the add picker")
        keys(*"fresh", "Enter")
        wait_until(lambda: (worktrees / "fresh").is_dir(), "the remote worktree")
        wait_until(
            lambda: (
                env.tmux(
                    ["list-windows", "-t", "=test", "-F", "#{window_name}"],
                    check=False,
                )
                .stdout.split()
                .count("wm-fresh")
                == 1
            ),
            "the worktree's window",
        )
        wait_for_screen(
            lambda shown: "api@lb" in dashboard_row(shown, "fresh"),
            "the new worktree listed",
        )

        # Close keeps the worktree; remove takes it and its branch.
        select("fresh")
        keys("c")
        wait_until(
            lambda: (
                "wm-fresh"
                not in env.tmux(
                    ["list-windows", "-t", "=test", "-F", "#{window_name}"], check=False
                ).stdout.split()
            ),
            "the window closed",
        )
        assert (worktrees / "fresh").is_dir()
        select("fresh")
        keys("r")
        wait_for_screen(lambda shown: "Remove" in shown, "the remove confirmation")
        keys("y")
        wait_until(lambda: not (worktrees / "fresh").exists(), "the worktree removed")
        # The branch goes after the worktree.
        wait_until(
            lambda: git_in(env, clone, "branch", "--list", "fresh") == "",
            "the branch removed",
        )
        assert (worktrees / "feature").is_dir()
        # Nothing was created on the workstation.
        assert git_in(env, repo_path, "branch", "--list", "fresh") == ""
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_dashboard_shows_pr_status_for_a_remote_projects_branches_while_unreachable(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
    outage: Path,
):
    """The Worktrees tab lists a remote project's worktrees from the last
    list the workstation saw while the remote cannot be reached, so their
    branches keep their PR status."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    control = Path(short_tmp["TMPDIR"])
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    project_in_sandbox(
        env, workmux_exe_path, "api", origin, short_tmp, root=remote_root
    )
    daemon_pid = None
    try:
        wm("add feature", cwd=clone_of(env, "api"))
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["new-window", "-t", "test:", "-n", "dash"])
        env.tmux(["resize-window", "-t", "=test:dash", "-x", "220", "-y", "50"])
        launch = control / "dashboard.sh"
        launch.write_text(
            f"export PATH={shlex.quote(env.env['PATH'])} "
            f"HOME={shlex.quote(str(env.home_path))} "
            f"TMPDIR={short_tmp['TMPDIR']} WORKMUX_TEST=1\n"
            f"cd {repo_path}\n"
            f"exec {workmux_exe_path} dashboard --tab worktrees\n"
        )

        def screen() -> str:
            return env.capture_pane("=test:dash") or ""

        def wait_for_screen(predicate, what: str, timeout=20.0):
            try:
                wait_until(lambda: predicate(screen()), what, timeout)
            except AssertionError as error:
                raise AssertionError(f"{error}\n{screen()}") from None

        def keys(*sequence: str):
            for key in sequence:
                env.send_keys("=test:dash", key, enter=False)
                time.sleep(0.2)

        def show_remote_project():
            env.send_keys("=test:dash", f"sh {launch}", enter=True)
            wait_for_screen(lambda shown: "Worktrees" in shown, "the Worktrees tab")
            keys("p")
            wait_for_screen(
                lambda shown: "api@lb" in shown, "the remote project in the picker"
            )
            keys("a", "p", "i", "@", "l", "b", "Enter")

        show_remote_project()
        wait_for_screen(
            lambda shown: "api@lb" in dashboard_row(shown, "feature"),
            "the remote's worktrees",
        )
        keys("q")
        wait_until(lambda: "Worktrees" not in screen(), "the dashboard to quit")

        # The workstation knows a PR for the branch, and the remote goes away.
        cache = env.home_path / ".cache" / "workmux"
        cache.mkdir(parents=True, exist_ok=True)
        (cache / "pr_status_cache.json").write_text(
            json.dumps(
                {
                    "remote/lb/api": {
                        "feature": {
                            "number": 77,
                            "title": "Remote feature",
                            "state": "OPEN",
                            "isDraft": False,
                        }
                    }
                }
            )
        )
        outage.write_text("")
        for pid in subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "remote serve"],
            capture_output=True,
            text=True,
        ).stdout.split():
            os.kill(int(pid), signal.SIGKILL)

        show_remote_project()
        wait_for_screen(
            lambda shown: "#77" in dashboard_row(shown, "feature"),
            "the remote branch's PR while the remote is unreachable",
        )
        keys("q")
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@pytest.mark.tmux_only
def test_pruning_openshell_sandboxes_closes_their_workstation_windows(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """`sandbox prune` deletes workmux's (`wm-`) sandboxes after one
    confirmation, which names the commits that exist only in them, and
    closes every workstation window tagged with their projects, leaving the
    user's other windows and other sandboxes alone. `openshell` is a fake on
    PATH."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    state = env.tmp_path / "openshell"
    state.mkdir()
    clone = project_in_sandbox(
        env, workmux_exe_path, "api", "https://example.com/api.git", short_tmp
    )
    sandbox = openshell_sandbox_name(clone)
    (state / f"phase-{sandbox}").write_text("Ready")
    (state / "phase-trellis-7fba5166").write_text("Ready")
    env.install_script(
        env.fake_bin_dir / "openshell",
        f"""#!/bin/sh
printf '%s\\n' "$*" >> {state}/log
case "$1 $2" in
'sandbox list')
  printf '{{"next_page_token":"","sandboxes":['
  sep=
  for entry in {sandbox}:api trellis-7fba5166:trellis; do
    name=${{entry%:*}}
    [ -e {state}/phase-$name ] || continue
    printf '%s{{"name":"%s","phase":"%s","annotations":{{"workmux.project":"%s"}}}}' \\
      "$sep" $name "$(cat {state}/phase-$name)" "${{entry#*:}}"
    sep=,
  done
  printf ']}}' ;;
'sandbox get')
  [ -e {state}/phase-$3 ] || {{ echo "sandbox not found" >&2; exit 1; }}
  printf '{{"phase":"%s","policy":{{}}}}' "$(cat {state}/phase-$3)" ;;
'sandbox delete') rm -f {state}/phase-$3 ;;
'sandbox exec') echo '[{{"path":null,"branch":"spike","uncommitted":false,"unpushed":1,"unmerged_into":null}}]' ;;
esac
""",
    )

    def wm(command, cwd=None, stdin_input=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            stdin_input=stdin_input,
            timeout=30.0,
        )

    tag = project_tag(env, sandbox, "api")
    env.tmux(["new-session", "-d", "-s", "api", "-n", "wm-feature"])
    env.tmux(["set-option", "-t", "=api:", "@workmux_project", tag])
    env.tmux(
        [
            "set-option",
            "-w",
            "-t",
            "=api:wm-feature",
            "@workmux_project_window",
            tag,
        ]
    )
    env.tmux(["new-window", "-d", "-t", "test:", "-n", "wm-other"])
    env.tmux(
        [
            "set-option",
            "-w",
            "-t",
            "=test:wm-other",
            "@workmux_project_window",
            tag,
        ]
    )
    env.tmux(["new-window", "-d", "-t", "test:", "-n", "mine"])

    pruned = wm("sandbox prune", cwd=clone_of(env, "api"), stdin_input="y\n")

    assert sandbox in pruned.stdout
    assert f"{sandbox}: branch spike: 1 commit not on any remote" in pruned.stdout
    assert "trellis-7fba5166" not in pruned.stdout
    log = (state / "log").read_text()
    assert f"sandbox delete {sandbox}" in log
    assert "trellis-7fba5166" not in log
    assert (state / "phase-trellis-7fba5166").exists()
    assert "Closed 2 workstation window(s) of project api" in pruned.stderr
    windows = env.tmux(["list-windows", "-a", "-F", "#{window_name}"]).stdout.split()
    assert "wm-feature" not in windows
    assert "wm-other" not in windows
    assert "mine" in windows
    sessions = env.tmux(["list-sessions", "-F", "#{session_name}"]).stdout.split()
    assert "api" not in sessions


FAKE_OPENSHELL = """#!/usr/bin/env python3
# A fake `openshell`: each sandbox's phase is a file, and exec runs the
# command on this machine under the sandbox's own HOME.
import json, os, sys

state, exe = {state!r}, {exe!r}
args = sys.argv[1:]
with open(os.path.join(state, "log"), "a") as log:
    log.write(" ".join(args) + "\\n")
if args[:2] == ["sandbox", "get"]:
    phase = os.path.join(state, args[2] + ".phase")
    if not os.path.exists(phase):
        sys.exit("sandbox not found")
    print(json.dumps({{"phase": open(phase).read().strip(), "policy": {{}}}}))
elif args[:2] == ["sandbox", "exec"]:
    split = args.index("--")
    options, argv = args[2:split], args[split + 1:]
    name = options[options.index("-n") + 1]
    home = os.path.join(state, name)
    os.makedirs(home, exist_ok=True)
    env = {{
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("TMUX", "WORKMUX_"))
    }}
    env.update(
        HOME=home,
        XDG_CONFIG_HOME=home + "/.config",
        XDG_STATE_HOME=home + "/.local/state",
        XDG_CACHE_HOME=home + "/.cache",
    )
    workdir = home
    for flag, value in zip(options, options[1:]):
        if flag == "--env":
            key, _, value = value.partition("=")
            env[key] = value
        elif flag == "--workdir":
            workdir = value
    if argv[0] == "workmux":
        argv[0] = exe
    os.chdir(workdir)
    os.execvpe(argv[0], argv, env)
"""


@pytest.mark.tmux_only
def test_stop_and_prune_act_on_lima_in_a_repository_whose_config_fails_to_load(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """`sandbox stop` and `sandbox prune` never needed the repository's
    config for Lima's VMs: one that fails to load leaves them acting on
    Lima, never on OpenShell. `limactl` and `openshell` are fakes on PATH
    that log every call."""
    env = mux_server
    log = env.tmp_path / "calls"
    for tool in ("limactl", "openshell"):
        env.install_script(
            env.fake_bin_dir / tool,
            f"#!/bin/sh\nprintf '{tool} %s\\n' \"$*\" >> {log}\n",
        )
    (repo_path / ".workmux.yaml").write_text("sandbox: [not, a, mapping\n")

    stopped = sandbox(env, workmux_exe_path, repo_path, "stop --all", short_tmp)
    pruned = sandbox(env, workmux_exe_path, repo_path, "prune", short_tmp)

    assert "No running workmux VMs found" in stopped.stdout
    assert "VM" in pruned.stdout, pruned.stdout
    calls = log.read_text().splitlines()
    assert "limactl list --json" in calls, calls
    assert not [call for call in calls if call.startswith("openshell")], calls


@pytest.mark.tmux_only
def test_a_command_for_every_project_checks_each_remotes_state_once(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """A command relayed to every remote project asks each OpenShell
    sandbox for its state once, whether or not the workstation daemon has
    its channel open yet, and reconciles a running one's policy from that
    same state. A stopped sandbox is skipped, not started. `openshell` is
    a fake on PATH."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    state = Path(tempfile.mkdtemp(prefix="wmo", dir="/tmp"))
    origin = make_origin(env, env.tmp_path / "origin", "panes:\n  - command: sh\n")
    env.install_script(
        env.fake_bin_dir / "openshell",
        FAKE_OPENSHELL.format(state=str(state), exe=str(workmux_exe_path)),
    )
    log = state / "log"

    def wm(command, cwd=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            cwd or repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def calls() -> dict:
        lines = log.read_text().splitlines() if log.exists() else []
        log.unlink(missing_ok=True)
        by_sandbox: dict = {}
        for line in lines:
            words = line.split()
            name = words[words.index("-n") + 1] if "-n" in words else words[2]
            by_sandbox.setdefault(name, []).append(" ".join(words[:2]))
        return by_sandbox

    daemon_pid = None
    try:
        sandbox = {}
        for name in ("api", "web"):
            clone = project_in_sandbox(
                env,
                workmux_exe_path,
                name,
                origin,
                short_tmp,
                config=f"  openshell:\n    path: {state}/{name}/{name}\n",
            )
            sandbox[name] = openshell_sandbox_name(clone)
            # Every project with an open workstation window is visited.
            env.tmux(["new-window", "-d", "-t", "test:", "-n", f"{name}-window"])
            env.tmux(
                [
                    "set-option",
                    "-w",
                    "-t",
                    f"=test:{name}-window",
                    "@workmux_project_window",
                    project_tag(env, sandbox[name], name),
                ]
            )
        (state / f"{sandbox['api']}.phase").write_text("Ready")
        (state / f"{sandbox['web']}.phase").write_text("Stopped")
        log.unlink(missing_ok=True)
        cold = wm("reap-agents")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert "Skipping remote project 'web'" in cold.stderr
        assert (state / "api" / "api" / ".git").is_dir()
        first = calls()
        assert first[sandbox["web"]] == ["sandbox get"], first
        assert "sandbox exec" in first[sandbox["api"]], first
        assert [call for call in first[sandbox["api"]] if call != "sandbox exec"] == [
            "sandbox get",
            "policy set",
        ], first

        wm("reap-agents")

        assert calls() == {
            sandbox["api"]: ["sandbox get", "policy set"],
            sandbox["web"]: ["sandbox get"],
        }
        assert (state / f"{sandbox['web']}.phase").read_text() == "Stopped"
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)
        subprocess.run(["pkill", "-f", f"{state}/"], check=False)
        shutil.rmtree(state, ignore_errors=True)


def reporting_gateway(env, state: Path):
    """Fake `openshell` and `docker` on PATH that log every call into
    `state`: `sandbox get` reports the phase in `state/phase` (no file: no
    sandbox), `sandbox list` lists nothing and anything else fails, as
    `docker` always does. Returns a function giving the `openshell`
    subcommands called since it was last called."""
    state.mkdir()
    env.install_script(
        env.fake_bin_dir / "openshell",
        f"""#!/bin/sh
printf '%s\\n' "$*" >> {state}/log
case "$1 $2" in
'sandbox get')
  [ -e {state}/phase ] || {{ echo "sandbox not found" >&2; exit 1; }}
  printf '{{"phase":"%s","policy":{{}}}}' "$(cat {state}/phase)" ;;
'sandbox list') echo '{{"next_page_token":"","sandboxes":[]}}' ;;
*) echo "unexpected: $*" >&2; exit 1 ;;
esac
""",
    )
    env.install_script(
        env.fake_bin_dir / "docker",
        f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> {state}/docker-log\nexit 1\n",
    )

    def calls() -> set:
        log = state / "log"
        lines = log.read_text().splitlines() if log.exists() else []
        log.unlink(missing_ok=True)
        return {" ".join(line.split()[:2]) for line in lines}

    return calls


@pytest.mark.tmux_only
def test_read_only_commands_never_create_or_start_an_openshell_sandbox(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """In a repository whose config puts it in an OpenShell sandbox,
    commands that only report (`sandbox status` among them) say the
    sandbox doesn't exist yet, or is
    stopped, and never create, start or build it. `openshell` and `docker`
    are fakes on PATH that log every call."""
    env = mux_server
    state = env.tmp_path / "openshell"
    calls = reporting_gateway(env, state)
    clone = project_in_sandbox(
        env, workmux_exe_path, "trellis", env.tmp_path / "origin", short_tmp
    )
    sandbox = openshell_sandbox_name(clone)

    def wm(command, expect_fail=False):
        return run_workmux_command(
            env,
            workmux_exe_path,
            clone,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=30.0,
        )

    # No sandbox yet.
    listed = wm("list")
    assert (
        "No OpenShell sandbox for trellis yet; `workmux add <branch>` creates it"
        in listed.stdout
    )
    assert wm("list --json").stdout.strip() == "[]"
    missing = wm("path feature", expect_fail=True)
    assert "No OpenShell sandbox for trellis yet" in missing.stderr
    wm("status")
    status = wm("sandbox status").stdout
    assert status.startswith(f"{sandbox} (openshell {sandbox})"), status
    assert "not created yet" in status
    assert calls() <= {"sandbox get", "sandbox list"}

    # A stopped sandbox.
    (state / "phase").write_text("Stopped")
    stopped = f"OpenShell sandbox {sandbox} (project trellis) is stopped"
    assert stopped in wm("list").stdout
    assert wm("list --json").stdout.strip() == "[]"
    assert stopped in wm("path feature", expect_fail=True).stderr
    wm("status")
    assert "stopped" in wm("sandbox status").stdout
    assert calls() <= {"sandbox get", "sandbox list"}
    assert (state / "phase").read_text() == "Stopped"
    assert not (state / "docker-log").exists()


@pytest.mark.tmux_only
def test_capture_wait_and_the_dashboard_and_sidebar_never_create_or_start_an_openshell_sandbox(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """With an agent of the repository's OpenShell sandbox in the state
    store, `capture`, `wait`, the sidebar's refresh of the agent's git status
    and the dashboard's never create or start the sandbox, whether it
    doesn't exist yet or is stopped. `openshell` and `docker` are fakes on
    PATH that log every call."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    control = Path(short_tmp["TMPDIR"])
    state = env.tmp_path / "openshell"
    calls = reporting_gateway(env, state)
    clone = project_in_sandbox(
        env, workmux_exe_path, "trellis", env.tmp_path / "origin", short_tmp
    )
    sandbox = openshell_sandbox_name(clone)

    def wm(command, expect_fail=False):
        return run_workmux_command(
            env,
            workmux_exe_path,
            clone,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=30.0,
        )

    # The sandbox's agent: a workstation pane that reported a status, filed
    # as the agent of a remote pane in the sandbox.
    agent = control / "agent.sh"
    agent.write_text(
        f"export PATH={shlex.quote(env.env['PATH'])} "
        f"HOME={shlex.quote(str(env.home_path))}\n"
        f"cd {clone} && {workmux_exe_path} set-window-status working"
        f" && touch {control}/reported\n"
        # Block in the shell itself, so the pane's command stays the agent's.
        f"mkfifo {control}/hold && read _ < {control}/hold\n"
    )
    env.tmux(["new-window", "-d", "-t", "test:", "-n", "wm-feature", f"sh {agent}"])
    wait_until(lambda: (control / "reported").exists(), "the agent's status")
    env.tmux(
        [
            "set-option",
            "-w",
            "-t",
            "=test:wm-feature",
            "@workmux_project_window",
            project_tag(env, sandbox, "trellis"),
        ]
    )
    (agent_file,) = (state_dir(env) / "agents").glob("*.json")
    entry = json.loads(agent_file.read_text())
    entry["workdir"] = "/sandbox/trellis__worktrees/feature"
    entry["remote"] = {
        "remote": sandbox,
        "pane": "trellis/feature/0",
        "work_gen": 0,
    }
    agent_file.write_text(json.dumps(entry))
    calls()

    env.tmux(["new-window", "-d", "-t", "test:", "-n", "dash"])
    env.tmux(["resize-window", "-t", "=test:dash", "-x", "220", "-y", "50"])
    launch = control / "dashboard.sh"
    launch.write_text(
        f"export PATH={shlex.quote(env.env['PATH'])} "
        f"HOME={shlex.quote(str(env.home_path))} "
        f"TMPDIR={short_tmp['TMPDIR']} WORKMUX_TEST=1\n"
        f"cd {clone}\n"
        f"exec {workmux_exe_path} dashboard\n"
    )

    def screen() -> str:
        return env.capture_pane("=test:dash") or ""

    for phase in [None, "Stopped"]:
        if phase:
            (state / "phase").write_text(phase)

        wm("capture feature")
        wm("wait feature --timeout 1", expect_fail=True)
        wm("wait other --timeout 1", expect_fail=True)

        wm("sidebar on")
        feed = SidebarFeed(short_tmp["TMPDIR"])
        try:
            feed.wait(
                lambda s: any(a.get("remote") for a in s.get("agents", [])),
                "the sandbox's agent in the sidebar",
            )
            # Longer than the sidebar's interval between remote git refreshes.
            time.sleep(6)
        finally:
            feed.close()
        wm("sidebar off")

        env.send_keys("=test:dash", f"sh {launch}", enter=True)
        wait_until(lambda: "feature" in screen(), "the agent in the dashboard")
        time.sleep(3)
        env.send_keys("=test:dash", "q", enter=False)
        wait_until(lambda: "feature" not in screen(), "the dashboard to quit")

        assert calls() <= {"sandbox get", "sandbox list"}
        if phase:
            assert (state / "phase").read_text() == phase
        else:
            assert not (state / "phase").exists()
        assert not (state / "docker-log").exists()
