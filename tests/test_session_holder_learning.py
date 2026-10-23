"""Learning tests for picking the session holder (remote-project-hosts ticket 02).

A session holder keeps a remote pane alive on a remote while no workstation
pane is attached, without multiplexing (ADR 0003). These tests drive each
candidate the way `workmux remote attach` will: the attaching client runs in a
real tmux pane, which plays the workstation pane and the terminal emulator.

Env-gated, because the candidates are not part of the normal toolchain:

    WORKMUX_TEST_SESSION_HOLDER=1 \\
      nix shell nixpkgs#dtach nixpkgs#abduco nixpkgs#shpool \\
      -c tests/venv/bin/pytest tests/test_session_holder_learning.py

Candidates missing from PATH are skipped. `WORKMUX_TEST_SESSION_HOLDER_NIX=1`
also checks that each candidate builds from nixpkgs, and
`WORKMUX_TEST_SESSION_HOLDER_CLAUDE=1` reattaches to a real Claude Code
interface instead of the stand-in full-screen TUI.
"""

import os
import pty
import select
import shlex
import shutil
import subprocess
import sys
import time
from abc import ABC, abstractmethod
from pathlib import Path
from typing import Callable, Generator, Optional

import pytest

from .conftest import TmuxEnvironment

pytestmark = pytest.mark.skipif(
    os.environ.get("WORKMUX_TEST_SESSION_HOLDER") != "1",
    reason="set WORKMUX_TEST_SESSION_HOLDER=1 to run session holder learning tests",
)

# A stand-in for an agent's full-screen interface: alternate screen, a full
# repaint on SIGWINCH or Ctrl-L, and a nonzero exit on "q" so the end of a
# remote pane is observable.
FAKE_TUI = r"""
import os, signal, sys, termios, tty

draws = 0
last_key = "none"

def draw(*_):
    global draws
    draws += 1
    cols, rows = os.get_terminal_size(0)
    out = "\x1b[?1049h\x1b[2J\x1b[H"
    out += f"FAKE-TUI rows={rows} cols={cols} key={last_key}"
    out += f"\x1b[{rows};1HFAKE-TUI-FOOTER"
    sys.stdout.write(out)
    sys.stdout.flush()

signal.signal(signal.SIGWINCH, draw)
tty.setraw(0)
draw()
while True:
    try:
        ch = os.read(0, 1)
    except InterruptedError:
        continue
    if not ch or ch == b"q":
        sys.stdout.write("\x1b[?1049l")
        sys.stdout.flush()
        sys.exit(7)
    if ch == b"\x0c":
        draw()
        continue
    last_key = ch.decode(errors="replace")
    draw()
"""

TIMEOUT = 10.0


