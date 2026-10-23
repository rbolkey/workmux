"""Learning tests pinning down the OpenShell behaviour the OpenShell transport relies on.

Run with WORKMUX_TEST_OPENSHELL=1 and an active OpenShell gateway. The git push
test also needs WORKMUX_TEST_OPENSHELL_GITHUB_PROVIDER (a configured `github`
provider) and WORKMUX_TEST_OPENSHELL_GITHUB_REPO (an HTTPS repository the token
may push scratch branches to). Findings live in ticket 01's `## Answer`.
"""

import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import time
import uuid
from typing import Generator, Optional

import pytest

from .support.openshell import (
    Sandbox,
    create_sandbox,
    new_sandbox_name,
    openshell,
    requires_openshell,
    workspace_dir,
)

pytestmark = requires_openshell

SENTINEL = "WORKMUX-REMOTE-HELLO"
HOLD_SECONDS = float(os.environ.get("WORKMUX_TEST_OPENSHELL_HOLD_SECONDS", "180"))


@pytest.fixture(scope="module")
def sandbox() -> Generator[Sandbox, None, None]:
    sandbox, created = create_sandbox("shared")
    try:
        assert created.returncode == 0, created.stderr
        sandbox.wait_for_phase("Ready")
        yield sandbox
    finally:
        sandbox.delete()


class LineReader:
    """Read newline-delimited stdout from a child process with a deadline per line."""

    def __init__(self, process: subprocess.Popen[bytes]):
        assert process.stdout is not None
        self.fd = process.stdout.fileno()
        self.buffer = b""

    def read_line(self, timeout: float) -> Optional[str]:
        deadline = time.monotonic() + timeout
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            ready, _, _ = select.select([self.fd], [], [], remaining)
            if not ready:
                return None
            chunk = os.read(self.fd, 4096)
            if not chunk:
                return None
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return line.decode(errors="replace").rstrip("\r")


