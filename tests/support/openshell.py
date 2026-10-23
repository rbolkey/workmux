"""Drive the `openshell` CLI for env-gated OpenShell learning and contract tests.

Every sandbox created here is named with the `wm-learn-` prefix and deleted by
the fixture that created it; nothing here touches other sandboxes.
"""

import json
import os
import shutil
import subprocess
import time
import uuid
from dataclasses import dataclass
from typing import Any, Optional, Sequence

import pytest

SANDBOX_PREFIX = "wm-learn-"
READY_TIMEOUT = float(os.environ.get("WORKMUX_TEST_OPENSHELL_READY_TIMEOUT", "300"))

requires_openshell = pytest.mark.skipif(
    os.environ.get("WORKMUX_TEST_OPENSHELL") != "1"
    or shutil.which("openshell") is None,
    reason="set WORKMUX_TEST_OPENSHELL=1 with openshell on PATH and an active gateway",
)


def openshell(
    args: Sequence[str],
    *,
    input: Optional[str] = None,
    timeout: float = 120,
) -> subprocess.CompletedProcess[str]:
    """Run an openshell CLI command and capture its output, never raising on failure."""
    return subprocess.run(
        ["openshell", *args],
        input=input,
        stdin=None if input is not None else subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=timeout,
        env={**os.environ, "NO_COLOR": "1"},
    )


def new_sandbox_name(label: str) -> str:
    return f"{SANDBOX_PREFIX}{label}-{uuid.uuid4().hex[:6]}"


def find_key(value: Any, key: str) -> Optional[Any]:
    """Return the first value stored under `key` anywhere in a decoded JSON document."""
    if isinstance(value, dict):
        if key in value:
            return value[key]
        children = list(value.values())
    elif isinstance(value, list):
        children = value
    else:
        return None
    for child in children:
        found = find_key(child, key)
        if found is not None:
            return found
    return None


@dataclass
class Sandbox:
    name: str

    def __post_init__(self) -> None:
        assert self.name.startswith(SANDBOX_PREFIX), (
            "learning tests only own wm-learn- sandboxes"
        )

    def exec_args(
        self,
        command: Sequence[str],
        *,
        tty: bool = False,
        workdir: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
    ) -> list[str]:
        args = [
            "openshell",
            "sandbox",
            "exec",
            "-n",
            self.name,
            "--tty" if tty else "--no-tty",
        ]
        if workdir is not None:
            args += ["--workdir", workdir]
        for key, value in (env or {}).items():
            args += ["--env", f"{key}={value}"]
        return [*args, "--", *command]

    def sh(
        self, script: str, *, timeout: float = 120
    ) -> subprocess.CompletedProcess[str]:
        """Run a POSIX shell script in the sandbox with captured, non-TTY stdio."""
        return openshell(self.exec_args(["sh", "-c", script])[1:], timeout=timeout)

    def sh_ok(self, script: str, *, timeout: float = 120) -> str:
        result = self.sh(script, timeout=timeout)
        assert result.returncode == 0, (
            f"exit {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        return result.stdout

    def get(self) -> subprocess.CompletedProcess[str]:
        return openshell(["sandbox", "get", self.name, "-o", "json"])

    def phase(self) -> Optional[str]:
        result = self.get()
        if result.returncode != 0:
            return None
        phase = find_key(json.loads(result.stdout), "phase")
        return None if phase is None else str(phase)

    def wait_for_phase(self, wanted: str, timeout: float = READY_TIMEOUT) -> None:
        deadline = time.monotonic() + timeout
        seen: Optional[str] = None
        while time.monotonic() < deadline:
            seen = self.phase()
            if seen is not None and seen.lower().endswith(wanted.lower()):
                return
            time.sleep(2)
        raise AssertionError(f"{self.name} never reached {wanted}; last phase {seen!r}")

    def stop(self) -> subprocess.CompletedProcess[str]:
        return openshell(["sandbox", "stop", self.name])

    def start(self) -> subprocess.CompletedProcess[str]:
        return openshell(["sandbox", "start", self.name])

    def delete(self) -> None:
        openshell(["sandbox", "delete", self.name], timeout=300)


def create_sandbox(
    label: str,
    *,
    image: Optional[str] = None,
    policy: Optional[str] = None,
    providers: Sequence[str] = (),
) -> tuple[Sandbox, subprocess.CompletedProcess[str]]:
    """Create a kept sandbox whose initial command exits at once, and return the create result."""
    sandbox = Sandbox(new_sandbox_name(label))
    args = [
        "sandbox",
        "create",
        "--name",
        sandbox.name,
        "--no-tty",
        "--no-auto-providers",
    ]
    if image is not None:
        args += ["--from", image]
    if policy is not None:
        args += ["--policy", policy]
    for provider in providers:
        args += ["--provider", provider]
    return sandbox, openshell([*args, "--", "true"], timeout=READY_TIMEOUT)


def workspace_dir(sandbox: Sandbox) -> str:
    """The sandbox's working directory, which the default policy makes read-write."""
    return sandbox.sh_ok("pwd").strip().splitlines()[-1]
