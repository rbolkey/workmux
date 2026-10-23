"""Learning tests pinning down the OpenShell behaviour the OpenShell transport relies on.

Run with WORKMUX_TEST_OPENSHELL=1 and an active OpenShell gateway. The git push
test also needs WORKMUX_TEST_OPENSHELL_GITHUB_PROVIDER (a configured `github`
provider) and WORKMUX_TEST_OPENSHELL_GITHUB_REPO (an HTTPS repository the token
may push scratch branches to). Findings live in ticket 01's `## Answer`; the
policy composition, live policy change and baked devshell findings live in
ticket 21's `## Answer`.
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
from pathlib import Path
from typing import Generator, Optional

import pytest

from .support.openshell import (
    DEVSHELL_IMAGE,
    Sandbox,
    FIXTURE_DIR,
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
    assert_stdio_round_trips(sandbox, HOLD_SECONDS)


def assert_stdio_round_trips(sandbox: Sandbox, hold_seconds: float) -> None:
    process = spawn_echo_loop(sandbox)
    reader = LineReader(process)
    try:
        noise = read_until_sentinel(reader)
        print(f"stdout before sentinel: {noise!r}")

        deadline = time.monotonic() + hold_seconds
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
        print(f"{count} round trips over {hold_seconds}s, worst {worst:.3f}s")
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
    assert_git_worktree_lifecycle(sandbox)


def assert_git_worktree_lifecycle(sandbox: Sandbox) -> None:
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
    assert_unix_socket_round_trip(sandbox)


def assert_unix_socket_round_trip(sandbox: Sandbox) -> None:
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


def test_default_policy_blocks_provisioning_nix(sandbox: Sandbox):
    """No egress, no root and no writable / under the default policy."""
    blocked = sandbox.sh(
        "curl -sS -m 10 -o /dev/null https://install.determinate.systems/nix; echo curl=$?; "
        "command -v sudo || echo no-sudo; "
        "mkdir /wm-learn-nix 2>&1; echo mkdir=$?"
    )
    print(blocked.stdout, blocked.stderr)
    assert "curl=7" in blocked.stdout
    assert "no-sudo" in blocked.stdout
    assert "mkdir=1" in blocked.stdout


def test_nix_profiles_fail_even_with_a_nix_policy():
    """Documents a failure: Nix opens "/", which Landlock denies even when the
    policy lists "/" read-only. The store itself works and survives restart."""
    nixed, created = create_sandbox("nix", policy=str(FIXTURE_DIR / "nix-policy.yaml"))
    try:
        assert created.returncode == 0, created.stderr
        nixed.wait_for_phase("Ready")
        install = nixed.sh(
            "curl -sSfL https://nixos.org/nix/install -o /tmp/install "
            "&& sh /tmp/install --no-daemon --yes",
            timeout=900,
        )
        print(install.stdout[-2000:], install.stderr[-2000:])
        assert 'opening file "/": Permission denied' in install.stdout + install.stderr
        nix = "$(ls -d /nix/store/*-nix-[0-9]*/bin | head -1)/nix"
        assert (
            nixed.sh_ok(
                f"{nix} --extra-experimental-features nix-command eval --expr 1+1"
            ).strip()
            == "2"
        )

        assert nixed.stop().returncode == 0
        nixed.wait_for_phase("Stopped")
        assert nixed.start().returncode == 0
        nixed.wait_for_phase("Ready")
        assert "Nix" in nixed.sh_ok(f"{nix} --version")
    finally:
        nixed.delete()


def test_slash_cannot_be_granted_read_write():
    refused, created = create_sandbox(
        "root", policy=str(FIXTURE_DIR / "rw-root-policy.yaml")
    )
    try:
        print(created.stderr)
        assert created.returncode != 0
        assert "overly broad" in created.stderr
    finally:
        refused.delete()


@pytest.mark.parametrize(
    "image",
    ["docker.io/library/ubuntu:24.04", "docker.io/library/debian:bookworm-slim"],
)
def test_any_image_runs_as_uid_1000_in_sandbox_workdir(image: str):
    """Root-user images and images without a uid 1000 user are both accepted."""
    stock, created = create_sandbox("img", image=image)
    try:
        assert created.returncode == 0, created.stderr
        stock.wait_for_phase("Ready")
        assert stock.sh_ok("id -u; pwd; echo $HOME").split() == [
            "1000",
            "/sandbox",
            "/sandbox",
        ]
    finally:
        stock.delete()


def test_stock_image_lacks_git_and_python():
    stock, created = create_sandbox("stock", image=None)
    try:
        assert created.returncode == 0, created.stderr
        stock.wait_for_phase("Ready")
        output = stock.sh_ok(
            "for b in git python3 curl; do command -v $b || echo no-$b; done"
        )
        assert output.split() == ["no-git", "no-python3", "no-curl"]
    finally:
        stock.delete()


def test_sandbox_names_are_limited_to_19_characters():
    long_name = openshell(
        ["sandbox", "create", "--name", "wm-learn-name-123456", "--no-tty", "--detach"]
    )
    assert long_name.returncode != 0
    assert "maximum length (20 > 19)" in long_name.stderr


def test_exit_of_initial_command_completes_the_sandbox_and_refuses_exec():
    done = Sandbox(new_sandbox_name("done"))
    try:
        created = openshell(
            [
                "sandbox",
                "create",
                "--name",
                done.name,
                "--no-tty",
                "--no-auto-providers",
                "--",
                "true",
            ]
        )
        assert created.returncode == 0, created.stderr
        done.wait_for_phase("Completed")
        refused = done.sh("true")
        assert refused.returncode == 1
        assert "is not ready (phase: Completed)" in refused.stderr
    finally:
        done.delete()


def test_default_policy_denies_allocating_a_pty(sandbox: Sandbox):
    """Ticket 02 criterion 3: /dev/ptmx is outside the default Landlock policy."""
    for tty in (False, True):
        process, master = run_in_pty(
            sandbox.exec_args(
                ["sh", "-c", "dtach -n /tmp/t.sock -E sleep 1; echo rc=$?"], tty=tty
            ),
            rows=24,
            cols=80,
        )
        try:
            output = read_pty_until(master, "rc=", timeout=30)
            assert "init_pty: Permission denied" in output
            assert "rc=1" in output
        finally:
            process.kill()
            process.wait()
            os.close(master)


@pytest.fixture(scope="module")
def pty_sandbox() -> Generator[Sandbox, None, None]:
    sandbox, created = create_sandbox(
        "pty", policy=str(FIXTURE_DIR / "pty-policy.yaml")
    )
    try:
        assert created.returncode == 0, created.stderr
        sandbox.wait_for_phase("Ready")
        yield sandbox
    finally:
        sandbox.delete()


@pytest.mark.parametrize(
    "socket_dir", ["$PWD/.workmux/panes", "$HOME/.local/state/workmux", "/tmp"]
)
def test_dtach_holder_with_pty_devices_in_policy(pty_sandbox: Sandbox, socket_dir: str):
    """Ticket 02 criterion 3: with /dev/pts and /dev/ptmx read-write, a dtach
    remote pane can be created by one exec and attached from another."""
    assert_dtach_holder_attaches(pty_sandbox, socket_dir)


def assert_dtach_holder_attaches(
    sandbox: Sandbox, socket_dir: str, command: str = "sh -c"
) -> None:
    """Start a ticking dtach holder from one exec and see it from a TTY attach.

    `command` prefixes the holder's script, e.g. to run it in a devshell.
    """
    socket = f"{socket_dir}/t-{uuid.uuid4().hex[:6]}.sock"
    # The holder must not inherit the exec's stdio, or the exec never returns.
    sandbox.sh_ok(
        f'mkdir -p "$(dirname {socket})" && '
        f"dtach -n {socket} -E {command} 'while :; do echo tick; sleep 1; done' "
        "</dev/null >/dev/null 2>&1",
        timeout=60,
    )
    process, master = run_in_pty(
        sandbox.exec_args(
            ["sh", "-c", f"exec dtach -a {socket} -E -r winch"], tty=True
        ),
        rows=24,
        cols=80,
    )
    try:
        assert "tick" in read_pty_until(master, "tick", timeout=30)
    finally:
        process.kill()
        process.wait()
        os.close(master)
    sandbox.sh_ok(f"pkill -f '^dtach -n {socket}'; rm -f {socket}")


# Ticket 21: policy composition, policy changes on a running sandbox, and a
# devshell baked into the image under the candidate workmux base policy.

BASE_POLICY = str(FIXTURE_DIR / "base-policy.yaml")
# OpenShell's built-in filesystem policy, as `sandbox get` reports it.
DEFAULT_FILESYSTEM = {
    "include_workdir": True,
    "read_only": ["/usr", "/lib", "/etc", "/var/log", "/proc", "/dev/urandom"],
    "read_write": ["/tmp", "/dev/null"],
}
NIXOS_ORG = """network_policies:
  nixorg:
    endpoints:
      - { host: nixos.org, port: 443 }
    binaries:
      - path: /usr/bin/curl
