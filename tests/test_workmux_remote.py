import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import time
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
        "export PATH=/usr/bin:/bin\n"
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


@pytest.mark.tmux_only
def test_remote_add_places_windows_in_the_project_session(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    short_tmp: dict,
):
    """`add` on a remote project creates the worktree on the remote, runs its
    post-create hooks there with output streamed back, and puts the window in
    a workstation session named after the project, whatever session the
    command was typed in. A session the user made is adopted; the `session:`
    override, current-session placement and session mode still work."""
    env = mux_server
    root = env.tmp_path / "remote-root"
    (root / "home").mkdir(parents=True)
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
    wm(f"project add web --remote lb --repo {origin} --path code/web --session frontend")
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
        env.tmux(["select-window", "-t", env.runner_pane_id])

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
