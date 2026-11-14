"""Learning tests pinning down the Docker Sandboxes (`sbx`) behaviour the
`docker_sbx` sandbox backend relies on.

Run with WORKMUX_TEST_SBX=1, sbx logged in and its global network policy
initialized (`sbx policy init`, which these tests never run). Template tests
also need a Docker daemon; the GitHub placeholder test needs `gh` logged in.
Findings, with the question numbers used below, live in the sbx-backend
ticket 01's `## Answer`.
"""

import ast
import fcntl
import json
import os
import pty
import select
import signal
import socket
import struct
import subprocess
import termios
import threading
import time
from pathlib import Path
from typing import Generator, Iterator

import pytest

from .support.sbx import (
    Sandbox,
    create_sandbox,
    load_learning_template,
    new_sandbox_name,
    requires_docker,
    requires_sbx,
    sbx,
    sbx_json,
    list_sandboxes,
)

pytestmark = requires_sbx


@pytest.fixture(scope="module")
def workspace(tmp_path_factory: pytest.TempPathFactory) -> Path:
    # Resolve /var -> /private/var: sbx mounts workspaces at their real path.
    path = tmp_path_factory.mktemp("sbx-ws").resolve()
    (path / "hello.txt").write_text("hello\n")
    return path


@pytest.fixture(scope="module")
def sandbox(workspace: Path) -> Generator[Sandbox, None, None]:
    sandbox, created = create_sandbox("shared", [str(workspace)])
    try:
        assert created.returncode == 0, created.stderr
        yield sandbox
    finally:
        sandbox.remove()


# --- Prerequisites, listing and names (Q5) -----------------------------------


def test_ls_json_reports_name_status_agent_and_workspaces(
    sandbox: Sandbox, workspace: Path
):
    sandbox.sh_ok("true")
    entry = list_sandboxes()[sandbox.name]
    assert entry["agent"] == "shell"
    assert entry["status"] == "running"
    assert entry["workspaces"] == [str(workspace)]
    assert set(entry) >= {"name", "id", "status", "workspaces", "created_at"}
    assert sandbox.name in sbx(["ls", "-q"]).stdout.split()


def test_stopped_sandbox_lists_as_stopped_and_exec_restarts_it(sandbox: Sandbox):
    sandbox.sh_ok("echo kept > ~/marker")
    assert sandbox.stop().returncode == 0
    assert sandbox.status() == "stopped"

    result = sandbox.sh("cat ~/marker")
    assert result.returncode == 0
    assert result.stdout == "kept\n", "home survives a stop"
    assert "started" in result.stderr, "the restart notice goes to stderr"
    assert sandbox.status() == "running"


def test_idle_sandbox_stops_itself_after_its_last_exec(sandbox: Sandbox):
    sandbox.sh_ok("true")
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline and sandbox.status() == "running":
        time.sleep(5)
    assert sandbox.status() == "stopped"


def test_there_is_no_start_command():
    result = sbx(["start", "wm-learn-none"])
    assert result.returncode != 0
    assert "unknown command" in result.stderr


def test_missing_sandbox_is_absent_from_ls_and_exec_exits_125():
    name = new_sandbox_name("missing")
    assert name not in list_sandboxes()
    result = sbx(["exec", name, "true"])
    assert result.returncode == 125
    assert "not found" in result.stderr
    assert sbx(["stop", name]).returncode == 1
    assert sbx(["rm", "-f", name]).returncode == 1


@pytest.mark.parametrize(
    "name, message",
    [
        ("wm_learn_bad", "cannot contain underscores"),
        ("-wm-learn", "must match regexp ^[a-zA-Z0-9][a-zA-Z0-9.-]+$"),
        ("w", "must match regexp"),
        ("default", "cannot be 'default'"),
        ("wm-learn-" + "x" * 55, "cannot exceed 63 characters"),
    ],
)
def test_sandbox_name_rules(workspace: Path, name: str, message: str):
    result = sbx(["create", "--name", name, "--no-progress", "shell", str(workspace)])
    try:
        assert result.returncode != 0
        assert message in result.stderr
    finally:
        if name.startswith("wm-learn-") and result.returncode == 0:
            Sandbox(name).remove()


# --- Guest to host TCP (Q1) and per-sandbox rules (Q14) ----------------------