"""
CURL_NIXOS_ORG = "curl -sS -m 10 -o /dev/null https://nixos.org; echo curl=$?"


def write_policy(tmp_path: Path, name: str, body: str) -> str:
    path = tmp_path / f"{name}.yaml"
    path.write_text(f"version: 1\n{body}")
    return str(path)


def base_policy_with(extra: str) -> str:
    """The base policy fixture's YAML with `extra` appended."""
    return (FIXTURE_DIR / "base-policy.yaml").read_text() + extra


def test_create_takes_exactly_one_policy_file():
    refused = openshell(
        [
            "sandbox",
            "create",
            "--name",
            new_sandbox_name("twop"),
            "--policy",
            BASE_POLICY,
            "--policy",
            BASE_POLICY,
            "--detach",
        ]
    )
    assert refused.returncode != 0
    assert "cannot be used multiple times" in refused.stderr


def test_sections_missing_from_the_policy_file_fall_back_to_the_default(
    tmp_path: Path,
):
    """A network-only policy keeps the default filesystem section, so OpenShell
    composes per top-level section, not per entry."""
    netonly, created = create_sandbox(
        "neto", policy=write_policy(tmp_path, "netonly", NIXOS_ORG)
    )
    try:
        assert created.returncode == 0, created.stderr
        netonly.wait_for_phase("Ready")
        policy = netonly.policy()
        assert policy["filesystem_policy"] == DEFAULT_FILESYSTEM
        assert list(policy["network_policies"]) == ["nixorg"]
        assert "curl=0" in netonly.sh_ok(CURL_NIXOS_ORG)
    finally:
        netonly.delete()


