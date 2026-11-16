"""PATH doubles for Docker Sandboxes' `sbx` and the `docker` it builds templates with.

The fake `sbx` keeps its sandboxes in a JSON file, answers the queries workmux
makes (`ls`, `policy ls`, `diagnose`, `template ls`) with canned JSON and
succeeds at everything else. `policy ls <name>` of an existing sandbox also
lists the rule its agent kit added at create. Both fakes record each invocation, in order, so a
test asserts on the commands workmux issued.
"""

import json
import shlex
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

FAKE_SBX = r"""
import fcntl, json, os, subprocess, sys
from pathlib import Path

STATE = Path(sys.argv[1])
PROGRAM = sys.argv[2]
argv = sys.argv[3:]


def read(name):
    return json.loads((STATE / name).read_text())


def write(name, value):
    (STATE / name).write_text(json.dumps(value))


def tmux_windows():
    if not os.environ.get("TMUX"):
        return None
    result = subprocess.run(
        ["tmux", "list-windows", "-a", "-F", "#{window_name}"],
        capture_output=True, text=True,
    )
    return result.stdout.split()


def flag(name):
    return argv[argv.index(name) + 1] if name in argv else None


def sbx():
    sandboxes = read("sandboxes.json")
    verb = argv[0] if argv else ""
    if verb == "ls":
        print(json.dumps(sandboxes))
    elif verb == "policy" and argv[1:2] == ["ls"]:
        rules = read("rules.json")
        name = argv[2] if len(argv) > 2 and not argv[2].startswith("-") else None
        kit = read("kit_resources.json")
        if kit and any(sandbox["name"] == name for sandbox in sandboxes):
            rules = rules + [{
                "scope": "sandbox:" + name,
                "decision": "allow",
                "resource_type": "network",
                "resources": kit,
                "provenance": {"created_via": "provisioned"},
            }]
        print(json.dumps({"rules": rules}))
    elif verb == "diagnose":
        print(json.dumps({"checks": read("checks.json")}))
    elif verb == "template" and argv[1:2] == ["ls"]:
        print("[]")
    elif verb == "create":
        sandboxes.append({"name": flag("--name"), "status": "running"})
        write("sandboxes.json", sandboxes)
    elif verb == "rm":
        names = [word for word in argv[1:] if not word.startswith("-")]
        write("sandboxes.json", [s for s in sandboxes if s["name"] not in names])
    elif verb == "stop":
        for sandbox in sandboxes:
            if sandbox["name"] in argv[1:]:
                sandbox["status"] = "stopped"
        write("sandboxes.json", sandboxes)


def docker():
    if argv[:2] == ["image", "inspect"] and "--format" in argv:
        print("sha256:" + "0" * 64)
    elif argv[:1] == ["build"]:
        sys.stdin.read()
    elif argv[:1] == ["save"] and "-o" in argv:
        Path(flag("-o")).write_bytes(b"")
    elif argv[:2] == ["network", "inspect"]:
        print("172.17.0.1")


with open(STATE / ".lock", "w") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    entry = {"program": PROGRAM, "argv": argv}
    if PROGRAM == "sbx" and argv[:1] == ["create"]:
        entry["windows"] = tmux_windows()
    with open(STATE / "calls.jsonl", "a") as log:
        log.write(json.dumps(entry) + "\n")
    sbx() if PROGRAM == "sbx" else docker()
"""


def passing_checks() -> list[dict[str, str]]:
    """`sbx diagnose` checks for a logged-in sbx with its global policy chosen."""
    return [
        {"name": "Authentication", "status": "pass"},
        {"name": "Global network policy", "status": "pass"},
    ]


@dataclass
class SbxCall:
    """One invocation of the fake `sbx` or `docker`."""

    program: str
    argv: list[str]
    # Window names in the multiplexer when `sbx create` ran (tmux only).
    windows: Optional[list[str]] = None


class FakeSbx:
    """Fake `sbx` and `docker` installed in a test's fake-bin directory.

    `install_script` comes from the test environment, which puts the fake-bin
    directory first on PATH, ahead of any real `sbx` or `docker`.
    """

    def __init__(
        self, bin_dir: Path, state_dir: Path, install_script: Callable[..., Path]
    ):
        self.state_dir = state_dir
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "calls.jsonl").write_text("")
        (state_dir / "fake_sbx.py").write_text(FAKE_SBX)
        self.sandboxes = []
        self.global_rules = []
        self.kit_resources = []
        self.checks = passing_checks()
        for program in ("sbx", "docker"):
            install_script(
                bin_dir / program,
                "#!/bin/sh\nexec "
                + shlex.join(
                    [sys.executable, "-I", str(state_dir / "fake_sbx.py")]
                    + [str(state_dir), program]
                )
                + ' "$@"\n',
            )

    def _write(self, name: str, value: Any) -> None:
        (self.state_dir / name).write_text(json.dumps(value))

    def _read(self, name: str) -> Any:
        return json.loads((self.state_dir / name).read_text())

    @property
    def sandboxes(self) -> list[dict[str, str]]:
        """What `sbx ls --json` lists: each sandbox's `name` and `status`."""
        return self._read("sandboxes.json")

    @sandboxes.setter
    def sandboxes(self, value: list[dict[str, str]]) -> None:
        self._write("sandboxes.json", value)

    @property
    def global_rules(self) -> list[dict[str, Any]]:
        """The rules `sbx policy ls --json` lists, for every sandbox."""
        return self._read("rules.json")

    @global_rules.setter
    def global_rules(self, value: list[dict[str, Any]]) -> None:
        self._write("rules.json", value)

    @property
    def kit_resources(self) -> list[str]:
        """What the rule each sandbox's kit adds at create allows (`host:port`)."""
        return self._read("kit_resources.json")

    @kit_resources.setter
    def kit_resources(self, value: list[str]) -> None:
        self._write("kit_resources.json", value)

    @property
    def checks(self) -> list[dict[str, str]]:
        """The checks `sbx diagnose --json` reports."""
        return self._read("checks.json")

    @checks.setter
    def checks(self, value: list[dict[str, str]]) -> None:
        self._write("checks.json", value)

    def calls(self, program: str = "sbx") -> list[SbxCall]:
        """Every recorded invocation of `program`, in order."""
        lines = (self.state_dir / "calls.jsonl").read_text().splitlines()
        calls = [SbxCall(**json.loads(line)) for line in lines]
        return [call for call in calls if call.program == program]

    def verb_calls(self, *verb: str) -> list[SbxCall]:
        """The `sbx` invocations whose argv starts with `verb`."""
        return [call for call in self.calls() if call.argv[: len(verb)] == list(verb)]
