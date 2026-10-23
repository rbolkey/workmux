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

from .conftest import MuxEnvironment, TmuxEnvironment, run_workmux_command


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


requires_dtach = pytest.mark.skipif(
    shutil.which("dtach") is None,
    reason="remote panes need the session holder dtach on PATH",
)


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


@pytest.fixture
def local_sshd(workmux_exe_path: Path):
    """A user-mode sshd on a free localhost port, configured entirely from a
    temp dir like the Rust SSH harness (never ~/.ssh or the system sshd).

    Host `wmx-test` reaches a remote whose shell startup echoes noise and puts
    workmux on PATH; `wmx-nowm` reaches the same server without workmux.
    """
    if os.environ.get("WORKMUX_TEST_SSH") != "1":
        pytest.skip("set WORKMUX_TEST_SSH=1 to run SSH tests")
    d = Path(tempfile.mkdtemp(prefix="wmx", dir="/tmp"))
    for key in ["host_key", "client_key"]:
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(d / key)],
            check=True,
        )
    shutil.copy(d / "client_key.pub", d / "authorized_keys")
    remote_home = d / "remote-home"
    remote_home.mkdir()
    rc = d / "rc"
    rc.mkdir()
    # A chatty shell startup must not break the control channel.
    startup = (
        "echo 'Welcome to the remote'; printf partial\n"
        f"export HOME={remote_home} XDG_CONFIG_HOME={remote_home}/.config\n"
        f"export XDG_STATE_HOME={remote_home}/.local/state\n"
        f"export PATH={Path(shutil.which('dtach') or '/usr/bin/dtach').parent}:/usr/bin:/bin\n"
        '[ -n "$WMX_NO_WORKMUX" ] || '
        f"export PATH={workmux_exe_path.parent}:$PATH\n"
    )
    (rc / ".zshenv").write_text(startup)
    (rc / ".bashrc").write_text(startup)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    (d / "sshd_config").write_text(
        f"Port {port}\n"
        "ListenAddress 127.0.0.1\n"
        f"HostKey {d}/host_key\n"
        f"AuthorizedKeysFile {d}/authorized_keys\n"
        f"PidFile {d}/sshd.pid\n"
        "PasswordAuthentication no\n"
        "KbdInteractiveAuthentication no\n"
        "UsePAM no\n"
        "StrictModes no\n"
        "AcceptEnv WMX_*\n"
        f"SetEnv ZDOTDIR={rc} BASH_ENV={rc}/.bashrc\n"
    )
    user = os.environ["USER"]

    def host(alias, extra=""):
        return (
            f"Host {alias}\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n"
            f"  IdentityFile {d}/client_key\n  IdentitiesOnly yes\n"
            "  IdentityAgent none\n  UserKnownHostsFile /dev/null\n"
            "  StrictHostKeyChecking no\n  LogLevel ERROR\n" + extra
        )

    ssh_config = d / "ssh_config"
    ssh_config.write_text(
        host("wmx-test") + host("wmx-nowm", "  SetEnv WMX_NO_WORKMUX=1\n")
    )
    sshd = subprocess.Popen(
        ["/usr/sbin/sshd", "-D", "-f", str(d / "sshd_config"), "-E", str(d / "log")]
    )
    deadline = time.monotonic() + 5
    while True:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            break
        except OSError:
            assert time.monotonic() < deadline, (d / "log").read_text()
            time.sleep(0.02)
    yield ssh_config
    sshd.kill()
    # End the remote panes left running on the "remote".
    subprocess.run(["pkill", "-f", f"dtach -n {d}/"], check=False)
    sshd.wait()
    shutil.rmtree(d, ignore_errors=True)