@pytest.fixture
def host_listener() -> Iterator[int]:
    """A TCP server on the workstation's loopback that greets each client."""
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(8)
    server.settimeout(0.5)
    port = server.getsockname()[1]
    stop = threading.Event()

    def serve() -> None:
        while not stop.is_set():
            try:
                conn, _ = server.accept()
            except socket.timeout:
                continue
            with conn:
                conn.sendall(f"HELLO-{port}\n".encode())

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    try:
        yield port
    finally:
        stop.set()
        thread.join()
        server.close()


def guest_connect(sandbox: Sandbox, host: str, port: int) -> str:
    """What a raw TCP client in the guest reads from host:port ('' when cut off)."""
    script = f"""import socket
try:
    s = socket.create_connection(({host!r}, {port}), 5)
    s.settimeout(5)
    print(repr(s.recv(64).decode()))
except Exception as e:
    print(repr("ERR " + type(e).__name__))
"""
    result = sbx(sandbox.exec_args(["python3", "-c", script])[1:])
    assert result.returncode == 0, result.stderr
    return ast.literal_eval(result.stdout.strip().splitlines()[-1])


def wait_for(predicate, timeout: float = 20) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(1)
    return predicate()


def test_host_docker_internal_resolves_to_the_host_gateway(sandbox: Sandbox):
    hosts = sandbox.sh_ok("getent ahostsv4 host.docker.internal")
    assert "169.254.1.1" in hosts
    env = sandbox.sh_ok('echo "$HTTPS_PROXY"; echo "$NO_PROXY"')
    assert "http://gateway.docker.internal:3128" in env


def test_guest_to_host_tcp_needs_a_localhost_port_rule(
    sandbox: Sandbox, host_listener: int
):
    port = host_listener
    greeting = f"HELLO-{port}\n"
    assert guest_connect(sandbox, "host.docker.internal", port) == "", (
        "balanced policy accepts then drops the connection"
    )

    def logged_as_blocked_localhost() -> bool:
        blocked = sbx_json(["policy", "log", sandbox.name])["blocked"]
        return any(
            entry["sandbox_id"] == sandbox.name and entry["host"] == f"localhost:{port}"
            for entry in blocked
        )

    assert wait_for(logged_as_blocked_localhost), (
        "the policy sees host.docker.internal as localhost"
    )

    # A rule naming host.docker.internal does not match.
    assert sandbox.allow(f"host.docker.internal:{port}").returncode == 0
    try:
        time.sleep(3)
        assert guest_connect(sandbox, "host.docker.internal", port) == ""
    finally:
        assert sandbox.remove_rule(f"host.docker.internal:{port}").returncode == 0

    added = sandbox.allow(f"localhost:{port}")
    assert added.returncode == 0, added.stderr
    assert f"scope: sandbox:{sandbox.name}" in added.stdout
    try:
        assert wait_for(
            lambda: guest_connect(sandbox, "host.docker.internal", port) == greeting
        )
        assert guest_connect(sandbox, "169.254.1.1", port) == greeting
        assert guest_connect(sandbox, "gateway.docker.internal", port) != greeting, (
            "gateway.docker.internal is the proxy, not the host"
        )
    finally:
        assert sandbox.remove_rule(f"localhost:{port}").returncode == 0
    assert wait_for(lambda: guest_connect(sandbox, "host.docker.internal", port) == "")


def test_bare_localhost_rule_allows_every_host_port(
    sandbox: Sandbox, host_listener: int
):
    assert sandbox.allow("localhost").returncode == 0
    try:
        assert wait_for(
            lambda: (
                guest_connect(sandbox, "host.docker.internal", host_listener)
                == f"HELLO-{host_listener}\n"
            )
        )
    finally:
        assert sandbox.remove_rule("localhost").returncode == 0


def test_policy_ls_lists_sandbox_rules_as_json_and_removes_by_id(sandbox: Sandbox):
    assert sandbox.allow("wm-learn.example.com:443").returncode == 0
    rules = sandbox.local_rules()
    assert len(rules) == 1
    rule = rules[0]
    assert rule["resources"] == ["wm-learn.example.com:443"]
    assert rule["decision"] == "allow"
    assert rule["applies_to"] == f"sandbox:{sandbox.name}"
    assert rule["provenance"] == {"created_via": "added"}
    assert rule["editable"] is True
    assert rule["id"] in sbx(["policy", "ls", sandbox.name, "-q"]).stdout.split()

    # Global rules are listed too, with scope "global".
    all_rules = sbx_json(["policy", "ls", sandbox.name])["rules"]
    assert {r["scope"] for r in all_rules} >= {"global", f"sandbox:{sandbox.name}"}

    removed = sbx(
        ["policy", "rm", "network", "--sandbox", sandbox.name, "--id", rule["id"], "-f"]
    )
    assert removed.returncode == 0, removed.stderr
    assert sandbox.local_rules() == []