def test_filesystem_lists_are_stored_verbatim_and_grants_add_up(tmp_path: Path):
    """No dedup and no conflict check: a path in both lists is writable."""
    overlap, created = create_sandbox(
        "ovlp",
        policy=write_policy(
            tmp_path,
            "overlap",
            "filesystem_policy:\n"
            "  include_workdir: true\n"
            "  read_only: [/usr, /lib, /etc, /proc, /tmp]\n"
            "  read_write: [/tmp, /dev/null, /dev/null]\n",
        ),
    )
    try:
        assert created.returncode == 0, created.stderr
        overlap.wait_for_phase("Ready")
        assert overlap.policy()["filesystem_policy"]["read_write"] == [
            "/tmp",
            "/dev/null",
            "/dev/null",
        ]
        overlap.sh_ok("echo written > /tmp/wm-learn-overlap")
    finally:
        overlap.delete()


def test_duplicate_network_policy_names_are_refused(tmp_path: Path):
    duplicate = NIXOS_ORG + NIXOS_ORG.split("\n", 1)[1]
    refused, created = create_sandbox(
        "dupk", policy=write_policy(tmp_path, "dupkey", duplicate)
    )
    try:
        assert created.returncode != 0
        assert 'duplicate key "nixorg"' in created.stderr
    finally:
        refused.delete()


def test_policy_set_on_a_running_sandbox(tmp_path: Path):
    """Network changes apply at once. Filesystem changes are stored at once but
    only reach the sandbox's processes after stop and start, and paths from the
    create-time policy can never be removed: that needs a new sandbox."""
    pty_policy = (FIXTURE_DIR / "pty-policy.yaml").read_text()
    live, created = create_sandbox("live", policy=str(FIXTURE_DIR / "pty-policy.yaml"))
    try:
        assert created.returncode == 0, created.stderr
        live.wait_for_phase("Ready")
        assert "curl=7" in live.sh_ok(CURL_NIXOS_ORG)
        assert "Permission denied" in live.sh("ls /nix").stderr

        widened = tmp_path / "widened.yaml"
        widened.write_text(base_policy_with(NIXOS_ORG))
        assert live.set_policy(str(widened)).returncode == 0
        assert "/nix" in live.policy()["filesystem_policy"]["read_only"]
        assert "curl=0" in live.sh_ok(CURL_NIXOS_ORG)
        assert "Permission denied" in live.sh("ls /nix").stderr

        live.restart()
        assert "curl=0" in live.sh_ok(CURL_NIXOS_ORG)
        live.sh_ok("ls /nix")

        without_pts = tmp_path / "without-pts.yaml"
        without_pts.write_text(pty_policy.replace(", /dev/pts, /dev/ptmx]", "]"))
        refused = live.set_policy(str(without_pts))
        print(refused.stderr)
        assert refused.returncode != 0
        assert (
            "read_write path '/dev/pts' cannot be removed on a live sandbox"
            in " ".join(refused.stderr.split()).replace("│ ", "")
        )

        assert live.set_policy(str(FIXTURE_DIR / "pty-policy.yaml")).returncode == 0
        live.sh_ok("ls /nix")
        live.restart()
        assert "Permission denied" in live.sh("ls /nix").stderr
        assert "curl=7" in live.sh_ok(CURL_NIXOS_ORG)
        print(openshell(["policy", "list", live.name]).stdout)
    finally:
        live.delete()


