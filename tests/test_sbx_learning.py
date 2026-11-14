"""Learning tests pinning down the Docker Sandboxes (`sbx`) behaviour the
`docker_sbx` sandbox backend relies on.

Run with WORKMUX_TEST_SBX=1, sbx logged in and its global network policy
initialized (`sbx policy init`, which these tests never run). Template tests
also need a Docker daemon; the GitHub placeholder test needs `gh` logged in.
Findings, with the question numbers used below, live in the sbx-backend
ticket 01's `## Answer`.
"""

import ast
import contextlib
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


# --- Network policy rules (ticket 06) ----------------------------------------
# Observed under the global `balanced` policy.


def guest_https_status(sandbox: Sandbox, url: str) -> str:
    """The HTTP status a guest curl gets through sbx's proxy (403 when blocked)."""
    return sandbox.sh_ok(
        f"curl -s -o /dev/null -w '%{{http_code}}' --max-time 15 {url}"
    )


def test_domain_rule_on_port_443_allows_only_that_port(sandbox: Sandbox):
    # example.com is not in the balanced policy's allow list.
    assert guest_https_status(sandbox, "https://example.com") == "403"
    assert sandbox.allow("example.com:443").returncode == 0
    try:
        assert wait_for(
            lambda: guest_https_status(sandbox, "https://example.com") == "200"
        )
        assert guest_https_status(sandbox, "http://example.com") == "403"
    finally:
        assert sandbox.remove_rule("example.com:443").returncode == 0


def test_wildcard_forms_one_label_or_any_depth_with_the_base(sandbox: Sandbox):
    base = "wm-learn.example.org"
    assert sandbox.allow(f"*.one.{base}:443").returncode == 0
    assert sandbox.allow(f"**.any.{base}:443").returncode == 0
    try:
        assert sandbox.allows(f"a.one.{base}")
        assert not sandbox.allows(f"a.b.one.{base}"), "* matches one label"
        assert not sandbox.allows(f"one.{base}"), "* does not match the base"
        assert sandbox.allows(f"a.b.any.{base}"), "** matches any depth"
        assert sandbox.allows(f"any.{base}"), "** also matches the base"
        assert not sandbox.allows(f"a.any.{base}:80")

        # A deny for the base takes it out again, leaving the subdomains.
        assert sandbox.deny(f"any.{base}:443").returncode == 0
        assert not sandbox.allows(f"any.{base}")
        assert sandbox.allows(f"a.any.{base}")
    finally:
        for resource in [f"*.one.{base}:443", f"**.any.{base}:443", f"any.{base}:443"]:
            sandbox.remove_rule(resource)


def test_sandbox_deny_beats_a_global_allow(sandbox: Sandbox):
    assert sandbox.allows("github.com"), "balanced allows github.com:443"
    assert sandbox.deny("github.com:443").returncode == 0
    try:
        assert not sandbox.allows("github.com")
        assert sandbox.allows("api.github.com")
    finally:
        assert sandbox.remove_rule("github.com:443").returncode == 0


def test_sandbox_deny_everything_also_beats_sandbox_allows(sandbox: Sandbox):
    assert sandbox.allow("wm-learn.example.com:443").returncode == 0
    assert sandbox.deny("**").returncode == 0
    try:
        assert not sandbox.allows("wm-learn.example.com")
    finally:
        sandbox.remove_rule("**")
        sandbox.remove_rule("wm-learn.example.com:443")


def test_adding_an_existing_rule_is_a_no_op_and_removing_a_missing_one_fails(
    sandbox: Sandbox,
):
    assert sandbox.allow("wm-learn.example.com:443").returncode == 0
    try:
        again = sandbox.allow("wm-learn.example.com:443")
        assert again.returncode == 0, again.stderr
        assert "Already covered" in again.stdout
        assert len(sandbox.local_rules()) == 1

        opposite = sandbox.deny("wm-learn.example.com:443")
        assert opposite.returncode != 0
        assert "conflicts with existing allow rule" in opposite.stderr
    finally:
        assert sandbox.remove_rule("wm-learn.example.com:443").returncode == 0
    missing = sandbox.remove_rule("wm-learn.example.com:443")
    assert missing.returncode == 1
    assert "rule not found" in missing.stderr


def nip_io_resolves() -> bool:
    try:
        return socket.gethostbyname("127.0.0.1.nip.io") == "127.0.0.1"
    except OSError:
        return False