# --- Exec (Q4, Q7, Q15) ------------------------------------------------------


def test_exec_passes_stdin_without_interactive_flag(sandbox: Sandbox):
    assert sandbox.sh("cat", input="piped\n").stdout == "piped\n"


def test_exec_propagates_exit_status(sandbox: Sandbox):
    assert sandbox.sh("exit 7").returncode == 7
    missing = sbx(["exec", sandbox.name, "wm-learn-no-such-command"])
    assert missing.returncode == 127


def test_exec_env_and_workdir(sandbox: Sandbox, workspace: Path):
    out = sbx(
        sandbox.exec_args(
            ["sh", "-c", 'echo "$WM_A|$WM_B|$(pwd)"'],
            env={"WM_A": "1", "WM_B": "x y"},
            workdir="/tmp",
        )[1:]
    ).stdout
    assert out == "1|x y|/tmp\n"

    bare = subprocess.run(
        ["sbx", "exec", "-e", "WM_C", sandbox.name, "sh", "-c", 'echo "$WM_C"'],
        capture_output=True,
        text=True,
        env={**os.environ, "WM_C": "from-host"},
    )
    assert bare.stdout == "from-host\n", "a bare -e KEY takes the caller's value"

    assert sandbox.sh_ok("pwd").strip() == str(workspace), (
        "default workdir is the first workspace"
    )
    assert sbx(["exec", "-w", "/nonexistent", sandbox.name, "true"]).returncode == 127


def test_exec_as_root(sandbox: Sandbox):
    # v0.50.1 set HOME=/root; since v0.51.0 root keeps the agent's HOME.
    assert sandbox.sh_ok("id -u; echo $HOME", user="root").split() == [
        "0",
        "/home/agent",
    ]
    assert sandbox.sh_ok("id -un; echo $HOME").split() == ["agent", "/home/agent"]
    assert sandbox.sh("sudo -n true").returncode == 0, (
        "the stock shell template has sudo"
    )


def run_in_pty(argv: list[str], rows: int, cols: int) -> tuple[subprocess.Popen, int]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    process = subprocess.Popen(
        argv, stdin=slave, stdout=slave, stderr=slave, start_new_session=True
    )
    os.close(slave)
    return process, master


def read_pty_until(master: int, needle: str, timeout: float) -> str:
    output = ""
    deadline = time.monotonic() + timeout
    while needle not in output and time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.5)
        if ready:
            try:
                output += os.read(master, 4096).decode(errors="replace")
            except OSError:
                break
    return output


def test_tty_exec_gets_size_and_resize(sandbox: Sandbox):
    script = (
        "trap 'echo \"size=$(stty size)\"' WINCH; "
        'echo "start=$(stty size) $(tty)"; echo READY; while :; do sleep 0.2; done'
    )
    process, master = run_in_pty(
        sandbox.exec_args(["bash", "-c", script], tty=True, interactive=True),
        rows=33,
        cols=111,
    )
    try:
        output = read_pty_until(master, "READY", timeout=60)
        assert "start=33 111 /dev/pts/" in output
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
        os.killpg(process.pid, signal.SIGWINCH)
        assert "size=40 100" in read_pty_until(master, "size=40 100", timeout=10)
    finally:
        process.kill()
        process.wait()
        os.close(master)