@pytest.mark.tmux_only
def test_remote_add_status_rm_against_ssh_to_localhost(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    local_sshd: Path,
):
    """An SSH remote is reachable through the workstation daemon; one without
    workmux names the version to install."""
    env = mux_server
    version = env.run_command([str(workmux_exe_path), "--version"]).stdout.split()[-1]
    # The daemon runs the system ssh from PATH; point it at the test config.
    env.install_script(
        env.fake_bin_dir / "ssh", f'#!/bin/sh\nexec /usr/bin/ssh -F {local_sshd} "$@"\n'
    )

    remote(
        env,
        workmux_exe_path,
        repo_path,
        "add box --transport ssh --target wmx-test",
        short_tmp,
    )
    remote(
        env,
        workmux_exe_path,
        repo_path,
        "add nowm --transport ssh --target wmx-nowm",
        short_tmp,
    )
    daemon_pid = None
    try:
        status = remote(
            env, workmux_exe_path, repo_path, "status box", short_tmp
        ).stdout
        assert "status:   reachable" in status
        assert f"workmux:  {version}" in status
        assert "protocol: 1" in status

        # The workstation daemon holds the channel through ssh.
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        children = subprocess.run(
            ["pgrep", "-P", str(daemon_pid), "-f", "wmx-test"],
            capture_output=True,
            text=True,
        ).stdout.split()
        assert len(children) == 1

        failed = remote(
            env, workmux_exe_path, repo_path, "status nowm", short_tmp, expect_fail=True
        )
        assert "status:   unreachable" in failed.stdout
        assert (
            f"workmux is not installed on nowm: install workmux {version}"
            in failed.stdout
        )

        remote(env, workmux_exe_path, repo_path, "rm nowm", short_tmp)
        remote(env, workmux_exe_path, repo_path, "rm box", short_tmp)
        listing = remote(env, workmux_exe_path, repo_path, "list", short_tmp).stdout
        assert "box" not in listing
        assert "nowm" not in listing
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
def test_remote_add_places_windows_in_the_project_session(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    remote_root: Path,
):
    """`add` on a remote project creates the worktree on the remote, runs its
    post-create hooks there with output streamed back, and puts the window in
    a workstation session named after the project, whatever session the
    command was typed in. A session the user made is adopted; the `session:`
    override, current-session placement and session mode still work."""
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

    def wm(command, expect_fail=False):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    wm(
        f"project add web --remote lb --repo {origin} --path code/web --session frontend"
    )
    wm(
        f"project add docs --remote lb --repo {origin} --path code/docs"
        " --placement current-session"
    )
    daemon_pid = None
    try:
        # Typed in session "test"; the window goes to the project's session.
        added = wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert "post-create ran in" in added.stdout + added.stderr
        worktree = root / "home" / "code" / "api__worktrees" / "feature"
        assert worktree.is_dir()
        assert tmux_value(env, "=api:wm-feature", "#{session_name}") == "api"
        assert session_tag(env, "api") == "lb:api"
        # The branch lives in the remote's clone, not on the workstation.
        for repo in [origin, repo_path]:
            branches = env.run_command(
                ["git", "branch", "--list", "feature"], cwd=repo
            ).stdout
            assert branches.strip() == ""
        # The pane runs on the remote, in the remote worktree.
        ran = wait_for(worktree / "pane-ran.txt")
        assert ran.strip() == str(worktree.resolve())

        # A session the user made is adopted and tagged.
        env.tmux(["new-session", "-d", "-s", "frontend"])
        wm("add other --project web")
        assert tmux_value(env, "=frontend:wm-other", "#{session_name}") == "frontend"
        assert session_tag(env, "frontend") == "lb:web"

        # Current-session placement keeps the window where the command ran.
        wm("add notes --project docs")
        assert tmux_value(env, "=test:wm-notes", "#{session_name}") == "test"
        assert session_tag(env, "test") == ""

        # The new window took focus; later commands run in the test pane.
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])

        # Session mode gives the worktree its own session.
        wm("add solo --project api --mode session")
        assert session_tag(env, "wm-solo") == "lb:api"

        # Typed without --project in the project's session (in a pane of the
        # user's own), the command targets the tagged project.
        env.tmux(["new-window", "-d", "-t", "api:", "-n", "mine"])
        code, out, err = run_in_pane(
            env,
            "=api:mine",
            f"cd /tmp && TMPDIR={short_tmp['TMPDIR']} {workmux_exe_path} add tagged",
            env.tmp_path / "pane-run",
            timeout=60.0,
        )
        assert code == 0, err
        assert (root / "home" / "code" / "api__worktrees" / "tagged").is_dir()
        assert tmux_value(env, "=api:wm-tagged", "#{session_name}") == "api"
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

    def wm(command, expect_fail=False, from_pane=None):
        tmp_env = dict(short_tmp)
        if from_pane is not None:
            tmp_env["TMUX_PANE"] = from_pane
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=tmp_env,
            expect_fail=expect_fail,
            timeout=60.0,
        )
        assert "unsupported on remote" not in result.stderr, result.stderr
        return result

    wm(f"remote add lb --transport loopback --target {root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    clone = root / "home" / "code" / "api"
    worktrees = root / "home" / "code" / "api__worktrees"
    daemon_pid = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        window = tmux_value(env, "=api:wm-feature", "#{pane_id}")

        # set-base typed in the worktree's window sets that branch's base.
        git_in(env, clone, "branch", "develop")
        set_base = wm("set-base develop --project api", from_pane=window)
        assert set_base.stdout.strip() == "Set base branch for 'feature' to 'develop'"
        assert git_in(env, clone, "config", "branch.feature.workmux-base") == "develop"

        # rebase with no name rebases the window's worktree onto its base.
        git_in(env, clone, "checkout", "-q", "develop")
        git_in(env, clone, "commit", "-q", "--allow-empty", "-m", "on develop")
        git_in(env, clone, "checkout", "-q", "main")
        rebased = wm("rebase --project api", from_pane=window)
        assert "✓ Rebased 'feature' onto 'develop'" in rebased.stdout
        assert git_in(env, clone, "log", "-1", "--format=%s", "feature") == (
            "on develop"
        )

        # sync-files, routed by the project session's tag alone, copies the
        # main worktree's files into the window's worktree.
        (clone / "local.env").write_text("SECRET=1\n")
        synced = wm("sync-files", from_pane=window)
        assert "✓ Synced files to 'feature'" in synced.stdout
        assert (worktrees / "feature" / "local.env").read_text() == "SECRET=1\n"
        outside = wm("sync-files --project api", expect_fail=True)
        assert outside.exit_code == 1
        assert "Current directory is the main worktree" in outside.stderr

        # rename moves the remote worktree, its branch and the window in the
        # project's session, though typed in another session.
        renamed = wm("rename feature renamed --branch --project api")
        assert "✓ Renamed worktree 'feature' -> 'renamed'" in renamed.stdout
        assert "✓ Renamed 1 tmux target" in renamed.stdout
        assert (worktrees / "renamed").is_dir()
        assert not (worktrees / "feature").exists()
        assert git_in(env, clone, "branch", "--list", "renamed") != ""
        assert git_in(env, clone, "branch", "--list", "feature") == ""
        assert tmux_value(env, window, "#{session_name}:#{window_name}") == (
            "api:wm-renamed"
        )
        # The window still leads to its worktree after the rename.
        again = wm("set-base main --project api", from_pane=window)
        assert again.stdout.strip() == "Set base branch for 'renamed' to 'main'"

        # init writes .workmux.yaml into the remote repository, refusing to
        # overwrite one exactly as locally.
        exists = wm("init --project api", expect_fail=True)
        assert exists.exit_code == 1
        assert ".workmux.yaml already exists" in exists.stderr
        (clone / ".workmux.yaml").unlink()
        created = wm("init --project api")
        assert "✓ Created .workmux.yaml" in created.stdout
        assert "workmux project configuration" in (clone / ".workmux.yaml").read_text()
        assert not (repo_path / ".workmux.yaml").exists()
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)


