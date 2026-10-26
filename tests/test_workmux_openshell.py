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
from .support.openshell import (
    GH_IMAGE,
    LEARNING_IMAGE,
    build_gh_image,
    build_learning_image,
    openshell,
)
from .test_workmux_remote import (
    SidebarFeed,
    agent_in,
    openshell_sandbox_name,
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
    """An OpenShell project named `wm-e20-…`, in the sandbox workmux names
    after its workstation clone (`wm-e20-…-<hash>`)."""

    def __init__(self, env: MuxEnvironment, exe: Path, repo: Path, tmp: dict):
        self.name = f"wm-e20-{uuid.uuid4().hex[:4]}"
        self.env, self.exe, self.repo = env, exe, repo
        self.configured = False
        self.tmp = {
            **tmp,
            **({"WORKMUX_OPENSHELL_WORKMUX": LINUX_WORKMUX} if LINUX_WORKMUX else {}),
            "DOCKER_CONFIG": str(Path.home() / ".docker"),
        }
        # The test's isolated HOME must still reach the user's gateway.
        config = env.home_path / ".config"
        config.mkdir(parents=True, exist_ok=True)
        (config / "openshell").symlink_to(Path.home() / ".config" / "openshell")

    def configure(self, origin: str, image: str, providers=()):
        """A workstation repository named after the project, with `origin`,
        whose `.workmux.yaml` puts it in an OpenShell sandbox of `image`
        (ADR 0004); `providers` go in global config, the only place they
        may be set. Commands run in it from now on."""
        repo = self.env.tmp_path / "workstation" / self.name
        repo.mkdir(parents=True)
        self.env.run_command(["git", "init", "-q", "-b", "main"], cwd=repo)
        self.env.run_command(["git", "remote", "add", "origin", origin], cwd=repo)
        (repo / ".workmux.yaml").write_text(
            "nerdfont: false\nsandbox:\n  enabled: true\n  backend: openshell\n"
            f"  openshell:\n    image: {image}\n"
        )
        if providers:
            config = self.env.home_path / ".config" / "workmux" / "config.yaml"
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(
                f"sandbox:\n  openshell:\n    providers: [{', '.join(providers)}]\n"
            )
        self.repo = repo
        self.configured = True

    @property
    def sandbox(self) -> str:
        """The sandbox workmux names after the project's clone."""
        assert self.configured, "configure() names the clone the sandbox is named after"
        return openshell_sandbox_name(self.repo)

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
            ["sandbox", "exec", "-n", self.sandbox, "--no-tty", "--no-login-shell"]
            + ["--", "sh", "-c", script]
        )
        assert result.returncode == 0, result.stderr
        return result.stdout

    def phase(self):
        result = openshell(["sandbox", "get", self.sandbox, "-o", "json"])
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
        if not self.configured:
            return
        openshell(["sandbox", "delete", self.sandbox], timeout=300)
        deadline = time.monotonic() + 120
        while self.phase() is not None and time.monotonic() < deadline:
            time.sleep(2)
        images = subprocess.run(
            ["docker", "image", "ls", "-q", f"workmux-openshell-{self.sandbox}"],
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
    project.configure("/sandbox/origin", LEARNING_IMAGE)
    started = project.wm("remote start", timeout=600.0)
    assert f"Creating remote {project.sandbox} on first use" in started.stderr
    assert project.phase() == "Ready"
    project.make_origin()


def test_listing_a_new_openshell_project_creates_nothing(project: Project):
    """`list` in a repository whose sandbox doesn't exist yet says so, and
    neither builds an image nor creates the sandbox (ADR 0004: the first
    `workmux add` does)."""
    project.configure("/sandbox/origin", LEARNING_IMAGE)

    listed = project.wm("list", timeout=60.0)

    assert (
        f"No OpenShell sandbox for {project.name} yet; `workmux add <branch>` creates it"
        in listed.stdout
    ), listed
    assert "Creating remote" not in listed.stderr, listed.stderr
    assert project.phase() is None
    images = subprocess.run(
        ["docker", "image", "ls", "-q", f"workmux-openshell-{project.sandbox}"],
        capture_output=True,
        text=True,
    ).stdout.split()
    assert images == []


def test_an_openshell_project_lives_stops_resumes_and_is_removed_with_its_sandbox(
    project: Project,
):
    """add → agent status in the sidebar → stop → stale → start → reconnect
    → remove, with the sandbox created on first use and deleted on request."""
    env = project.env
    assert isinstance(env, TmuxEnvironment)
    add_project(project)
    status = project.wm("remote status").stdout
    assert "status:   reachable" in status, status
    assert "read-write: /tmp, /dev/null, /dev/pts, /dev/ptmx" in status, status

    daemon_pid = None
    feed = None
    try:
        project.wm("add feature", timeout=120.0)
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
        assert agent_in(snapshot, pane)["remote"]["remote"] == project.sandbox

        # Stopped only on request; the agent turns stale, and status never
        # starts the sandbox.
        stopped = project.wm("remote stop", timeout=300.0)
        assert "Stopped remote" in stopped.stdout
        feed.wait(
            lambda s: (
                agent_in(s, pane).get("remote", {}).get("stale")
                or not agent_in(s, pane)
            ),
            "the entry to turn stale",
            timeout=60.0,
        )
        status = project.wm("remote status").stdout
        assert "status:   stopped" in status, status
        project.wm("resurrect")
        assert project.phase() == "Stopped"

        # Started on request: the channel reconnects and the workspace is
        # intact.
        project.wm("remote start", timeout=300.0)
        status = project.wm("remote status").stdout
        assert "status:   reachable" in status, status
        listing = project.wm("list").stdout
        assert "feature" in listing, listing
        assert listing.splitlines()[0] == (
            f"Worktrees in OpenShell sandbox {project.sandbox} (project {project.name})"
        ), listing

        # A command that needs a stopped sandbox starts it itself.
        project.wm("remote stop", timeout=300.0)
        path = project.wm("path feature", timeout=300.0)
        assert path.stdout.strip() == f"/sandbox/{project.name}__worktrees/feature"
        assert "Starting remote" in path.stderr

        project.wm("remove feature --force", timeout=120.0)
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
    kept = project.wm("remote rm", stdin="n\n")
    assert "branch spike: 1 commit not on any remote" in kept.stdout, kept.stdout
    assert project.phase() == "Ready"
    removed = project.wm("remote rm --yes", timeout=300.0)
    assert "Deleted sandbox" in removed.stdout
    assert project.phase() is None


def test_a_recreate_carries_a_worktree_and_its_commit_across(project: Project):
    """Commit on a worktree branch, recreate: the branch and the worktree are
    back with the commit, in a new sandbox."""
    add_project(project)
    project.wm("add feature", timeout=120.0)
    worktree = f"/sandbox/{project.name}__worktrees/feature"
    project.sh(
        f"cd {worktree} && echo work > work.txt && git add work.txt && "
        "git -c user.name=t -c user.email=t@t commit -q -m work"
    )
    head = project.sh(f"git -C {worktree} rev-parse HEAD").strip()
    project.sh(f"echo draft > {worktree}/draft.txt")
    before = json.loads(
        openshell(["sandbox", "get", project.sandbox, "-o", "json"]).stdout
    )["id"]

    refused = project.wm("remote recreate --yes", expect_fail=True, timeout=120.0)
    assert "uncommitted changes" in refused.stderr, refused.stderr
    assert f"{worktree} (branch feature)" in refused.stderr, refused.stderr

    project.sh(f"rm {worktree}/draft.txt")
    project.wm("remote recreate --yes", timeout=900.0)

    after = json.loads(
        openshell(["sandbox", "get", project.sandbox, "-o", "json"]).stdout
    )["id"]
    assert after != before
    assert project.sh(f"git -C {worktree} rev-parse HEAD").strip() == head
    assert project.sh(f"git -C {worktree} branch --show-current").strip() == "feature"
    assert project.sh(f"cat {worktree}/work.txt").strip() == "work"
    assert (
        project.sh(f"git -C /sandbox/{project.name} remote get-url origin").strip()
        == "/sandbox/origin"
    )


def test_a_projects_lima_sandbox_is_ignored_inside_its_openshell_sandbox(
    project: Project,
):
    """A committed `.workmux.yaml` enabling workmux's Lima sandbox still adds
    a worktree and its agent pane: the OpenShell sandbox is the sandbox."""
    add_project(project)
    project.sh(
        "set -e; "
        "printf 'sandbox:\\n  enabled: true\\n  backend: lima\\n' "
        ">> /sandbox/origin/.workmux.yaml; "
        "git -C /sandbox/origin -c user.name=t -c user.email=t@t "
        "commit -q -am 'lima sandbox'"
    )

    added = project.wm("add feature", timeout=120.0)

    output = added.stdout + added.stderr
    assert "limactl" not in output, output
    assert (
        "the sandbox: setting is ignored inside OpenShell sandbox "
        f"{project.sandbox}" in output
    ), output
    wait_until(
        lambda: (
            project.sh("test -f /sandbox/control/started && echo yes || true") != ""
        ),
        "the agent to start",
        timeout=60.0,
    )


def test_an_openshell_sandbox_follows_the_sandbox_settings_of_every_backend(
    project: Project,
):
    """`env_passthrough` reaches the agent pane with the workstation's value;
    `extra_mounts` is ignored with a note, since OpenShell never mounts."""
    add_project(project)
    config = project.env.home_path / ".config" / "workmux" / "config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        "sandbox:\n  env_passthrough: [WM_E20_PASSED]\n  extra_mounts: [/tmp]\n"
    )
    project.tmp["WM_E20_PASSED"] = "from the workstation"
    project.sh(
        "set -e; "
        "printf 'panes:\\n  - command: printenv WM_E20_PASSED "
        "> /sandbox/control/passed; sh /sandbox/agent.sh\\n' "
        "> /sandbox/origin/.workmux.yaml; "
        "git -C /sandbox/origin -c user.name=t -c user.email=t@t "
        "commit -q -am 'passthrough pane'"
    )

    added = project.wm("add feature", timeout=120.0)

    output = added.stdout + added.stderr
    assert (
        f"Note: extra_mounts is ignored for OpenShell sandbox {project.sandbox}"
        in output
    ), output
    wait_until(
        lambda: project.sh("cat /sandbox/control/passed 2>/dev/null || true") != "",
        "the agent pane to start",
        timeout=60.0,
    )
    assert project.sh("cat /sandbox/control/passed") == "from the workstation\n"