@pytest.mark.skipif(not nip_io_resolves(), reason="needs DNS for nip.io")
def test_domain_rule_reaches_the_workstation_loopback_its_dns_points_at(
    sandbox: Sandbox, host_listener: int
):
    # Unlike workmux's CONNECT proxy, sbx doesn't refuse private or loopback
    # addresses an allowed domain resolves to.
    host = "127.0.0.1.nip.io"
    script = f"""import socket, time
s = socket.create_connection(("gateway.docker.internal", 3128), 5)
s.settimeout(5)
s.sendall(b"CONNECT {host}:{host_listener} HTTP/1.1\\r\\nHost: {host}\\r\\n\\r\\n")
data = b""
try:
    while True:
        chunk = s.recv(256)
        if not chunk:
            break
        data += chunk
except OSError:
    pass
print(repr(data.decode(errors="replace")))
"""
    resource = f"{host}:{host_listener}"
    assert sandbox.allow(resource).returncode == 0
    try:
        time.sleep(3)
        result = sbx(sandbox.exec_args(["python3", "-c", script])[1:])
        assert result.returncode == 0, result.stderr
        assert f"HELLO-{host_listener}" in result.stdout
    finally:
        assert sandbox.remove_rule(resource).returncode == 0


# --- Exec (Q4, Q7, Q15) ------------------------------------------------------


def test_exec_passes_stdin_without_interactive_flag(sandbox: Sandbox):
    assert sandbox.sh("cat", input="piped\n").stdout == "piped\n"


def test_exec_propagates_exit_status(sandbox: Sandbox):
    assert sandbox.sh("exit 7").returncode == 7
    missing = sbx(["exec", sandbox.name, "wm-learn-no-such-command"])
    assert missing.returncode == 127


def test_exec_asks_for_a_tty_only_when_stdin_is_a_terminal(sandbox: Sandbox):
    # `-it` with piped stdin fails after a timeout; `-i` alone passes piped
    # stdin and the exit status.
    no_terminal = sbx(
        sandbox.exec_args(["cat"], tty=True, interactive=True)[1:], input="piped\n"
    )
    assert no_terminal.returncode == 125
    assert "context deadline exceeded" in no_terminal.stderr
    piped = sbx(
        sandbox.exec_args(["bash", "-c", "cat; exit 3"], interactive=True)[1:],
        input="piped\n",
    )
    assert piped.returncode == 3
    assert piped.stdout.endswith("piped\n")


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


def test_writable_mounts_nested_in_a_read_only_git_dir_let_commits_through(
    repo: Path,
):
    """The container backend's git-metadata layout: the main git dir read-only,
    the parts commits write (objects, refs, logs, the worktree's admin dir)
    writable inside it, and its files workmux protects read-only again. sbx
    mounts parents before children whatever order the workspaces are listed in."""
    git_dir = repo / "repo" / ".git"
    worktree = repo / "repo__worktrees" / "feat"
    admin = git_dir / "worktrees" / "feat"
    sandbox, created = create_sandbox(
        "gitro",
        [
            str(worktree),
            str(git_dir / "objects"),
            str(git_dir / "refs"),
            str(git_dir / "logs"),
            str(admin),
            f"{git_dir}/objects/info:ro",
            f"{admin}/gitdir:ro",
            f"{worktree}/.git:ro",
            f"{git_dir}:ro",
        ],
    )
    try:
        assert created.returncode == 0, created.stderr
        script = f"""
            touch {git_dir}/hooks/pre-commit 2>/dev/null; echo hooks=$?
            touch {git_dir}/wm-probe 2>/dev/null; echo git_dir=$?
            touch {git_dir}/objects/info/wm-probe 2>/dev/null; echo objects_info=$?
            echo x >> {admin}/gitdir 2>/dev/null; echo gitdir=$?
            echo x >> {worktree}/.git 2>/dev/null; echo pointer=$?
            mkdir {git_dir}/worktrees/other 2>/dev/null; echo other_admin=$?
        """
        for user in [None, "root"]:
            out = sandbox.sh_ok(script, user=user).split()
            assert out == [
                "hooks=1",
                "git_dir=1",
                "objects_info=1",
                "gitdir=1",
                "pointer=1",
                "other_admin=1",
            ], user
        sandbox.sh_ok(
            "echo y > nested.txt && git add nested.txt && git -c user.name=wm "
            "-c user.email=wm@example.invalid commit -q -m nested"
        )
        assert git("log", "-1", "--format=%s", cwd=worktree) == "nested\n"
    finally:
        sandbox.remove()


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


@pytest.mark.parametrize("memory", ["1GiB", "1gb", "1.5g", "536870912"])
def test_memory_takes_binary_sizes_in_several_spellings(workspace: Path, memory: str):
    """`sandbox.docker_sbx.memory` is checked at config load against these."""
    sized, created = create_sandbox("memfmt", [str(workspace)], extra=["-m", memory])
    try:
        assert created.returncode == 0, created.stderr
    finally:
        sized.remove()


