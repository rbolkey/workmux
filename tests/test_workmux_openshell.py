"""End-to-end tests of OpenShell projects against a real gateway.

Env-gated: `WORKMUX_TEST_OPENSHELL=1`. The workmux under test is a dev
build, so workmux builds a Linux workmux from this checkout into the sandbox
image; `WORKMUX_OPENSHELL_WORKMUX`, when set, points at a Linux build to use
instead. Each test owns one `wm-e20-…` project and sandbox, and deletes the
sandbox and the images built for it.
"""

import json
import os
import shlex
import shutil
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

import pytest

from .conftest import MuxEnvironment, TmuxEnvironment, run_workmux_command
from .support.openshell import LEARNING_IMAGE, build_learning_image, openshell
from .test_workmux_remote import (
    SidebarFeed,
    agent_in,
    remote_daemon_ping,
    tmux_value,
    wait_until,
)

LINUX_WORKMUX = os.environ.get("WORKMUX_OPENSHELL_WORKMUX", "")

pytestmark = [
    pytest.mark.skipif(
        os.environ.get("WORKMUX_TEST_OPENSHELL") != "1",
        reason="set WORKMUX_TEST_OPENSHELL=1",
    ),
    pytest.mark.tmux_only,
]

# A stand-in agent in the sandbox: runs each status hook the test drops
# into /sandbox/control/next.
AGENT = """\
mkdir -p /sandbox/control
touch /sandbox/control/started
while :; do
  if [ -e /sandbox/control/next ]; then
    status=$(cat /sandbox/control/next); rm /sandbox/control/next
    workmux set-window-status "$status"
    echo "$status" >> /sandbox/control/done
  fi
  sleep 0.05
done
"""


class Project:
    """An OpenShell project named `wm-e20-…`, whose sandbox has its name."""

    def __init__(self, env: MuxEnvironment, exe: Path, repo: Path, tmp: dict):
        self.name = f"wm-e20-{uuid.uuid4().hex[:4]}"
        self.env, self.exe, self.repo = env, exe, repo
        self.tmp = {
            **tmp,
            **({"WORKMUX_OPENSHELL_WORKMUX": LINUX_WORKMUX} if LINUX_WORKMUX else {}),
            "DOCKER_CONFIG": str(Path.home() / ".docker"),
        }
        # The test's isolated HOME must still reach the user's gateway.
        config = env.home_path / ".config"
        config.mkdir(parents=True, exist_ok=True)
        (config / "openshell").symlink_to(Path.home() / ".config" / "openshell")

    def wm(self, command: str, expect_fail=False, timeout=60.0, stdin=None):
        return run_workmux_command(
            self.env,
            self.exe,
            self.repo,
            command,
            pre_run_env=self.tmp,
            expect_fail=expect_fail,
            timeout=timeout,
            stdin_input=stdin,
        )

    def sh(self, script: str) -> str:
        result = openshell(
            ["sandbox", "exec", "-n", self.name, "--no-tty", "--no-login-shell"]
            + ["--", "sh", "-c", script]
        )
        assert result.returncode == 0, result.stderr
        return result.stdout

    def phase(self):
        result = openshell(["sandbox", "get", self.name, "-o", "json"])
        if result.returncode != 0:
            return None
        return json.loads(result.stdout)["phase"]

    def make_origin(self):
        """A repository in the sandbox that the project clones on first use."""
        self.sh(
            "set -e; mkdir -p /sandbox/origin /sandbox/control; cd /sandbox/origin; "
            "git init -q -b main; "
            f"printf %s {shlex.quote(AGENT)} > /sandbox/agent.sh; "
            "printf 'panes:\\n  - command: sh /sandbox/agent.sh\\n' > .workmux.yaml; "
            "git add .workmux.yaml; "
            "git -c user.name=t -c user.email=t@t commit -q -m initial"
        )

    def hook(self, status: str):
        before = self.sh("cat /sandbox/control/done 2>/dev/null | wc -l").strip()
        self.sh(f"echo {status} > /sandbox/control/next")
        wait_until(
            lambda: (
                self.sh("cat /sandbox/control/done 2>/dev/null | wc -l").strip()
                != before
            ),
            f"the {status} hook",
            timeout=30.0,
        )

    def clean_up(self):
        openshell(["sandbox", "delete", self.name], timeout=300)
        deadline = time.monotonic() + 120
        while self.phase() is not None and time.monotonic() < deadline:
            time.sleep(2)
        images = subprocess.run(
            ["docker", "image", "ls", "-q", f"workmux-openshell-{self.name}"],
            capture_output=True,
            text=True,
        ).stdout.split()
        for image in images:
            subprocess.run(["docker", "rmi", "-f", image], capture_output=True)


