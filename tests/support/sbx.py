"""Drive the `sbx` CLI (Docker Sandboxes) for env-gated learning and contract tests.

Every sandbox created here is named with the `wm-learn-` prefix and removed by
the fixture or test that created it. Policy rules and secrets are only ever
scoped to those sandboxes with `--sandbox`; nothing here touches the global
policy, global secrets or any other sandbox.
"""

import functools
import json
import os
import shutil
import subprocess
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional, Sequence

import pytest

SANDBOX_PREFIX = "wm-learn-"
FIXTURE_DIR = Path(__file__).parent.parent / "fixtures" / "sbx"
TEMPLATE_TAG = "wm-learn-sbx-claude:latest"
CREATE_TIMEOUT = float(os.environ.get("WORKMUX_TEST_SBX_CREATE_TIMEOUT", "600"))


def _sbx_ready() -> bool:
    if os.environ.get("WORKMUX_TEST_SBX") != "1" or shutil.which("sbx") is None:
        return False
    # `sbx ls` fails when logged out or when the daemon can't be reached.
    try:
        listed = subprocess.run(
            ["sbx", "ls", "-q"], capture_output=True, text=True, timeout=60
        )
    except subprocess.TimeoutExpired:
        return False
    return listed.returncode == 0


requires_sbx = pytest.mark.skipif(
    not _sbx_ready(),
    reason="set WORKMUX_TEST_SBX=1 with sbx on PATH, logged in, and its global policy initialized",
)

requires_docker = pytest.mark.skipif(
    os.environ.get("WORKMUX_TEST_SBX") != "1"
    or shutil.which("docker") is None
    or subprocess.run(["docker", "info"], capture_output=True).returncode != 0,
    reason="needs a running Docker daemon to build the template layer",
)


def sbx(
    args: Sequence[str],
    *,
    input: Optional[str] = None,
    timeout: float = 120,
) -> subprocess.CompletedProcess[str]:
    """Run an sbx command and capture its output, never raising on failure."""
    return subprocess.run(
        ["sbx", *args],
        input=input,
        stdin=None if input is not None else subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=timeout,
        env={**os.environ, "NO_COLOR": "1"},
    )


def sbx_json(args: Sequence[str]) -> Any:
    result = sbx([*args, "--json"])
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def new_sandbox_name(label: str) -> str:
    return f"{SANDBOX_PREFIX}{label}-{uuid.uuid4().hex[:6]}"


def list_sandboxes() -> dict[str, dict[str, Any]]:
    return {entry["name"]: entry for entry in sbx_json(["ls"])}


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
        interactive: bool = False,
        user: Optional[str] = None,
        workdir: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
    ) -> list[str]:
        args = ["sbx", "exec"]
        if interactive:
            args.append("-i")
        if tty:
            args.append("-t")
        if user is not None:
            args += ["-u", user]
        if workdir is not None:
            args += ["-w", workdir]
        for key, value in (env or {}).items():
            args += ["-e", f"{key}={value}"]
        return [*args, self.name, *command]

    def sh(
        self,
        script: str,
        *,
        user: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
        input: Optional[str] = None,
        timeout: float = 120,
    ) -> subprocess.CompletedProcess[str]:
        """Run a shell script in the sandbox with captured, non-TTY stdio."""
        argv = self.exec_args(["bash", "-c", script], user=user, env=env)
        return sbx(argv[1:], input=input, timeout=timeout)

    def sh_ok(self, script: str, **kwargs: Any) -> str:
        result = self.sh(script, **kwargs)
        assert result.returncode == 0, (
            f"exit {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        return result.stdout

    def status(self) -> Optional[str]:
        entry = list_sandboxes().get(self.name)
        return None if entry is None else entry["status"]

    def stop(self) -> subprocess.CompletedProcess[str]:
        return sbx(["stop", self.name])

    def remove(self) -> None:
        sbx(["rm", "-f", self.name], timeout=300)

    def local_rules(self) -> list[dict[str, Any]]:
        """Rules scoped to this sandbox, from `sbx policy ls <name> --json`."""
        rules = sbx_json(["policy", "ls", self.name])["rules"]
        return [rule for rule in rules if rule["scope"] == f"sandbox:{self.name}"]

    def allow(self, resource: str) -> subprocess.CompletedProcess[str]:
        return sbx(["policy", "allow", "network", "--sandbox", self.name, resource])

    def remove_rule(self, resource: str) -> subprocess.CompletedProcess[str]:
        return sbx(
            [
                "policy",
                "rm",
                "network",
                "--sandbox",
                self.name,
                "--resource",
                resource,
                "-f",
            ]
        )


def create_sandbox(
    label: str,
    workspaces: Sequence[str],
    *,
    agent: str = "shell",
    extra: Sequence[str] = (),
) -> tuple[Sandbox, subprocess.CompletedProcess[str]]:
    """Create a sandbox and return it with the create result.

    The caller removes it, also when creation failed part-way.
    """
    sandbox = Sandbox(new_sandbox_name(label))
    created = sbx(
        ["create", "--name", sandbox.name, "--no-progress", *extra, agent, *workspaces],
        timeout=CREATE_TIMEOUT,
    )
    return sandbox, created


@functools.cache
def load_learning_template() -> str:
    """Build the thin sbx layer over workmux's claude image and load it into sbx."""
    subprocess.run(
        ["docker", "build", "-q", "-t", TEMPLATE_TAG, str(FIXTURE_DIR)],
        check=True,
        capture_output=True,
    )
    tar = (
        Path(os.environ.get("TMPDIR", "/tmp")) / f"wm-learn-{uuid.uuid4().hex[:6]}.tar"
    )
    try:
        subprocess.run(
            ["docker", "save", TEMPLATE_TAG, "-o", str(tar)],
            check=True,
            capture_output=True,
        )
        loaded = sbx(["template", "load", str(tar)], timeout=CREATE_TIMEOUT)
        assert loaded.returncode == 0, loaded.stderr
    finally:
        tar.unlink(missing_ok=True)
    return TEMPLATE_TAG