def test_malformed_memory_is_rejected(workspace: Path):
    name = new_sandbox_name("memfmt")
    result = sbx(
        [
            "create",
            "--name",
            name,
            "--no-progress",
            "-m",
            "abc",
            "shell",
            str(workspace),
        ]
    )
    try:
        assert result.returncode != 0
        assert "invalid size: 'abc'" in result.stderr
    finally:
        if result.returncode == 0:
            Sandbox(name).remove()


# --- Provisioning and kits (ticket 10) ---------------------------------------


@contextlib.contextmanager
def removing_assembled_templates() -> Iterator[None]:
    """Remove the templates sbx assembles for kit compositions made inside."""

    def assembled() -> set[str]:
        return {
            f"{t['repository']}:{t['tag']}"
            for t in sbx_json(["template", "ls"])
            if t["repository"].endswith("/sandbox-kit-assembled")
        }

    before = assembled()
    try:
        yield
    finally:
        for tag in assembled() - before:
            sbx(["template", "rm", "-f", tag])


def test_root_exec_reaches_the_network_through_sbx_proxy(sandbox: Sandbox):
    """Provisioning runs as root and downloads through sbx's proxy."""
    out = sandbox.sh_ok(
        "echo $HTTPS_PROXY; curl -s -o /dev/null -w '%{http_code}' https://deb.debian.org/",
        user="root",
    )
    proxy, status = out.split()
    assert proxy == "http://gateway.docker.internal:3128"
    assert status in ("200", "301", "302")


@requires_docker
def test_mixin_kit_composes_onto_a_loaded_template(template: str, workspace: Path):
    """A mixin's overlay lands on workmux's own template, with --pull never."""
    with removing_assembled_templates():
        kitted, created = create_sandbox(
            "kit",
            [str(workspace)],
            extra=[
                "-t",
                template,
                "--pull",
                "never",
                "--kit",
                "docker.io/sbx/task:latest",
            ],
        )
        try:
            assert created.returncode == 0, created.stderr
            out = kitted.sh_ok("task --version; command -v claude; id -un")
            assert out.split() == ["3.50.0", "/usr/local/bin/claude", "agent"]
        finally:
            kitted.remove()


def test_kit_network_rules_are_scoped_to_the_sandbox(workspace: Path):
    """A kit's rules widen only its sandbox's access, and go with it."""
    with removing_assembled_templates():
        kitted, created = create_sandbox(
            "kitnet", [str(workspace)], extra=["--kit", "docker.io/sbx/vale:latest"]
        )
        try:
            assert created.returncode == 0, created.stderr
            rules = kitted.local_rules()
            assert [
                (r["decision"], sorted(r["resources"]), r["provenance"]["created_via"])
                for r in rules
            ] == [
                (
                    "allow",
                    [
                        "github.com",
                        "objects.githubusercontent.com",
                        "release-assets.githubusercontent.com",
                    ],
                    "provisioned",
                )
            ]
        finally:
            kitted.remove()
        global_rules = json.dumps(sbx_json(["policy", "ls"])["rules"])
        assert kitted.name not in global_rules


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
def test_root_bind_mount_keeps_guest_writes_off_the_host_until_stop(
    template: str, tmp_path_factory: pytest.TempPathFactory
):
    """A guest-local dir bound over a workspace subdir takes the guest's writes
    (pi's bin/); the bind ends when the sandbox stops, its contents stay.
    The stock shell image has no `mount`; workmux's image does."""
    config = tmp_path_factory.mktemp("sbx-bind").resolve()
    host_bin = config / "bin"
    sandbox, created = create_sandbox(
        "bind", [str(config)], extra=["-t", template, "--pull", "never"]
    )
    try:
        assert created.returncode == 0, created.stderr
        bind = (
            f"install -d -o agent -g agent /var/lib/wm-learn && mkdir -p {host_bin} "
            f"&& mount --bind /var/lib/wm-learn {host_bin}"
        )
        sandbox.sh_ok(bind, user="root")
        sandbox.sh_ok(f"echo linux > {host_bin}/fd")
        assert list(host_bin.iterdir()) == []

        sandbox.stop()
        out = sandbox.sh_ok(
            f"mountpoint -q {host_bin} && echo bound || echo unbound; ls /var/lib/wm-learn"
        )
        assert out.split() == ["unbound", "fd"]
    finally:
        sandbox.remove()


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