@pytest.fixture
def short_tmp():
    """A short TMPDIR, for the workstation daemon's socket."""
    path = tempfile.mkdtemp(prefix="wm-", dir="/tmp")
    yield {"TMPDIR": path}
    shutil.rmtree(path, ignore_errors=True)


@pytest.fixture
def project(mux_server, workmux_exe_path, repo_path, short_tmp):
    build_learning_image()
    project = Project(mux_server, workmux_exe_path, repo_path, short_tmp)
    yield project
    project.clean_up()


def add_project(project: Project):
    added = project.wm(
        f"project add {project.name} --openshell --repo /sandbox/origin "
        f"--image {LEARNING_IMAGE}"
    )
    assert "the sandbox is created on first use" in added.stdout
    started = project.wm(f"remote start {project.name}", timeout=600.0)
    assert f"Creating remote {project.name} on first use" in started.stderr
    assert project.phase() == "Ready"
    project.make_origin()


def test_an_openshell_project_lives_stops_resumes_and_is_removed_with_its_sandbox(
    project: Project,
):
    """add → agent status in the sidebar → stop → stale → start → reconnect
    → remove, with the sandbox created on first use and deleted on request."""
    env = project.env
    assert isinstance(env, TmuxEnvironment)
    add_project(project)
    status = project.wm(f"remote status {project.name}").stdout
    assert "status:   reachable" in status, status
    assert "read-write: /tmp, /dev/null, /dev/pts, /dev/ptmx" in status, status

    daemon_pid = None
    feed = None
    try:
        project.wm(f"add feature --project {project.name}", timeout=120.0)
        daemon_pid = remote_daemon_ping(env, project.exe, project.repo, project.tmp)[
            "pid"
        ]
        pane = tmux_value(env, f"={project.name}:wm-feature", "#{pane_id}")
        feed = SidebarFeed(project.tmp["TMPDIR"])
        wait_until(
            lambda: (
                project.sh("test -f /sandbox/control/started && echo yes || true") != ""
            ),
            "the agent to start",
            timeout=60.0,
        )

        project.hook("working")
        snapshot = feed.wait(
            lambda s: agent_in(s, pane).get("status") == "working",
            "the sandbox agent working",
            timeout=60.0,
        )
        assert agent_in(snapshot, pane)["remote"]["remote"] == project.name

        # Stopped only on request; the agent turns stale, and status never
        # starts the sandbox.
        stopped = project.wm(f"remote stop {project.name}", timeout=300.0)
        assert "Stopped remote" in stopped.stdout
        feed.wait(
            lambda s: (
                agent_in(s, pane).get("remote", {}).get("stale")
                or not agent_in(s, pane)
            ),
            "the entry to turn stale",
            timeout=60.0,
        )
        status = project.wm(f"remote status {project.name}").stdout
        assert "status:   stopped" in status, status
        project.wm("resurrect")
        assert project.phase() == "Stopped"

        # Started on request: the channel reconnects and the workspace is
        # intact.
        project.wm(f"remote start {project.name}", timeout=300.0)
        status = project.wm(f"remote status {project.name}").stdout
        assert "status:   reachable" in status, status
        listing = project.wm(f"list --project {project.name}").stdout
        assert "feature" in listing, listing

        # A command that needs a stopped sandbox starts it itself.
        project.wm(f"remote stop {project.name}", timeout=300.0)
        path = project.wm(f"path feature --project {project.name}", timeout=300.0)
        assert path.stdout.strip() == f"/sandbox/{project.name}__worktrees/feature"
        assert "Starting remote" in path.stderr

        project.wm(f"remove feature --project {project.name} --force", timeout=120.0)
    finally:
        if feed is not None:
            feed.close()
        if daemon_pid is not None:
            os.kill(daemon_pid, 15)

    # Delete names work that exists only in the sandbox, and asks first.
    project.sh(
        f"cd /sandbox/{project.name} && git checkout -q -b spike && "
        "git -c user.name=t -c user.email=t@t commit -q --allow-empty -m wip && "
        "git checkout -q main"
    )
    kept = project.wm(f"remote rm {project.name}", stdin="n\n")
    assert "branch spike: 1 commit not on any remote" in kept.stdout, kept.stdout
    assert project.phase() == "Ready"
    removed = project.wm(f"remote rm {project.name} --yes", timeout=300.0)
    assert "Deleted sandbox" in removed.stdout
    assert project.phase() is None
    config = (env.home_path / ".config" / "workmux" / "config.yaml").read_text()
    assert project.name not in config