@requires_dtach
@pytest.mark.tmux_only
def test_remote_add_and_remove_over_ssh_to_localhost(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
    local_sshd: Path,
):
    """`add` over the SSH transport creates the worktree on the remote and a
    window in the project's session whose pane runs there; `remove` takes
    the worktree, its branch and the window away again, after the
    unmerged-branch confirmation is answered in the invoking terminal."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    env.install_script(
        env.fake_bin_dir / "ssh", f'#!/bin/sh\nexec /usr/bin/ssh -F {local_sshd} "$@"\n'
    )
    remote_home = local_sshd.parent / "remote-home"
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        'post_create:\n  - echo "post-create ran in $PWD"\n'
        "panes:\n  - command: pwd -P > pane-ran.txt\n",
    )

    def wm(command, stdin_input=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            stdin_input=stdin_input,
            timeout=60.0,
        )

    wm("remote add box --transport ssh --target wmx-test")
    wm(f"project add api --remote box --repo {origin} --path code/api")
    daemon_pid = None
    try:
        added = wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert "post-create ran in" in added.stdout + added.stderr
        worktree = remote_home / "code" / "api__worktrees" / "feature"
        assert worktree.is_dir()
        assert tmux_value(env, "=api:wm-feature", "#{session_name}") == "api"
        assert session_tag(env, "api") == "box:api"
        assert wait_for(worktree / "pane-ran.txt", timeout=30).strip() == str(
            worktree.resolve()
        )

        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        env.run_command(["git", "add", "pane-ran.txt"], cwd=worktree)
        env.run_command(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
            + ["-m", "unmerged work"],
            cwd=worktree,
        )
        removed = wm("remove feature --project api", stdin_input="y\n")
        assert "Are you sure you want to continue? [y/N]" in removed.stdout
        assert not worktree.exists()
        branches = env.run_command(
            ["git", "branch", "--list", "feature"], cwd=remote_home / "code" / "api"
        ).stdout
        assert branches.strip() == ""
        windows = env.tmux(["list-windows", "-a", "-F", "#{window_name}"]).stdout
        assert "wm-feature" not in windows.split()
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

    def wm(command, expect_fail=False, stdin_input=None):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
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

    wm(f"remote add lb --transport loopback --target {root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        assert "wm-feature" in windows()

        # `close` closes the window, ending its remote panes, and keeps the
        # worktree.
        holders = f"dtach -n {root}/home/.local/state/workmux/panes/api/feature/"
        assert subprocess.run(["pgrep", "-f", holders]).returncode == 0
        wm("close feature --project api")
        assert "wm-feature" not in windows()
        assert (worktrees / "feature").is_dir()
        wait_until(
            lambda: subprocess.run(["pgrep", "-f", holders]).returncode == 1,
            "the remote panes to end",
        )

        wm("remove feature --project api")
        assert not (worktrees / "feature").exists()
        assert "feature" not in branches()
        assert "wm-feature" not in windows()

        # An unmerged branch asks first; the answer comes from this terminal.
        wm("add risky --project api")
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        env.run_command(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"]
            + ["--allow-empty", "-m", "unmerged work"],
            cwd=worktrees / "risky",
        )
        declined = wm("remove risky --project api", stdin_input="n\n")
        assert "risky (base: main)" in declined.stdout
        assert "Are you sure you want to continue? [y/N]" in declined.stdout
        assert "Aborted." in declined.stdout
        assert (worktrees / "risky").is_dir()
        assert "risky" in branches()

        wm("remove risky --project api", stdin_input="y\n")
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def head(repo: Path) -> str:
        return env.run_command(["git", "rev-parse", "main"], cwd=repo).stdout.strip()

    wm(f"remote add lb --transport loopback --target {root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    workstation_head = head(repo_path)
    daemon_pid = None
    try:
        wm("add feature --project api")
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

        wm("merge feature --project api")

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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
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
            f"cd {repo_path}\n"
            f"{workmux_exe_path} add slow --project api > {out}/out 2> {out}/err\n"
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
    shows the reconnecting screen, reattaches on its own and redraws. "r"
    retries at once, "q" closes the view but not the remote pane, and the pane
    closes once the agent itself exits."""
    env = mux_server
    assert isinstance(env, TmuxEnvironment)
    agent = remote_root / "fake_tui.py"
    agent.write_text(FAKE_TUI)
    agent_pattern = str(agent)
    origin = make_origin(
        env,
        env.tmp_path / "origin",
        f"panes:\n  - command: {shlex.quote(sys.executable)} {agent}\n",
    )

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        pane = tmux_value(env, "=api:wm-feature", "#{pane_id}")
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

        # "q" closes the workstation pane; the remote pane keeps running.
        drop()
        env.tmux(["send-keys", "-t", pane, "q"])
        wait_until(lambda: closed(pane), "the pane to close")
        assert pids(agent_pattern) == agents

        # When the remote pane's process exits, its pane closes as it would
        # locally. Like a local pane, the agent's pane falls back to a shell.
        wm("add second --project api")
        second = tmux_value(env, "=api:wm-second", "#{pane_id}")
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    worktrees = home / "code" / "api__worktrees"
    daemon_pid = None
    try:
        wm("add feature --project api")
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
        wm("add forked --project api --fork")
        forked = worktrees / "forked"
        assert wait_for(forked / "agent-args.txt").strip() == (
            f"args:--resume {session_id}"
        )
        copied = claude_project_dir(home, forked) / f"{session_id}.jsonl"
        assert copied.read_text() == '{"type":"user"}\n'

        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        wm("add again --project api --continue")
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
    without asking. `remote status` warns about a remote whose agents have
    no hooks."""
    env = mux_server
    provisioned = env.tmp_path / "provisioned"
    bare = env.tmp_path / "bare"
    for root in [provisioned, bare]:
        (root / "home" / ".claude").mkdir(parents=True)
    origin = make_origin(env, env.tmp_path / "origin", "panes: []\n")

    def wm(command, expect_fail=False):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {provisioned}")
    wm(f"remote add bare --transport loopback --target {bare}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    try:
        listing = wm("list --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        assert "Installed hooks to" in listing.stdout + listing.stderr
        settings = json.loads(
            (provisioned / "home" / ".claude" / "settings.json").read_text()
        )
        assert "hooks" in settings

        status = wm("remote status lb").stdout
        assert "status:   reachable" in status
        # Agents found on the workstation's PATH (the loopback shares it)
        # may warn too; Claude Code's hooks were installed.
        assert "Claude Code has no status hooks" not in status, status
        assert "dtach" not in status, status

        status = wm("remote status bare").stdout
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
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

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    feed = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        pane = tmux_value(env, "=api:wm-feature", "#{pane_id}")
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
        assert entry["session"] == "api"
        assert pane not in snapshot["stale_pane_ids"]
        assert tmux_value(env, pane, "#{@workmux_pane_status}") != ""

        # Snoozed on the workstation, then the remote becomes unreachable.
        env.tmux(["set-option", "-g", "@workmux_sleeping_panes", pane])
        (remote_root / "offline").write_text("")
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
        (remote_root / "offline").unlink()

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
        (remote_root / "offline").unlink(missing_ok=True)
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
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

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemons = []
    try:
        wm("add feature --project api")
        daemons.append(
            remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)["pid"]
        )
        window = "=api:wm-feature"
        shown = remote_panes(window)
        assert len(shown) == 2, shown
        agent_pid = int(wait_for(started).strip())

        # The window is killed in tmux: its remote panes keep running.
        env.tmux(["kill-window", "-t", window])
        assert window_gone(window)
        assert pid_alive(agent_pid)

        reopened = wm("open feature --project api")
        assert "Opened tmux window for 'feature'" in reopened.stdout
        assert remote_panes(window) == shown
        assert started.read_text().split() == [str(agent_pid)]

        # Opening an open window only switches to it.
        again = wm("open feature --project api")
        assert "Switched to existing tmux window" in again.stdout
        assert remote_panes(window) == shown

        # The workstation's tmux server dies; the remote panes survive it.
        env.tmux(["kill-server"], check=False)
        env.start_server()
        assert pid_alive(agent_pid)

        planned = wm("resurrect --dry-run")
        assert "feature" in planned.stdout and "restoring" in planned.stdout
        assert window_gone(window)

        restored = wm("resurrect")
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

    def wm(command, expect_fail=False):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            expect_fail=expect_fail,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])

        ran = wm(
            "run feature --project api -- "
            "sh -c 'pwd -P; echo $WORKMUX_REMOTE_PANE; echo oops >&2; exit 3'",
            expect_fail=True,
        )

        assert ran.exit_code == 3, ran
        lines = ran.stdout.splitlines()
        assert lines[0] == str(worktree.resolve())
        assert lines[1].startswith("api/feature/"), lines
        assert "oops" in ran.stderr
        assert "unsupported on remote" not in ran.stderr
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
    An agent's age comes from its status file (ticket 11) when its hooks
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def add_agent(name: str) -> int:
        count = len(started.read_text().split()) if started.exists() else 0
        wm(f"add {name} --project api")
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
        pane = tmux_value(env, f"=api:wm-{worktree}", "#{@workmux_remote_pane}")
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

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
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
        assert tmux_value(env, "=api:wm-finished", "#{window_name}") == "wm-finished"
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def running() -> int:
        return len(pids(holders))

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        window = "=api:wm-feature"
        [shown] = env.tmux(
            ["list-panes", "-t", window, "-F", "#{pane_id}"]
        ).stdout.split()

        # A long `run` gets a remote pane; its view is closed in tmux.
        wm("run feature --project api --background -- sleep 600")
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

        wm("close feature --project api")
        wait_until(lambda: running() == 0, "close to end every remote pane")

        # The window is killed outside workmux; `remove` ends its survivors.
        wm("open feature --project api")
        wait_until(lambda: running() == 1, "the reopened window's remote pane")
        env.tmux(["kill-window", "-t", window])
        time.sleep(0.5)
        assert running() == 1

        wm("remove feature --project api")
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    def windows():
        return env.tmux(
            ["list-windows", "-a", "-F", "#{session_name}:#{window_name}"]
        ).stdout.split()

    wm(f"remote add lb --transport loopback --target {root} --host-command pwd")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        env.tmux(["select-window", "-t", env.runner_pane_id or "test:0"])
        pane = tmux_value(env, "=api:wm-feature", "#{pane_id}")
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
        # window in the project's session on the workstation.
        code, stdout, stderr = in_pane("workmux add second")
        assert code == 0, stderr
        assert (worktrees / "second").is_dir()
        assert "api:wm-second" in windows()
        second = tmux_value(env, "=api:wm-second", "#{pane_id}")
        shows_prompt(env, second)
        assert (
            env.tmux(
                ["show-options", "-wqv", "-t", second, "@workmux_project_window"]
            ).stdout.strip()
            == "lb:api"
        )

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
        wait_until(lambda: "api:wm-feature" not in windows(), "the window to close")
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
        wait_until(lambda: "api:wm-second" not in windows(), "the window to close")
        wait_until(lambda: not (worktrees / "second").exists(), "the worktree to go")
        third = tmux_value(env, "=api:wm-third", "#{pane_id}")
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
        assert "workstation not connected" in stderr
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

    def wm(command, expect_fail=False, timeout=60.0):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
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

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    feed = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        # A sidebar keeps the workstation daemon running; read its feed so
        # the daemon never drops it as a slow client.
        feed = SidebarFeed(short_tmp["TMPDIR"])
        threading.Thread(target=drain, args=(feed.sock,), daemon=True).start()
        view = tmux_value(env, "=api:wm-feature", "#{pane_id}")
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
        (remote_root / "offline").write_text("")
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
        (remote_root / "offline").unlink()
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
            invoking = tmux_value(env, "test:", "#{pane_id}")
            settings = state_dir(env) / "settings.json"
            current = json.loads(settings.read_text()) if settings.exists() else {}
            settings.write_text(json.dumps({**current, "last_pane_id": local}))
            wm("last-agent")
            assert client_pane(client) == local
            assert json.loads(settings.read_text())["last_pane_id"] == invoking
    finally:
        if feed is not None:
            feed.close()
        (remote_root / "offline").unlink(missing_ok=True)
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
):
    """The dashboard, filtered to the project's session, lists the remote
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    daemon_pid = None
    feed = None
    try:
        wm("add feature --project api")
        daemon_pid = remote_daemon_ping(env, workmux_exe_path, repo_path, short_tmp)[
            "pid"
        ]
        pane = tmux_value(env, "=api:wm-feature", "#{pane_id}")
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
            ["new-window", "-d", "-t", "test:", "-n", "wm-local", f"sh {local_agent}"]
        )
        wait_until(
            lambda: (control / "local-waiting").exists(), "the local agent's status"
        )

        env.tmux(["new-window", "-t", "api:", "-n", "dash"])
        env.tmux(["resize-window", "-t", "=api:dash", "-x", "220", "-y", "50"])
        launch = control / "dashboard.sh"
        launch.write_text(
            f"export PATH={shlex.quote(env.env['PATH'])} "
            f"HOME={shlex.quote(str(env.home_path))} "
            f"TMPDIR={short_tmp['TMPDIR']} WORKMUX_TEST=1\n"
            f"exec {workmux_exe_path} dashboard --session\n"
        )
        env.send_keys("=api:dash", f"sh {launch}", enter=True)

        def screen() -> str:
            return env.capture_pane("=api:dash") or ""

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
        env.send_keys("=api:dash", "d", enter=False)
        wait_for_screen(lambda shown: "line nineteen" in shown, "the remote diff")
        env.send_keys("=api:dash", "a", enter=False)
        wait_for_screen(lambda shown: "line two" in shown, "patch mode")
        env.send_keys("=api:dash", "y", enter=False)
        wait_until(
            lambda: "+line two" in git_in(env, worktree, "diff", "--cached"),
            "the hunk staged in the remote's index",
        )
        assert "nineteen" not in git_in(env, worktree, "diff", "--cached")
        env.send_keys("=api:dash", "q", enter=False)
        env.send_keys("=api:dash", "q", enter=False)

        # A disconnected remote's entry stays, stale, with its last status.
        (remote_root / "offline").write_text("")
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
        (remote_root / "offline").unlink()
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
        env.send_keys("=api:dash", "X", enter=False)
        wait_until(lambda: not socket_path.exists(), "the remote pane to end")
        panes = env.tmux(["list-panes", "-a", "-F", "#{pane_id}"]).stdout.split()
        assert pane not in panes
    finally:
        if feed is not None:
            feed.close()
        (remote_root / "offline").unlink(missing_ok=True)
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

    def wm(command):
        return run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            command,
            pre_run_env=short_tmp,
            timeout=60.0,
        )

    wm(f"remote add lb --transport loopback --target {remote_root}")
    wm(f"project add api --remote lb --repo {origin} --path code/api")
    clone = remote_root / "home" / "code" / "api"
    worktrees = remote_root / "home" / "code" / "api__worktrees"
    daemon_pid = None
    try:
        wm("add feature --project api")
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
                    ["list-windows", "-t", "=api", "-F", "#{window_name}"],
                    check=False,
                )
                .stdout.split()
                .count("wm-fresh")
                == 1
            ),
            "the worktree's window in the project's session",
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
                    ["list-windows", "-t", "=api", "-F", "#{window_name}"], check=False
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
        assert git_in(env, clone, "branch", "--list", "fresh") == ""
        assert (worktrees / "feature").is_dir()
        # Nothing was created on the workstation.
        assert git_in(env, repo_path, "branch", "--list", "fresh") == ""
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, signal.SIGTERM)