def test_two_execs_run_concurrently(sandbox: Sandbox):
    flag = f"/tmp/wm-learn-{os.getpid()}"
    waiter = subprocess.Popen(
        sandbox.exec_args(
            ["sh", "-c", f"while [ ! -e {flag} ]; do sleep 0.2; done; echo first-done"]
        ),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        time.sleep(3)
        assert waiter.poll() is None
        sandbox.sh_ok(f"touch {flag}")
        out, _ = waiter.communicate(timeout=30)
        assert out == "first-done\n"
    finally:
        waiter.kill()
        sandbox.sh(f"rm -f {flag}")


# --- Lifecycle: closing panes, stop and remove (ticket 03) -------------------


def guest_has_process(sandbox: Sandbox, marker: str) -> bool:
    # "[w]m-..." keeps the pattern from matching the shell running pgrep.
    return sandbox.sh(f"pgrep -f '[{marker[0]}]{marker[1:]}'").returncode == 0


def hangup_script(session: str) -> str:
    """The guest side of workmux's hangup (docker_sbx `hangup_command`)."""
    return (
        "for p in /proc/[0-9]*; do "
        f'grep -qxzF WM_SBX_SESSION={session} "$p/environ" 2>/dev/null '
        '&& kill -HUP "${p#/proc/}"; done; true'
    )


def test_hanging_up_a_tty_exec_leaves_its_guest_command_running(sandbox: Sandbox):
    """Closing a tmux window hangs up the pane's `sbx exec -it`: the client
    exits, but sbx keeps the guest command (and its pty) alive. A hangup of
    the processes carrying the exec's session marker ends only that session."""
    closed = f"wm-learn-closed-{os.getpid()}"
    kept = f"wm-learn-kept-{os.getpid()}"
    pane, master = run_in_pty(
        sandbox.exec_args(
            ["bash", "-c", f"echo READY; exec -a {closed} sleep 300"],
            tty=True,
            interactive=True,
            env={"WM_SBX_SESSION": "closed"},
        ),
        rows=24,
        cols=80,
    )
    other = subprocess.Popen(
        sandbox.exec_args(
            ["bash", "-c", f"exec -a {kept} sleep 300"], env={"WM_SBX_SESSION": "kept"}
        ),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        assert "READY" in read_pty_until(master, "READY", timeout=60)
        assert wait_for(lambda: guest_has_process(sandbox, kept))
        assert guest_has_process(sandbox, closed)

        os.killpg(pane.pid, signal.SIGHUP)
        pane.wait(timeout=30)
        time.sleep(5)
        assert guest_has_process(sandbox, closed), "sbx keeps the guest command"

        sandbox.sh_ok(hangup_script("closed"))
        assert wait_for(lambda: not guest_has_process(sandbox, closed))
        assert other.poll() is None
        assert guest_has_process(sandbox, kept)
    finally:
        pane.kill()
        pane.wait()
        os.close(master)
        other.kill()
        other.wait()
        sandbox.sh(hangup_script("closed"))
        sandbox.sh(hangup_script("kept"))


def test_concurrent_execs_both_start_a_stopped_sandbox(sandbox: Sandbox):
    """Panes of a worktree opening at once after sbx stopped its idle sandbox."""
    assert sandbox.stop().returncode == 0
    assert sandbox.status() == "stopped"
    panes = [
        subprocess.Popen(
            sandbox.exec_args(["echo", f"pane-{i}"]),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        for i in range(2)
    ]
    outputs = [pane.communicate(timeout=120) for pane in panes]
    assert [pane.returncode for pane in panes] == [0, 0], outputs
    assert [out for out, _ in outputs] == ["pane-0\n", "pane-1\n"]
    assert sandbox.status() == "running"


def test_stop_is_idempotent(sandbox: Sandbox):
    sandbox.sh_ok("true")
    for _ in range(2):
        stopped = sandbox.stop()
        assert stopped.returncode == 0, stopped.stderr
        assert stopped.stdout == f"Stopped sandbox {sandbox.name}\n"
        assert sandbox.status() == "stopped"


def test_rm_deletes_the_sandboxs_rules(workspace: Path):
    removed, created = create_sandbox("rm-rules", [str(workspace)])
    try:
        assert created.returncode == 0, created.stderr
        assert removed.allow("localhost").returncode == 0
        assert len(removed.local_rules()) == 1
        result = sbx(["rm", "-f", removed.name], timeout=300)
        assert result.returncode == 0, result.stderr
        assert removed.name not in list_sandboxes()
        rules = sbx_json(["policy", "ls"])["rules"]
        assert [r for r in rules if r["scope"] == f"sandbox:{removed.name}"] == []
    finally:
        removed.remove()


# --- Mounts (Q2, Q3, Q11, Q12) -----------------------------------------------


def git(*args: str, cwd: Path) -> str:
    return subprocess.run(
        ["git", "-c", "user.name=wm", "-c", "user.email=wm@example.invalid", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    ).stdout


@pytest.fixture(scope="module")
def repo(tmp_path_factory: pytest.TempPathFactory) -> Path:
    """A main repository with a linked worktree beside it, workmux-style."""
    root = tmp_path_factory.mktemp("sbx-repo").resolve()
    git("init", "-q", "-b", "main", "repo", cwd=root)
    main = root / "repo"
    git("commit", "-q", "--allow-empty", "-m", "init", cwd=main)
    git("worktree", "add", "-q", "-b", "feat", "../repo__worktrees/feat", cwd=main)
    worktree = root / "repo__worktrees" / "feat"
    (worktree / ".env").write_text("SECRET=1\n")
    (root / "single.txt").write_text("single\n")
    return root


@pytest.fixture(scope="module")
def mounted(repo: Path) -> Generator[Sandbox, None, None]:
    worktree = repo / "repo__worktrees" / "feat"
    git_dir = repo / "repo" / ".git"
    sandbox, created = create_sandbox(
        "mnt",
        [
            str(worktree),
            str(git_dir),
            f"{git_dir}/hooks:ro",
            f"{git_dir}/config:ro",
            f"{worktree}/.env:ro",
            f"{repo}/single.txt:ro",
        ],
    )
    try:
        assert created.returncode == 0, created.stderr
        yield sandbox
    finally:
        sandbox.remove()


def test_linked_worktree_with_main_git_dir_commits_at_workstation_paths(
    mounted: Sandbox, repo: Path
):
    worktree = repo / "repo__worktrees" / "feat"
    out = mounted.sh_ok(
        "pwd; git status -sb | head -1; echo x > guest.txt; git add guest.txt; "
        "git -c user.name=wm -c user.email=wm@example.invalid commit -q -m guest"
    )
    assert out.splitlines()[:2] == [str(worktree), "## feat"]
    assert git("log", "-1", "--format=%s", cwd=worktree) == "guest\n"


def test_single_file_mount_must_be_read_only(repo: Path):
    name = new_sandbox_name("file")
    result = sbx(
        ["create", "--name", name, "--no-progress", "shell", f"{repo}/single.txt"]
    )
    try:
        assert result.returncode != 0
        assert "is not a directory" in result.stderr
    finally:
        if result.returncode == 0:
            Sandbox(name).remove()


def test_read_only_file_mount_is_readable_not_writable(mounted: Sandbox, repo: Path):
    out = mounted.sh(f"cat {repo}/single.txt; echo y >> {repo}/single.txt")
    assert out.stdout == "single\n"
    assert "Read-only file system" in out.stderr


@pytest.mark.parametrize("user", [None, "root"])
def test_nested_read_only_mounts_hold_even_for_root(mounted: Sandbox, repo: Path, user):
    git_dir = repo / "repo" / ".git"
    worktree = repo / "repo__worktrees" / "feat"
    script = f"""
        touch {git_dir}/hooks/pre-commit 2>/dev/null; echo hooks=$?
        echo '#' >> {git_dir}/config 2>/dev/null; echo config=$?
        echo z >> {worktree}/.env 2>/dev/null; echo env=$?
        rm -f {worktree}/.env 2>/dev/null; echo env_rm=$?
        touch {git_dir}/objects/wm-probe && rm {git_dir}/objects/wm-probe; echo objects=$?
        cat {worktree}/.env
    """
    out = mounted.sh_ok(script, user=user).split()
    assert out == [
        "hooks=1",
        "config=1",
        "env=1",
        "env_rm=1",
        "objects=0",
        "SECRET=1",
    ], "a :ro file inside a writable mount is read-only but its content is visible"


def test_root_unmounting_a_read_only_mount_does_not_make_it_writable(
    mounted: Sandbox, repo: Path
):
    hooks = repo / "repo" / ".git" / "hooks"
    out = mounted.sh_ok(
        f"umount {hooks} 2>/dev/null; touch {hooks}/x 2>/dev/null; echo rc=$?",
        user="root",
    )
    assert out.strip() == "rc=1"
    assert not (hooks / "x").exists()


def test_file_ownership_maps_between_guest_agent_and_host_user(
    mounted: Sandbox, repo: Path
):
    worktree = repo / "repo__worktrees" / "feat"
    (worktree / "host.png").write_bytes(b"\x89PNG-host")
    out = mounted.sh_ok(
        f"touch {worktree}/guest-made; stat -c '%u:%g' {worktree}/guest-made "
        f"{worktree}/host.png; od -An -c {worktree}/host.png | tr -d ' \\n'"
    ).split()
    assert out[:2] == ["1000:1000", "1000:1000"], "the guest sees everything as agent"
    assert out[2] == "211PNG-host"
    stat = (worktree / "guest-made").stat()
    assert stat.st_uid == os.getuid(), "guest writes land as the workstation user"


def test_host_config_dir_reaches_agent_home_through_a_symlink(
    tmp_path_factory: pytest.TempPathFactory,
):
    """Workspaces mount only at their host path; a root symlink exposes one in ~."""
    config = tmp_path_factory.mktemp("sbx-cfg").resolve()
    (config / "settings.json").write_text("{}\n")
    sandbox, created = create_sandbox("cfg", [str(config)])
    try:
        assert created.returncode == 0, created.stderr
        sandbox.sh_ok(f"ln -sfn {config} /home/agent/.wmlearn", user="root")
        out = sandbox.sh_ok(
            "cat ~/.wmlearn/settings.json; echo guest > ~/.wmlearn/written"
        )
        assert out == "{}\n"
        assert (config / "written").read_text() == "guest\n"
    finally:
        sandbox.remove()


# --- Resources (Q13) ---------------------------------------------------------


def test_cpus_and_memory_flags(workspace: Path):
    sized, created = create_sandbox(
        "res", [str(workspace)], extra=["--cpus", "2", "-m", "1g"]
    )
    try:
        assert created.returncode == 0, created.stderr
        nproc, mem_mib = sized.sh_ok("nproc; free -m | awk '/Mem:/{print $2}'").split()
        assert nproc == "2"
        assert 900 <= int(mem_mib) <= 1024
    finally:
        sized.remove()


def test_memory_below_minimum_is_rejected(workspace: Path):
    name = new_sandbox_name("mem")
    result = sbx(
        [
            "create",
            "--name",
            name,
            "--no-progress",
            "-m",
            "100m",
            "shell",
            str(workspace),
        ]
    )
    try:
        assert result.returncode != 0
        assert "below the minimum of 512 MiB" in result.stderr
    finally:
        if result.returncode == 0:
            Sandbox(name).remove()


def test_create_has_no_disk_flag():
    help_text = sbx(["create", "-h"]).stdout
    assert "--cpus" in help_text and "--memory" in help_text
    assert "--disk" not in help_text


# --- Secrets and placeholders (Q6, Q9, Q16) ----------------------------------


def test_secret_ls_json_lists_services_without_values():
    listing = sbx_json(["secret", "ls"])
    assert set(listing) >= {"secrets", "custom_secrets"}
    for secret in listing["secrets"]:
        assert set(secret) == {"scope", "type", "name", "secret"}
        assert secret["secret"].startswith("("), "only a status such as (stored)"


def test_secret_ls_for_an_unknown_sandbox_is_empty():
    listing = sbx_json(["secret", "ls", "--sandbox", new_sandbox_name("none")])
    assert listing["secrets"] == []


def test_service_list_covers_agent_providers():
    help_text = sbx(["secret", "set", "-h"]).stdout
    services = help_text.split("Available services:")[1].splitlines()[0]
    names = {name.strip() for name in services.split(",")}
    assert {"anthropic", "openai", "google", "copilot", "github"} <= names


def test_guest_sees_placeholder_environment(sandbox: Sandbox):
    out = sandbox.sh_ok(
        'printf "[%s]\\n" "${GH_TOKEN:0:12}" "$ANTHROPIC_API_KEY" "$SBX_CRED_GITHUB_MODE"'
    ).split()
    # v0.50.1 also set ANTHROPIC_API_KEY/OPENAI_API_KEY=proxy-managed and
    # SBX_CRED_<SERVICE>_MODE in every sandbox; v0.51.0's shell sandboxes
    # set only the GitHub placeholder.
    assert out == ["[gho_sbxproxy]", "[]", "[]"]


def gh_logged_in() -> bool:
    try:
        return (
            subprocess.run(["gh", "auth", "status"], capture_output=True).returncode
            == 0
        )
    except FileNotFoundError:
        return False


@pytest.mark.skipif(not gh_logged_in(), reason="needs gh logged in on the workstation")
def test_github_secret_from_command_reaches_the_api_as_placeholder(sandbox: Sandbox):
    def status() -> str:
        return sandbox.sh_ok(
            'curl -s -o /dev/null -w "%{http_code}" '
            '-H "Authorization: Bearer $GH_TOKEN" https://api.github.com/user'
        )

    assert status() == "401"
    stored = sbx(
        [
            "secret",
            "set",
            "github",
            "--sandbox",
            sandbox.name,
            "--command",
            "gh auth token",
        ]
    )
    assert stored.returncode == 0, stored.stderr
    try:
        listed = sbx_json(["secret", "ls", "--sandbox", sandbox.name])["secrets"]
        assert listed == [
            {
                "scope": sandbox.name,
                "type": "service",
                "name": "github",
                "secret": "(stored)",
            }
        ]
        assert wait_for(lambda: status() == "200"), "applied to the running sandbox"
        token = sandbox.sh_ok('printf %s "$GH_TOKEN"')
        assert token.startswith("gho_sbxproxy"), "the guest still holds the placeholder"
    finally:
        sbx(["secret", "rm", "github", "--sandbox", sandbox.name, "-f"])
    assert wait_for(lambda: status() == "401")


# --- workmux image as template (Q10, Q16) ------------------------------------


@pytest.fixture(scope="module")
def template() -> Generator[str, None, None]:
    tag = load_learning_template()
    try:
        yield tag
    finally:
        sbx(["template", "rm", "-f", tag])


@requires_docker
def test_workmux_image_with_thin_agent_layer_is_a_template(
    template: str, workspace: Path
):
    tmpl, created = create_sandbox(
        "tmpl",
        [str(workspace)],
        agent="claude",
        extra=["-t", template, "--pull", "never"],
    )
    try:
        assert created.returncode == 0, created.stderr
        out = tmpl.sh_ok("id -un; id -u; echo $HOME; pwd; claude --version")
        user, uid, home, cwd, version = out.splitlines()[:5]
        assert (user, uid, home, cwd) == (
            "agent",
            "1000",
            "/home/agent",
            str(workspace),
        )
        assert "Claude Code" in version
        assert tmpl.sh_ok("id -u", user="root").strip() == "0", (
            "exec -u root works without sudo in the image"
        )

        # sbx's claude kit writes the OAuth credential file in the template home,
        # holding no real token.
        creds = json.loads(tmpl.sh_ok("cat ~/.claude/.credentials.json"))
        oauth = creds["claudeAiOauth"]
        assert oauth["accessToken"] in ("", "sk-ant-oat01-proxy-managed")
        assert oauth["refreshToken"] in ("", "sk-ant-ort01-proxy-managed")
    finally:
        tmpl.remove()


@requires_docker
def test_claude_kit_no_longer_needs_an_agent_owned_claude_dir(workspace: Path):
    """v0.50.1 failed to create a claude sandbox from an image without an
    agent-owned ~/.claude (500, "failed to apply kit to sandbox"). Since
    v0.51.0 the claude agent ships as a kit (docker.io/sbx/claude) and
    create succeeds."""
    tag = "wm-learn-sbx-bare:latest"
    subprocess.run(
        [
            "docker",
            "build",
            "-q",
            "-t",
            tag,
            "-",
        ],
        input=(
            "FROM ghcr.io/raine/workmux-sandbox:claude\n"
            "RUN useradd -m -u 1000 -s /bin/bash agent "
            "&& mkdir -p /usr/local/share/ca-certificates "
            "&& touch /etc/sandbox-persistent.sh\n"
            "ENV BASH_ENV=/etc/sandbox-persistent.sh\nUSER agent\nWORKDIR /home/agent\n"
        ),
        text=True,
        check=True,
        capture_output=True,
    )
    tar = workspace / "bare.tar"
    subprocess.run(["docker", "save", tag, "-o", str(tar)], check=True)
    try:
        assert sbx(["template", "load", str(tar)], timeout=600).returncode == 0
        bare, created = create_sandbox(
            "bare",
            [str(workspace)],
            agent="claude",
            extra=["-t", tag, "--pull", "never"],
        )
        try:
            assert created.returncode == 0, created.stderr
            assert "now ships as a kit" in created.stderr
        finally:
            bare.remove()
    finally:
        tar.unlink(missing_ok=True)
        sbx(["template", "rm", "-f", tag])