def test_policy_update_adds_a_network_endpoint_live():
    live, created = create_sandbox("upd", policy=BASE_POLICY)
    try:
        assert created.returncode == 0, created.stderr
        live.wait_for_phase("Ready")
        updated = openshell(
            [
                "policy",
                "update",
                live.name,
                "--add-endpoint",
                "nixos.org:443",
                "--binary",
                "/usr/bin/curl",
                "--wait",
            ]
        )
        assert updated.returncode == 0, updated.stderr
        assert "curl=0" in live.sh_ok(CURL_NIXOS_ORG)
        assert list(live.policy()["network_policies"]) == ["allow_nixos_org_443"]
    finally:
        live.delete()


@pytest.fixture(scope="module")
def devshell_sandbox() -> Generator[Sandbox, None, None]:
    """The devshell image under the candidate base policy alone."""
    sandbox, created = create_sandbox("dev", image=DEVSHELL_IMAGE, policy=BASE_POLICY)
    try:
        assert created.returncode == 0, created.stderr
        sandbox.wait_for_phase("Ready")
        yield sandbox
    finally:
        sandbox.delete()


def test_baked_devshell_tools_run_through_exec(devshell_sandbox: Sandbox):
    """Prebuilt store paths run without the `opening file "/"` denial that
    nix's own commands hit; the wrapper sources the recorded environment."""
    output = devshell_sandbox.sh_ok(
        'devshell sh -c \'echo "devshell=$WM_LEARN_DEVSHELL"; command -v jq; '
        "jq -n 1+1; echo needle | rg -c needle; git --version'"
    )
    print(output)
    lines = output.splitlines()
    assert "devshell=1" in lines
    assert any(
        line.startswith("/nix/store/") and line.endswith("/bin/jq") for line in lines
    )
    assert "2" in lines and "1" in lines
    assert any(line.startswith("git version") for line in lines)


def test_baked_devshell_runs_an_agent_command_in_a_tty(devshell_sandbox: Sandbox):
    process, master = run_in_pty(
        devshell_sandbox.exec_args(
            ["devshell", "bash", "-c", "jq -M -c -n '{agent: \"ok\"}'; tty; echo DONE"],
            tty=True,
        ),
        rows=24,
        cols=80,
    )
    try:
        output = read_pty_until(master, "DONE", timeout=60)
        print(repr(output))
        assert '{"agent":"ok"}' in output
        assert "/dev/pts/" in output
    finally:
        process.kill()
        process.wait()
        os.close(master)


def test_baked_devshell_as_a_session_holder_command(devshell_sandbox: Sandbox):
    assert_dtach_holder_attaches(
        devshell_sandbox, "$HOME/.local/state/workmux", command="devshell bash -c"
    )


def test_baked_devshell_needs_nix_in_the_policy():
    """Without /nix read-only the baked store is unreadable."""
    denied, created = create_sandbox(
        "nonx", image=DEVSHELL_IMAGE, policy=str(FIXTURE_DIR / "pty-policy.yaml")
    )
    try:
        assert created.returncode == 0, created.stderr
        denied.wait_for_phase("Ready")
        result = denied.sh("devshell jq -n 1")
        print(result.stdout, result.stderr)
        assert result.returncode != 0
        assert "Permission denied" in result.stderr
    finally:
        denied.delete()


@pytest.mark.parametrize(
    "socket_dir", ["$PWD/.workmux/panes", "$HOME/.local/state/workmux", "/tmp"]
)
def test_base_policy_keeps_the_session_holder_working(
    devshell_sandbox: Sandbox, socket_dir: str
):
    assert_dtach_holder_attaches(devshell_sandbox, socket_dir)


def test_base_policy_keeps_git_worktrees_working(devshell_sandbox: Sandbox):
    assert_git_worktree_lifecycle(devshell_sandbox)


def test_base_policy_keeps_unix_sockets_working(devshell_sandbox: Sandbox):
    assert_unix_socket_round_trip(devshell_sandbox)


def test_base_policy_keeps_the_stdio_channel_working(devshell_sandbox: Sandbox):
    assert_stdio_round_trips(devshell_sandbox, min(HOLD_SECONDS, 60))