def test_a_recreate_carries_a_worktree_and_its_commit_across(project: Project):
    """Commit on a worktree branch, recreate: the branch and the worktree are
    back with the commit, in a new sandbox."""
    add_project(project)
    project.wm(f"add feature --project {project.name}", timeout=120.0)
    worktree = f"/sandbox/{project.name}__worktrees/feature"
    project.sh(
        f"cd {worktree} && echo work > work.txt && git add work.txt && "
        "git -c user.name=t -c user.email=t@t commit -q -m work"
    )
    head = project.sh(f"git -C {worktree} rev-parse HEAD").strip()
    project.sh(f"echo draft > {worktree}/draft.txt")
    before = json.loads(
        openshell(["sandbox", "get", project.name, "-o", "json"]).stdout
    )["id"]

    refused = project.wm(
        f"remote recreate {project.name} --yes", expect_fail=True, timeout=120.0
    )
    assert "uncommitted changes" in refused.stderr, refused.stderr
    assert f"{worktree} (branch feature)" in refused.stderr, refused.stderr

    project.sh(f"rm {worktree}/draft.txt")
    project.wm(f"remote recreate {project.name} --yes", timeout=900.0)

    after = json.loads(
        openshell(["sandbox", "get", project.name, "-o", "json"]).stdout
    )["id"]
    assert after != before
    assert project.sh(f"git -C {worktree} rev-parse HEAD").strip() == head
    assert project.sh(f"git -C {worktree} branch --show-current").strip() == "feature"
    assert project.sh(f"cat {worktree}/work.txt").strip() == "work"
    assert (
        project.sh(f"git -C /sandbox/{project.name} remote get-url origin").strip()
        == "/sandbox/origin"
    )


def test_an_openshell_projects_sandbox_runs_claude_code(project: Project):
    """The image installs the default agent, Claude Code, where OpenShell's
    claude-code provider profile expects it, and `add` starts it in the
    sandbox with no credentials attached."""
    env = project.env
    added = project.wm(
        f"project add {project.name} --openshell --repo /sandbox/origin "
        f"--image {LEARNING_IMAGE}"
    )
    assert "Agent claude: installed at /usr/local/bin/claude" in added.stdout, (
        added.stdout
    )
    assert (
        "no provider attached to the sandbox has a profile that allows /usr/local/bin/claude"
        in added.stderr
    ), added.stderr
    project.wm(f"remote start {project.name}", timeout=900.0)
    # A project with no panes of its own: its agent pane runs the agent.
    project.sh(
        "set -e; mkdir -p /sandbox/origin; git -C /sandbox/origin init -q -b main; "
        "echo api > /sandbox/origin/README; git -C /sandbox/origin add README; "
        "git -C /sandbox/origin -c user.name=t -c user.email=t@t commit -q -m initial"
    )
    status = project.wm(f"remote status {project.name}").stdout
    assert "agent:    claude: installed at /usr/local/bin/claude" in status, status

    daemon_pid = None
    try:
        project.wm(f"add feature --project {project.name}", timeout=120.0)
        daemon_pid = remote_daemon_ping(env, project.exe, project.repo, project.tmp)[
            "pid"
        ]
        wait_until(
            lambda: (
                project.sh(
                    "for p in /proc/[0-9]*; do cat $p/comm 2>/dev/null; done "
                    "| grep -x claude || true"
                ).strip()
                != ""
            ),
            "claude to run in the sandbox",
            timeout=60.0,
        )
        assert "Claude Code" in project.sh("claude --version")
        project.wm(f"remove feature --project {project.name} --force", timeout=120.0)
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, 15)