def spawn_echo_loop(sandbox: Sandbox) -> subprocess.Popen[bytes]:
    """Start a remote process that prints the sentinel, then echoes each stdin line back."""
    script = f'echo {SENTINEL}; while IFS= read -r line; do echo "echo:$line"; done'
    return subprocess.Popen(
        sandbox.exec_args(["sh", "-c", script]),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def send_line(process: subprocess.Popen[bytes], line: str) -> None:
    assert process.stdin is not None
    process.stdin.write(f"{line}\n".encode())
    process.stdin.flush()


def read_until_sentinel(reader: LineReader) -> list[str]:
    """Return the lines that arrived before the sentinel hello line."""
    noise: list[str] = []
    while True:
        line = reader.read_line(timeout=60)
        assert line is not None, f"no sentinel; noise so far: {noise!r}"
        if line == SENTINEL:
            return noise
        noise.append(line)


def test_exec_holds_bidirectional_stdio_for_minutes(sandbox: Sandbox):
    process = spawn_echo_loop(sandbox)
    reader = LineReader(process)
    try:
        noise = read_until_sentinel(reader)
        print(f"stdout before sentinel: {noise!r}")

        deadline = time.monotonic() + HOLD_SECONDS
        worst = 0.0
        count = 0
        while time.monotonic() < deadline:
            message = f'{{"id":{count},"method":"ping"}}'
            sent = time.monotonic()
            send_line(process, message)
            assert reader.read_line(timeout=10) == f"echo:{message}"
            worst = max(worst, time.monotonic() - sent)
            count += 1
            time.sleep(5)
        print(f"{count} round trips over {HOLD_SECONDS}s, worst {worst:.3f}s")
        assert worst < 2.0
    finally:
        process.kill()
        process.wait()


def test_closing_stdin_ends_the_remote_process(sandbox: Sandbox):
    process = spawn_echo_loop(sandbox)
    reader = LineReader(process)
    read_until_sentinel(reader)
    assert process.stdin is not None
    process.stdin.close()
    assert process.wait(timeout=30) == 0


def test_killing_the_workstation_side_ends_the_remote_process(sandbox: Sandbox):
    pidfile = f"/tmp/wm-learn-{uuid.uuid4().hex[:6]}.pid"
    process = subprocess.Popen(
        sandbox.exec_args(["sh", "-c", f"echo $$ > {pidfile}; echo {SENTINEL}; cat"]),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    read_until_sentinel(LineReader(process))
    process.kill()
    process.wait()

    time.sleep(10)
    survived = sandbox.sh(f'kill -0 "$(cat {pidfile})" 2>/dev/null')
    assert survived.returncode != 0, "remote process outlived the killed exec client"


def run_in_pty(
    argv: list[str], rows: int, cols: int
) -> tuple[subprocess.Popen[bytes], int]:
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


def test_tty_exec_with_workdir_env_and_size(sandbox: Sandbox):
    argv = sandbox.exec_args(
        ["sh", "-c", 'pwd; echo "env=$WM_LEARN"; stty size; tty; echo DONE'],
        tty=True,
        workdir="/tmp",
        env={"WM_LEARN": "from-workstation"},
    )
    process, master = run_in_pty(argv, rows=33, cols=111)
    try:
        output = read_pty_until(master, "DONE", timeout=60)
        print(repr(output))
        assert "/tmp" in output
        assert "env=from-workstation" in output
        assert "33 111" in output
        assert "/dev/pts/" in output
    finally:
        process.kill()
        process.wait()
        os.close(master)


def test_tty_resize_propagates_sigwinch(sandbox: Sandbox):
    script = "trap 'echo \"size=$(stty size)\"' WINCH; echo READY; while :; do sleep 0.2; done"
    process, master = run_in_pty(
        sandbox.exec_args(["sh", "-c", script], tty=True), rows=24, cols=80
    )
    try:
        assert "READY" in read_pty_until(master, "READY", timeout=60)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
        os.killpg(process.pid, signal.SIGWINCH)
        assert "size=40 100" in read_pty_until(master, "size=40 100", timeout=10)
    finally:
        process.kill()
        process.wait()
        os.close(master)


def test_git_worktree_lifecycle_under_default_policy(sandbox: Sandbox):
    root = f"{workspace_dir(sandbox)}/wm-learn-git-{uuid.uuid4().hex[:6]}"
    sandbox.sh_ok(
        f"""set -eu
        git --version
        export GIT_AUTHOR_NAME=wm GIT_AUTHOR_EMAIL=wm@example.invalid
        export GIT_COMMITTER_NAME=wm GIT_COMMITTER_EMAIL=wm@example.invalid
        mkdir -p {root} && cd {root}
        git init -q -b main origin && git -C origin commit -q --allow-empty -m init
        git clone -q origin repo && cd repo
        git worktree add -q -b feature ../repo__worktrees/feature
        cd ../repo__worktrees/feature
        echo hello > file && git add file && git commit -q -m feature
        cd ../../repo
        git worktree list
        git worktree remove ../repo__worktrees/feature
        git branch -D feature
        test "$(git worktree list | wc -l)" -eq 1
        rm -rf {root}
        """
    )


def test_unix_socket_in_workspace(sandbox: Sandbox):
    path = f"{workspace_dir(sandbox)}/wm-learn-{uuid.uuid4().hex[:6]}.sock"
    output = sandbox.sh_ok(
        f"""python3 - <<'EOF'
import os, socket, threading
path = {path!r}
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(1)
def serve():
    conn, _ = server.accept()
    conn.sendall(conn.recv(64).upper())
    conn.close()
thread = threading.Thread(target=serve)
thread.start()
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(path)
client.sendall(b"ping")
print(client.recv(64).decode())
thread.join()
os.unlink(path)
EOF"""
    )
    assert output.strip().splitlines()[-1] == "PING"


@pytest.mark.skipif(
    not (
        os.environ.get("WORKMUX_TEST_OPENSHELL_GITHUB_PROVIDER")
        and os.environ.get("WORKMUX_TEST_OPENSHELL_GITHUB_REPO")
    ),
    reason="needs WORKMUX_TEST_OPENSHELL_GITHUB_PROVIDER and WORKMUX_TEST_OPENSHELL_GITHUB_REPO",
)
def test_https_git_push_through_github_provider():
    provider = os.environ["WORKMUX_TEST_OPENSHELL_GITHUB_PROVIDER"]
    repo = os.environ["WORKMUX_TEST_OPENSHELL_GITHUB_REPO"]
    policy = os.environ.get("WORKMUX_TEST_OPENSHELL_GITHUB_POLICY")
    pushed, created = create_sandbox("push", policy=policy, providers=[provider])
    try:
        assert created.returncode == 0, created.stderr
        pushed.wait_for_phase("Ready")
        branch = new_sandbox_name("push")
        print(
            pushed.sh(
                "env | grep -i -E 'git|gh_|token' | sed 's/=.*/=<redacted>/'; "
                "git config --show-origin --get-all credential.helper"
            ).stdout
        )
        pushed.sh_ok(
            f"""set -eu
            export GIT_AUTHOR_NAME=wm GIT_AUTHOR_EMAIL=wm@example.invalid
            export GIT_COMMITTER_NAME=wm GIT_COMMITTER_EMAIL=wm@example.invalid
            git clone -q {repo} push-repo && cd push-repo
            git switch -q -c {branch}
            git commit -q --allow-empty -m 'workmux OpenShell learning test'
            git push -q origin {branch}
            git push -q origin --delete {branch}
            """,
            timeout=300,
        )
    finally:
        pushed.delete()


def test_workspace_survives_stop_and_start():
    kept, created = create_sandbox("persist")
    try:
        assert created.returncode == 0, created.stderr
        kept.wait_for_phase("Ready")
        workspace = workspace_dir(kept)
        kept.sh_ok(
            f"""set -eu
            export GIT_AUTHOR_NAME=wm GIT_AUTHOR_EMAIL=wm@example.invalid
            export GIT_COMMITTER_NAME=wm GIT_COMMITTER_EMAIL=wm@example.invalid
            cd {workspace} && git init -q -b main repo && cd repo
            git commit -q --allow-empty -m init
            git worktree add -q -b feature ../repo__worktrees/feature
            echo kept > ../repo__worktrees/feature/marker
            echo kept > /tmp/wm-learn-marker
            ls -d /nix 2>/dev/null || true
            """
        )

        assert kept.stop().returncode == 0
        kept.wait_for_phase("Stopped")
        assert kept.start().returncode == 0
        kept.wait_for_phase("Ready")

        assert (
            kept.sh_ok(f"cat {workspace}/repo__worktrees/feature/marker").strip()
            == "kept"
        )
        assert "feature" in kept.sh_ok(f"git -C {workspace}/repo worktree list")
        print("tmp after restart:", kept.sh("cat /tmp/wm-learn-marker").stdout)
    finally:
        kept.delete()


def test_nix_installs_at_provisioning_and_survives_restart():
    """The Lima backend's Determinate installer flow, run inside a stock sandbox."""
    policy = os.environ.get("WORKMUX_TEST_OPENSHELL_NIX_POLICY")
    nixed, created = create_sandbox("nix", policy=policy)
    try:
        assert created.returncode == 0, created.stderr
        nixed.wait_for_phase("Ready")
        install = nixed.sh(
            "curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix "
            "| sh -s -- install linux --init none --no-confirm",
            timeout=900,
        )
        print(install.stdout[-4000:], install.stderr[-4000:])
        assert install.returncode == 0
        assert nixed.stop().returncode == 0
        nixed.wait_for_phase("Stopped")
        assert nixed.start().returncode == 0
        nixed.wait_for_phase("Ready")
        nixed.sh_ok("/nix/var/nix/profiles/default/bin/nix --version")
    finally:
        nixed.delete()


def test_root_user_image_is_refused_under_default_policy():
    refused, created = create_sandbox("root", image="docker.io/library/ubuntu:24.04")
    try:
        print(created.returncode, created.stdout, created.stderr)
        assert created.returncode != 0
    finally:
        refused.delete()


def test_status_and_errors_for_running_stopped_and_missing():
    tracked, created = create_sandbox("status")
    try:
        assert created.returncode == 0, created.stderr
        tracked.wait_for_phase("Ready")
        assert tracked.name in openshell(["sandbox", "list", "--names"]).stdout.split()

        assert tracked.stop().returncode == 0
        tracked.wait_for_phase("Stopped")
        assert tracked.name in openshell(["sandbox", "list", "--names"]).stdout.split()
        stopped_exec = tracked.sh("true")
        print("exec on stopped:", stopped_exec.returncode, stopped_exec.stderr)
        assert stopped_exec.returncode != 0
    finally:
        tracked.delete()

    missing = Sandbox(new_sandbox_name("missing"))
    get_missing = missing.get()
    exec_missing = missing.sh("true")
    print("get missing:", get_missing.returncode, get_missing.stderr)
    print("exec missing:", exec_missing.returncode, exec_missing.stderr)
    assert get_missing.returncode != 0
    assert exec_missing.returncode != 0
    assert missing.name not in openshell(["sandbox", "list", "--names"]).stdout.split()