def wait_until(
    predicate: Callable[[], bool], what: str, timeout: float = TIMEOUT
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


class Holder(ABC):
    """One session holder candidate, rooted in a private directory."""

    name: str
    binary: str

    def __init__(self, root: Path):
        self.root = root
        self.env = os.environ.copy()
        self.env.pop("TMUX", None)

    @abstractmethod
    def create_argv(self, session: str, command: list[str]) -> list[str]:
        """Create a remote pane running `command`, without attaching."""

    @abstractmethod
    def attach_argv(self, session: str) -> list[str]:
        """Attach the current terminal to an existing remote pane."""

    @abstractmethod
    def exists(self, session: str) -> bool:
        """Whether the holder still has a remote pane by this name."""

    def create(self, session: str, command: list[str]) -> None:
        subprocess.run(
            self.create_argv(session, command),
            env=self.env,
            check=True,
            timeout=TIMEOUT,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )


class Dtach(Holder):
    name = "dtach"
    binary = "dtach"

    def socket(self, session: str) -> Path:
        return self.root / f"{session}.sock"

    def create_argv(self, session: str, command: list[str]) -> list[str]:
        return ["dtach", "-n", str(self.socket(session)), "-E", *command]

    def attach_argv(self, session: str) -> list[str]:
        return ["dtach", "-a", str(self.socket(session)), "-E", "-r", "winch"]

    def exists(self, session: str) -> bool:
        return self.socket(session).exists()


class Abduco(Holder):
    name = "abduco"
    binary = "abduco"

    def __init__(self, root: Path):
        super().__init__(root)
        self.env["ABDUCO_SOCKET_DIR"] = str(root)

    def create_argv(self, session: str, command: list[str]) -> list[str]:
        return ["abduco", "-n", session, *command]

    def attach_argv(self, session: str) -> list[str]:
        return ["abduco", "-a", session]

    def exists(self, session: str) -> bool:
        listing = subprocess.run(
            ["abduco"], env=self.env, capture_output=True, text=True
        ).stdout
        return any(line.endswith(f"\t{session}") for line in listing.splitlines())


class Shpool(Holder):
    name = "shpool"
    binary = "shpool"

    def __init__(self, root: Path):
        super().__init__(root)
        self.config = root / "shpool.toml"
        self.config.write_text("")
        self.env["XDG_RUNTIME_DIR"] = str(root)

    def base_argv(self) -> list[str]:
        return [
            "shpool",
            "--socket",
            str(self.root / "shpool.sock"),
            "--config-file",
            str(self.config),
            "--log-file",
            str(self.root / "shpool.log"),
            "--daemonize",
        ]

    def create_argv(self, session: str, command: list[str]) -> list[str]:
        return [
            *self.base_argv(),
            "attach",
            "--background",
            "--cmd",
            shlex.join(command),
            session,
        ]

    def attach_argv(self, session: str) -> list[str]:
        # --force: without it, attach right after a background create or a
        # dropped client is refused as Busy until the daemon notices.
        return [*self.base_argv(), "attach", "--force", session]

    def exists(self, session: str) -> bool:
        listing = subprocess.run(
            [*self.base_argv(), "list"], env=self.env, capture_output=True, text=True
        ).stdout
        return any(line.split("\t")[0] == session for line in listing.splitlines())


HOLDERS: list[type[Holder]] = [Dtach, Abduco, Shpool]


@pytest.fixture(params=HOLDERS, ids=lambda h: h.name)
def holder(request, tmp_path: Path) -> Generator[Holder, None, None]:
    cls: type[Holder] = request.param
    if shutil.which(cls.binary) is None:
        pytest.skip(f"{cls.binary} not on PATH (try nix shell nixpkgs#{cls.binary})")
    # Short root: Unix socket paths are limited to ~104 bytes on macOS.
    root = Path(
        subprocess.run(
            ["mktemp", "-d", "/tmp/wmsh.XXXXXX"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    )
    instance = cls(root)
    yield instance
    # Remote panes outlive their clients by design; reap everything this test
    # started (holders name the root or the fake TUI under tmp_path).
    for owned in (root, tmp_path):
        subprocess.run(["pkill", "-f", str(owned)], check=False)
    shutil.rmtree(root, ignore_errors=True)


@pytest.fixture
def terminal(tmp_path: Path) -> Generator["Terminal", None, None]:
    env = TmuxEnvironment(tmp_path)
    env.mux_command(["new-session", "-d", "-s", "test", "-x", "100", "-y", "30"])
    env.mux_command(["set-option", "-g", "window-size", "manual"])
    env.mux_command(["set-option", "-g", "remain-on-exit", "on"])
    env.mux_command(["set-option", "-g", "status", "off"])
    yield Terminal(env)
    env.stop_server()


class Terminal:
    """A workstation pane: runs attaching clients and shows what they draw."""

    def __init__(self, env: TmuxEnvironment):
        self.env = env
        self.pane: Optional[str] = None

    def run(self, argv: list[str], holder_env: dict[str, str]) -> str:
        """Run `argv` in a fresh window at the current size; returns the pane."""
        exports = " ".join(
            f"{k}={shlex.quote(v)}"
            for k, v in holder_env.items()
            if k in ("ABDUCO_SOCKET_DIR", "XDG_RUNTIME_DIR", "PATH", "HOME")
        )
        cmd = f"env {exports} {shlex.join(argv)}"
        self.pane = self.env.mux_command(
            ["new-window", "-P", "-F", "#{pane_id}", cmd]
        ).stdout.strip()
        return self.pane

    def screen(self) -> str:
        assert self.pane is not None
        return self.env.mux_command(
            ["capture-pane", "-p", "-t", self.pane], check=False
        ).stdout

    def wait_for(self, text: str, timeout: float = TIMEOUT) -> None:
        try:
            wait_until(lambda: text in self.screen(), repr(text), timeout)
        except AssertionError:
            raise AssertionError(
                f"{text!r} not on screen; screen was:\n{self.screen()}"
            ) from None

    def resize(self, cols: int, rows: int) -> None:
        assert self.pane is not None
        self.env.mux_command(
            ["resize-window", "-t", self.pane, "-x", str(cols), "-y", str(rows)]
        )

    def send(self, keys: str) -> None:
        assert self.pane is not None
        self.env.mux_command(["send-keys", "-t", self.pane, "-l", keys])

    def client_pid(self) -> int:
        assert self.pane is not None
        return int(self._format("#{pane_pid}"))

    def dead_status(self) -> Optional[int]:
        """Exit status of the pane's client (negative signal if killed), or
        None while it runs."""
        if self._format("#{pane_dead}") != "1":
            return None
        status = self._format("#{pane_dead_status}")
        if status:
            return int(status)
        return -1

    def wait_dead(self) -> int:
        wait_until(lambda: self.dead_status() is not None, "client exit")
        status = self.dead_status()
        assert status is not None
        return status

    def drop(self) -> None:
        """Kill the attaching client outright, as a lost connection would."""
        os.kill(self.client_pid(), 9)
        self.wait_dead()

    def _format(self, fmt: str) -> str:
        assert self.pane is not None
        return self.env.mux_command(
            ["display-message", "-p", "-t", self.pane, fmt]
        ).stdout.strip()


def fake_tui(tmp_path: Path) -> list[str]:
    script = tmp_path / "fake_tui.py"
    script.write_text(FAKE_TUI)
    return [sys.executable, str(script)]


def attach_once(holder: Holder) -> tuple[int, str]:
    """Attach on a private pty and return the client's exit status and output."""
    controller, tty = pty.openpty()
    proc = subprocess.Popen(
        holder.attach_argv("agent"),
        env=holder.env,
        stdin=tty,
        stdout=tty,
        stderr=tty,
        start_new_session=True,
    )
    try:
        status = proc.wait(timeout=TIMEOUT)
    except subprocess.TimeoutExpired:
        proc.kill()
        raise AssertionError("attach to an ended remote pane did not exit")
    # Hold our end of the pty open until drained: macOS discards unread
    # output once the last slave descriptor closes.
    output = b""
    while select.select([controller], [], [], 0.2)[0]:
        output += os.read(controller, 4096)
    os.close(tty)
    os.close(controller)
    return status, output.decode(errors="replace").strip()


def force_resize(terminal: Terminal, cols: int, rows: int) -> None:
    """What the attach loop does after reattaching: shrink, then restore."""
    terminal.resize(cols, rows - 1)
    time.sleep(0.2)
    terminal.resize(cols, rows)


def test_reattach_redraws_full_screen_interface(
    holder: Holder, terminal: Terminal, tmp_path: Path
):
    """Criterion 1: after a drop, a new client sees the whole interface again."""
    holder.create("agent", fake_tui(tmp_path))
    terminal.run(holder.attach_argv("agent"), holder.env)
    terminal.wait_for("FAKE-TUI rows=30 cols=100")
    terminal.send("x")
    terminal.wait_for("key=x")

    terminal.drop()
    terminal.run(holder.attach_argv("agent"), holder.env)
    force_resize(terminal, 100, 30)

    terminal.wait_for("FAKE-TUI rows=30 cols=100 key=x")
    terminal.wait_for("FAKE-TUI-FOOTER")


def test_resize_reaches_inner_process(
    holder: Holder, terminal: Terminal, tmp_path: Path
):
    """Criterion 2: the inner process sees the workstation pane's size."""
    holder.create("agent", fake_tui(tmp_path))
    terminal.run(holder.attach_argv("agent"), holder.env)
    terminal.wait_for("rows=30 cols=100")

    terminal.resize(120, 40)
    terminal.wait_for("rows=40 cols=120")

    terminal.resize(80, 24)
    terminal.wait_for("rows=24 cols=80")


def test_dropping_client_leaves_inner_process_running(
    holder: Holder, terminal: Terminal, tmp_path: Path
):
    """A lost connection detaches; input sent after reattach still arrives."""
    holder.create("agent", fake_tui(tmp_path))
    terminal.run(holder.attach_argv("agent"), holder.env)
    terminal.wait_for("FAKE-TUI")

    terminal.drop()
    time.sleep(0.5)

    terminal.run(holder.attach_argv("agent"), holder.env)
    force_resize(terminal, 100, 30)
    terminal.send("z")
    terminal.wait_for("key=z")


def test_reattach_picks_the_named_remote_pane(
    holder: Holder, terminal: Terminal, tmp_path: Path
):
    """Two remote panes side by side; attach reaches the one it names."""
    holder.create("first", fake_tui(tmp_path))
    holder.create("second", fake_tui(tmp_path))

    terminal.run(holder.attach_argv("first"), holder.env)
    terminal.wait_for("FAKE-TUI")
    terminal.send("1")
    terminal.wait_for("key=1")
    terminal.drop()

    terminal.run(holder.attach_argv("second"), holder.env)
    force_resize(terminal, 100, 30)
    terminal.wait_for("key=none")
    terminal.drop()

    terminal.run(holder.attach_argv("first"), holder.env)
    force_resize(terminal, 100, 30)
    terminal.wait_for("key=1")


def test_attach_after_inner_process_exits_reports_ended(
    holder: Holder, terminal: Terminal, tmp_path: Path
):
    """The remote pane ends with its process; a later attach fails clearly.

    Ticket 10 tells "ended" apart from "unreachable": the transport reaching the
    remote but the attach failing this way means the remote pane is gone.
    """
    holder.create("agent", fake_tui(tmp_path))
    terminal.run(holder.attach_argv("agent"), holder.env)
    terminal.wait_for("FAKE-TUI")
    terminal.send("q")
    attached_status = terminal.wait_dead()
    attached_screen = terminal.screen()

    later_status, later_output = attach_once(holder)

    print(f"\n[{holder.name}] attached client exit={attached_status}")
    print(f"[{holder.name}] attached screen: {attached_screen.strip()!r}")
    print(f"[{holder.name}] later attach exit={later_status}")
    print(f"[{holder.name}] later attach output: {later_output!r}")
    if isinstance(holder, Shpool):
        # `shpool attach` is create-or-attach: the ended remote pane comes
        # back as a fresh login shell, so "ended" is indistinguishable.
        assert holder.exists("agent")
        return
    assert later_status == 1
    assert "No such file or directory" in later_output
    assert not holder.exists("agent")


@pytest.mark.skipif(
    os.environ.get("WORKMUX_TEST_SESSION_HOLDER_CLAUDE") != "1",
    reason="set WORKMUX_TEST_SESSION_HOLDER_CLAUDE=1 to reattach to Claude Code",
)
def test_reattach_redraws_claude_code(holder: Holder, terminal: Terminal):
    """Criterion 1 against the real agent interface."""
    claude = shutil.which("claude")
    if claude is None:
        pytest.skip("claude not on PATH")
    workdir = holder.root / "project"
    workdir.mkdir()
    holder.create("claude", ["sh", "-c", f"cd {workdir} && exec {claude}"])
    terminal.run(holder.attach_argv("claude"), holder.env)
    wait_until(lambda: len(terminal.screen().strip()) > 40, "claude to draw", 30)
    time.sleep(2)
    before = terminal.screen()

    terminal.drop()
    terminal.run(holder.attach_argv("claude"), holder.env)
    force_resize(terminal, 100, 30)
    time.sleep(2)
    after = terminal.screen()

    print(f"\n[{holder.name}] before drop:\n{before}")
    print(f"[{holder.name}] after reattach:\n{after}")
    marker = next(
        line.strip() for line in before.splitlines() if len(line.strip()) > 10
    )
    assert marker in after


@pytest.mark.skipif(
    os.environ.get("WORKMUX_TEST_SESSION_HOLDER_NIX") != "1",
    reason="set WORKMUX_TEST_SESSION_HOLDER_NIX=1 to build candidates from nixpkgs",
)
@pytest.mark.parametrize("cls", HOLDERS, ids=lambda h: h.name)
def test_holder_installs_through_nix(cls: type[Holder]):
    """Criterion 4: the holder comes from nixpkgs with its binary in bin/."""
    out = subprocess.run(
        ["nix", "build", "--no-link", "--print-out-paths", f"nixpkgs#{cls.binary}"],
        capture_output=True,
        text=True,
        check=True,
        timeout=600,
    ).stdout.split()
    assert any((Path(p) / "bin" / cls.binary).exists() for p in out)
