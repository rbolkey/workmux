"""Drive the `openshell` CLI for env-gated OpenShell learning and contract tests.

Every sandbox created here is named with the `wm-learn-` prefix and deleted by
the fixture that created it; nothing here touches other sandboxes.
"""

import functools
import io
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import time
import uuid
from pathlib import Path
from dataclasses import dataclass
from typing import Optional, Sequence

import pytest

SANDBOX_PREFIX = "wm-learn-"
LEARNING_IMAGE = "wm-learn-openshell:latest"
DEVSHELL_IMAGE = "wm-learn-devshell:latest"
GH_IMAGE = "wm-learn-gh:latest"
FIXTURE_DIR = Path(__file__).parent.parent / "fixtures" / "openshell"
CHECKOUT = Path(__file__).parent.parent.parent
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
    env: Optional[dict[str, str]] = None,
) -> subprocess.CompletedProcess[str]:
    """Run an openshell CLI command and capture its output, never raising on
    failure. `env` adds to its environment, for credentials kept off argv."""
    return subprocess.run(
        ["openshell", *args],
        input=input,
        stdin=None if input is not None else subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=timeout,
        env={**os.environ, "NO_COLOR": "1", **(env or {})},
    )


def new_sandbox_name(label: str) -> str:
    """A unique name within OpenShell's 19-character limit on sandbox names."""
    return f"{SANDBOX_PREFIX}{label[:4]}-{uuid.uuid4().hex[:5]}"


@functools.cache
def build_learning_image() -> str:
    """Build the fixture image; OpenShell's stock image has no git or python3."""
    subprocess.run(
        ["docker", "build", "-q", "-t", LEARNING_IMAGE, str(FIXTURE_DIR)],
        check=True,
        capture_output=True,
    )
    return LEARNING_IMAGE


@functools.cache
def build_devshell_image() -> str:
    """Build the learning image with a flake devshell's closure baked into /nix.

    The closure is built by a nix builder stage on the workstation, so the
    sandbox never evaluates or builds anything.
    """
    build_learning_image()
    subprocess.run(
        ["docker", "build", "-q", "-t", DEVSHELL_IMAGE, str(FIXTURE_DIR / "devshell")],
        check=True,
        capture_output=True,
    )
    return DEVSHELL_IMAGE


@functools.cache
def build_gh_image() -> str:
    """Build the learning image with GitHub's `gh` installed."""
    build_learning_image()
    subprocess.run(
        ["docker", "build", "-q", "-t", GH_IMAGE, str(FIXTURE_DIR / "gh")],
        check=True,
        capture_output=True,
    )
    return GH_IMAGE


# Builds this checkout's workmux as a static Linux binary: Alpine's Rust
# targets musl, so it runs on any base image.
LINUX_WORKMUX_DOCKERFILE = """\
FROM docker.io/library/rust:1.98-alpine AS build
RUN apk add --no-cache musl-dev
COPY . /src
WORKDIR /src
RUN --mount=type=cache,id=workmux-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \\
    --mount=type=cache,id=workmux-cargo-target,target=/target,sharing=locked \\
    cargo build --locked --bin workmux --target-dir /target \\
    && cp /target/debug/workmux /workmux
FROM scratch
COPY --from=build /workmux /workmux
"""


@functools.cache
def build_linux_workmux() -> Path:
    """This checkout's workmux, working tree included, built for Linux in
    Docker: the workstation may have no cross-linker for `workmux sandbox
    install-dev` to build with. No image is tagged."""
    files = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=CHECKOUT,
        check=True,
        capture_output=True,
    ).stdout.split(b"\0")
    context = io.BytesIO()
    with tarfile.open(fileobj=context, mode="w") as tar:
        for name in filter(None, files):
            path = CHECKOUT / name.decode()
            if path.is_file():
                tar.add(path, arcname=name.decode())
        dockerfile = LINUX_WORKMUX_DOCKERFILE.encode()
        info = tarfile.TarInfo("Dockerfile.workmux-linux")
        info.size = len(dockerfile)
        tar.addfile(info, io.BytesIO(dockerfile))
    out = Path(tempfile.mkdtemp(prefix="wm-linux-workmux-"))
    subprocess.run(
        ["docker", "build", "-q", "-f", "Dockerfile.workmux-linux"]
        + ["--output", f"type=local,dest={out}", "-"],
        input=context.getvalue(),
        check=True,
        capture_output=True,
        env={**os.environ, "DOCKER_BUILDKIT": "1"},
    )
    return out / "workmux"


def install_linux_workmux(state_home: Path) -> None:
    """Put the Linux workmux where `workmux sandbox install-dev` installs it
    for OpenShell sandbox images, under the test's `XDG_STATE_HOME`.
    install-dev itself would also patch the user's container image and
    running Lima VMs."""
    dest = state_home / "workmux" / "openshell" / "workmux"
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(build_linux_workmux(), dest)


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
        return json.loads(result.stdout).get("phase")

    def wait_for_phase(self, wanted: str, timeout: float = READY_TIMEOUT) -> None:
        deadline = time.monotonic() + timeout
        seen: Optional[str] = None
        while time.monotonic() < deadline:
            seen = self.phase()
            if seen == wanted:
                return
            assert seen != "Error", f"{self.name} entered phase Error"
            time.sleep(2)
        raise AssertionError(f"{self.name} never reached {wanted}; last phase {seen!r}")

    def policy(self) -> dict:
        """The sandbox's effective policy as `sandbox get` reports it."""
        result = self.get()
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)["policy"]

    def set_policy(self, policy: str) -> subprocess.CompletedProcess[str]:
        """Replace the policy of the running sandbox and wait until it is loaded."""
        return openshell(["policy", "set", self.name, "--policy", policy, "--wait"])

    def stop(self) -> subprocess.CompletedProcess[str]:
        return openshell(["sandbox", "stop", self.name])

    def start(self) -> subprocess.CompletedProcess[str]:
        return openshell(["sandbox", "start", self.name])

    def restart(self) -> None:
        assert self.stop().returncode == 0
        self.wait_for_phase("Stopped")
        assert self.start().returncode == 0
        self.wait_for_phase("Ready")

    def delete(self) -> None:
        openshell(["sandbox", "delete", self.name], timeout=300)


def create_sandbox(
    label: str,
    *,
    image: Optional[str] = LEARNING_IMAGE,
    policy: Optional[str] = None,
    providers: Sequence[str] = (),
) -> tuple[Sandbox, subprocess.CompletedProcess[str]]:
    """Create a detached sandbox and return it with the create result.

    `image=None` uses OpenShell's stock image. A sandbox whose initial command
    exits goes to phase Completed and refuses exec, so the main process is
    always the detached default.
    """
    if image == LEARNING_IMAGE:
        build_learning_image()
    elif image == DEVSHELL_IMAGE:
        build_devshell_image()
    sandbox = Sandbox(new_sandbox_name(label))
    args = [
        "sandbox",
        "create",
        "--name",
        sandbox.name,
        "--no-tty",
        "--no-auto-providers",
        "--detach",
    ]
    if image is not None:
        args += ["--from", image]
    if policy is not None:
        args += ["--policy", policy]
    for provider in providers:
        args += ["--provider", provider]
    return sandbox, openshell(args, timeout=READY_TIMEOUT)


def workspace_dir(sandbox: Sandbox) -> str:
    """The sandbox's working directory, which the default policy makes read-write."""
    return sandbox.sh_ok("pwd").strip().splitlines()[-1]