def test_an_openshell_projects_sandbox_runs_claude_code(project: Project):
    """The image installs the default agent, Claude Code, where OpenShell's
    claude-code provider profile expects it, and `add` starts it in the
    sandbox with no credentials attached."""
    env = project.env
    project.configure("/sandbox/origin", LEARNING_IMAGE)
    started = project.wm("remote start", timeout=900.0)
    assert (
        "no provider attached to the sandbox has a profile that allows /usr/local/bin/claude"
        in started.stderr
    ), started.stderr
    # A project with no panes of its own: its agent pane runs the agent.
    project.sh(
        "set -e; mkdir -p /sandbox/origin; git -C /sandbox/origin init -q -b main; "
        "echo api > /sandbox/origin/README; git -C /sandbox/origin add README; "
        "git -C /sandbox/origin -c user.name=t -c user.email=t@t commit -q -m initial"
    )
    status = project.wm("remote status").stdout
    assert "agent:    claude: installed at /usr/local/bin/claude" in status, status

    daemon_pid = None
    try:
        added = project.wm("add feature", timeout=120.0)
        daemon_pid = remote_daemon_ping(env, project.exe, project.repo, project.tmp)[
            "pid"
        ]
        # Provisioning installs the status hooks before Claude Code has
        # ever run, so before it has made its config directory.
        assert "No agents detected" not in added.stdout + added.stderr
        settings = json.loads(
            project.sh('cat "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json"')
        )
        assert "workmux set-window-status" in json.dumps(settings["hooks"]), settings
        # With no credentials Claude Code exits once it fails to reach
        # Anthropic, so check it started: it records its first start.
        wait_until(
            lambda: "firstStartTime" in project.sh("cat ~/.claude.json || true"),
            "claude to start in the sandbox",
            timeout=60.0,
        )
        assert "Claude Code" in project.sh("claude --version")
        project.wm("remove feature --force", timeout=120.0)
    finally:
        if daemon_pid is not None:
            os.kill(daemon_pid, 15)


PRIVATE_REPO = os.environ.get("WORKMUX_TEST_OPENSHELL_PRIVATE_REPO", "")
GITHUB_PROFILE = (
    Path(__file__).parent / "fixtures" / "openshell" / "github-profile.yaml"
)


@pytest.fixture
def github_provider(project: Project):
    """A provider holding the workstation's `gh` token, made from a copy of
    OpenShell's stock `github` profile (clone and fetch only); both are
    deleted afterwards, after the project's sandbox. The token goes through
    the environment only."""
    if not PRIVATE_REPO:
        pytest.skip("set WORKMUX_TEST_OPENSHELL_PRIVATE_REPO=<owner>/<repo>")
    token = subprocess.run(
        ["gh", "auth", "token", "--hostname", "github.com"],
        capture_output=True,
        text=True,
    ).stdout.strip()
    if not token:
        pytest.skip("needs a `gh` login on the workstation")
    name = f"wm-test-35-{uuid.uuid4().hex[:6]}"
    profile = Path(tempfile.mkdtemp(prefix="wm-")) / "github.yaml"
    profile.write_text(
        GITHUB_PROFILE.read_text().replace("\nid: github\n", f"\nid: {name}\n")
    )
    imported = openshell(
        ["provider", "profile", "import", "-f", str(profile), "--global"]
    )
    assert imported.returncode == 0, imported.stderr
    try:
        created = subprocess.run(
            ["openshell", "provider", "create", "--name", name, "--type", name]
            + ["--global-profile", "--credential", "GITHUB_TOKEN"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            env={**os.environ, "NO_COLOR": "1", "GITHUB_TOKEN": token},
        )
        assert created.returncode == 0, created.stderr
        yield name
    finally:
        # A provider attached to a sandbox can't be deleted.
        project.clean_up()
        openshell(["provider", "delete", name])
        openshell(["provider", "profile", "delete", "--global", name])
        shutil.rmtree(profile.parent, ignore_errors=True)


def test_a_private_ssh_github_repo_clones_over_https_with_the_providers_credentials(
    project: Project, github_provider: str
):
    """An SSH origin on github.com is rewritten to HTTPS, visibly; the
    image's credential helper hands git the provider's placeholder, so the
    first-use clone of a private repository succeeds, and SSH GitHub URLs
    typed in the sandbox go over HTTPS too."""
    ssh = f"git@github.com:{PRIVATE_REPO}.git"
    https = f"https://github.com/{PRIVATE_REPO}.git"
    project.configure(ssh, LEARNING_IMAGE, providers=[github_provider])
    started = project.wm("remote start", timeout=900.0)
    assert (
        "The repository's origin uses SSH, which OpenShell sandboxes don't support; the sandbox "
        f"reaches github.com over HTTPS with `{github_provider}` credentials instead."
    ) in started.stderr, started.stderr

    listed = project.wm("list", timeout=300.0)
    assert (
        f"Cloning {https} (rewritten from SSH: {ssh}) into /sandbox/{project.name}"
    ) in listed.stderr, listed.stderr
    assert project.sh(f"git -C /sandbox/{project.name} config remote.origin.url") == (
        f"{https}\n"
    )
    status = project.wm("remote status").stdout
    assert f"repo:     {https} (rewritten from SSH: {ssh})" in status, status
    for url in [ssh, f"ssh://git@github.com/{PRIVATE_REPO}.git"]:
        assert "HEAD" in project.sh(f"git ls-remote {shlex.quote(url)} HEAD")


@pytest.fixture
def gh_login(project: Project):
    """The workstation's real `gh` login, which the isolated HOME can't
    reach: a `gh` on PATH runs the real one with the real HOME (it holds no
    token). Writing a token to the yielded file stands in for a new login:
    `gh auth token` prints it instead. The provider and profile workmux
    creates on the gateway are deleted afterwards, after the project's
    sandbox, unless they were there before; a `gh-login` that was there
    gets the real login's token back."""
    if not PRIVATE_REPO:
        pytest.skip("set WORKMUX_TEST_OPENSHELL_PRIVATE_REPO=<owner>/<repo>")
    gh = shutil.which("gh")
    if not gh or subprocess.run([gh, "auth", "token"], capture_output=True).returncode:
        pytest.skip("needs a `gh` login on the workstation")
    shims = Path(tempfile.mkdtemp(prefix="wm-"))
    login = shims / "login"
    (shims / "gh").write_text(
        "#!/bin/sh\n"
        f'[ "$1 $2" = "auth token" ] && [ -f {shlex.quote(str(login))} ] '
        f"&& exec cat {shlex.quote(str(login))}\n"
        f'HOME={shlex.quote(str(Path.home()))} exec {shlex.quote(gh)} "$@"\n'
    )
    (shims / "gh").chmod(0o755)
    project.tmp["PATH"] = f"{shims}:{os.environ['PATH']}"
    provider_there = openshell(["provider", "get", "gh-login"]).returncode == 0
    profile_there = (
        "workmux-github"
        in openshell(["provider", "profile", "list", "-o", "json"]).stdout
    )
    try:
        yield login
    finally:
        project.clean_up()
        if provider_there:
            # A test may have left it a stand-in token.
            token = subprocess.run(
                [gh, "auth", "token"], capture_output=True, text=True
            ).stdout.strip()
            openshell(
                [
                    "provider",
                    "update",
                    "gh-login",
                    "--credential",
                    "GH_TOKEN",
                    "--wait",
                ],
                env={"GH_TOKEN": token},
            )
        else:
            openshell(["provider", "delete", "gh-login"])
        if not profile_there:
            openshell(["provider", "profile", "delete", "--global", "workmux-github"])
        shutil.rmtree(shims, ignore_errors=True)


def test_a_projects_sandbox_gets_the_workstations_gh_login(
    project: Project, gh_login: Path
):
    """Provider `gh-login` needs only a `gh` login: workmux keeps its token
    in that gateway provider, of its own full-access profile, so the
    first-use clone of a private repository succeeds and push is
    authorised."""
    https = f"https://github.com/{PRIVATE_REPO}.git"
    project.configure(https, LEARNING_IMAGE, providers=["gh-login"])
    started = project.wm("remote start", timeout=900.0)
    assert "provider gh-login gives the sandbox your gh login's GitHub access" in (
        started.stderr
    ), started.stderr
    project.wm("list", timeout=300.0)

    assert project.sh(f"git -C /sandbox/{project.name} rev-parse --verify HEAD")
    # An empty, no-op push, as git's child: 200 is authorised, 401 is not.
    probe = (
        "curl -s -o /dev/null -w %{http_code} -X POST -u x-access-token:$GH_TOKEN "
        "-H 'Content-Type: application/x-git-receive-pack-request' "
        f"--data-binary 0000 {https}/git-receive-pack"
    )
    pushed = project.sh(f"git -c alias.probe={shlex.quote('!' + probe)} probe")
    assert pushed == "200", pushed


def add_gh_login_project(project: Project):
    """A `gh-login` project of the private repository, from an image with
    `gh`, cloned and ready."""
    build_gh_image()
    project.configure(
        f"https://github.com/{PRIVATE_REPO}.git", GH_IMAGE, providers=["gh-login"]
    )
    project.wm("remote start", timeout=900.0)
    project.wm("list", timeout=300.0)


def gh_api(project: Project, args: str) -> tuple[str, str]:
    """`gh api` in the sandbox: the HTTP status line and the body."""
    out = project.sh(f"gh api -i {args} 2>/dev/null; true")
    head, _, body = out.replace("\r\n", "\n").partition("\n\n")
    return head.splitlines()[0], body


def test_a_gh_login_sandbox_is_authorised_to_open_pull_requests(
    project: Project, gh_login: Path
):
    """`gh pr create`'s GraphQL mutation and the REST call for a pull
    request both reach GitHub with the login's token: GitHub, not the
    proxy, rejects them, for an id that names no repository and an empty
    body. No pull request is created."""
    add_gh_login_project(project)

    status, body = gh_api(
        project,
        "graphql -f query="
        + shlex.quote(
            "mutation { createPullRequest(input: {repositoryId: "
            '"R_wm_no_such_repository", baseRefName: "main", '
            'headRefName: "wm-none", title: "never"}) { pullRequest { url } } }'
        ),
    )
    assert " 200" in status, (status, body)
    assert [e["type"] for e in json.loads(body)["errors"]] == ["NOT_FOUND"], body

    status, body = gh_api(
        project, f"-X POST repos/{PRIVATE_REPO}/pulls --input /dev/null"
    )
    assert " 422" in status, (status, body)


def test_a_new_gh_login_reaches_a_running_sandbox(project: Project, gh_login: Path):
    """A command that needs the sandbox gives `gh-login` the workstation's
    current `gh` token, and the running sandbox authenticates with it at
    once: a token GitHub rejects gets 401, the real login 200 again."""
    add_gh_login_project(project)
    assert gh_api(project, "user")[0].endswith(" 200 OK")

    gh_login.write_text("gho_wm_not_a_real_token\n")
    try:
        project.wm("list", timeout=300.0)
        status, body = gh_api(project, "user")
        assert " 401" in status, (status, body)
    finally:
        # Never leave `gh-login` the stand-in token.
        gh_login.unlink(missing_ok=True)
        project.wm("list", timeout=300.0)
    status, body = gh_api(project, "user")
    assert " 200" in status, (status, body)
