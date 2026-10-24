---
title: "OpenShell projects"
description: Give each project its own OpenShell sandbox, cloned from git, with no workstation mounts
---

An OpenShell project lives in an [OpenShell](https://github.com/NVIDIA/OpenShell) sandbox of its own. The sandbox is the project's remote, so projects are isolated from each other and from your workstation. Everything in [Remote projects](/guide/remote-projects/) applies; this page covers what is specific to OpenShell.

- The repository is cloned inside the sandbox, and its worktrees live beside the clone.
- workmux never requests a bind mount, so no workstation file is visible in the sandbox.
- The control channel runs over `openshell sandbox exec`, so the sandbox's network policy needs no rule for workmux.

## Add a project

```bash
workmux project add api --openshell --repo https://github.com/me/api.git \
  --image ghcr.io/me/api-dev:1 \
  --policy ./api-policy.yaml \
  --provider github
```

| Option            | Meaning                                                                                      |
| ----------------- | -------------------------------------------------------------------------------------------- |
| `--image`         | Image the sandbox is built from. It must contain git and dtach. Default: workmux's own image |
| `--policy FILE`   | Policy file merged on top of workmux's [base policy](#policy). Stored as an absolute path    |
| `--provider NAME` | OpenShell provider to attach, for credentials (repeatable). No other provider is attached    |
| `--agent NAME`    | Agent the image installs (repeatable); the first is the sandbox's default agent              |
| `--sandbox NAME`  | Sandbox name. Default: the project name                                                      |
| `--path`          | The clone's path in the sandbox. Default: `~/<name>`                                         |

OpenShell sandbox names allow only lowercase letters, digits and hyphens, up to 19 characters. When the project name doesn't fit, the sandbox is named with a prefix of it plus a short hash.

The settings are saved under the project's `openshell:` key in global config:

```yaml
projects:
  api:
    git_url: https://github.com/me/api.git
    path: ~/api
    openshell:
      image: ghcr.io/me/api-dev:1
      policy: /Users/me/api-policy.yaml
      providers: [github]
      agents: [claude]
```

The project's remote is implicit and has the project's name, so commands such as `workmux remote status api` work without a `remote add`.

Git credentials and agent API keys should come from OpenShell providers, so that secrets never sit in the sandbox environment.

## Lifecycle

workmux creates and starts the sandbox for you, and stops or deletes it only when you ask.

- **Create.** The first command that needs the project builds the image, creates the sandbox and clones the repository, with the notice `Creating remote api on first use; …`.
- **Start.** A command that needs a stopped sandbox starts it first (`Starting remote api…`).
- **Never in the background.** The sidebar, the dashboard, `wait`, `list --all`, `resurrect` and `reap-agents` never create or start a sandbox. `resurrect` and `reap-agents` skip a stopped one with a notice and don't count it as a failure.

```bash
workmux remote stop api      # stop the sandbox; the workspace survives
workmux remote start api     # start it again (creating it if missing)
workmux remote recreate api  # delete and recreate, carrying committed work across
workmux remote rm api        # delete the sandbox and unregister the project
```

**Stopping ends every process in the sandbox**, agents included. The worktrees and status files survive, but agents do not: after `remote start`, workstation panes find their remote panes ended and close. Reopen the worktree with `workmux open`.

### Deleting a sandbox

`workmux remote rm api` deletes the sandbox and everything in it, because nothing is mounted from your workstation. Before it asks for confirmation, it lists the work at risk, worktree by worktree and branch by branch:

- uncommitted changes
- commits that are on no remote-tracking branch, so exist only in the sandbox
- branches not merged into the project's base branch

It starts a stopped sandbox to check, and stops it again if you decline. Once the sandbox is deleted, it closes the project's workstation windows and removes the project from global config. `-y` skips the confirmation.

### Recreating a sandbox

Some changes need a new sandbox: a narrowed policy, a changed image, or a new devshell. workmux never recreates on its own; it warns and points at `workmux remote recreate`. A recreate:

1. refuses while any worktree has uncommitted changes, and names them
2. confirms, naming unmerged work, because agents' remote panes are lost (`-y` skips this)
3. bundles every ref and keeps the bundle in `$XDG_STATE_HOME/workmux/recreate/<sandbox>/` until the restore succeeds
4. deletes and creates the sandbox
5. restores the clone: branches, tags, remotes, upstreams and every worktree at its path
6. reinstalls agent hooks

If the restore fails, run `workmux remote recreate` again. It resumes from the kept bundle without deleting the sandbox again.

## Image

workmux builds the sandbox image with `docker build` on the Docker daemon your OpenShell gateway uses, so it is native to the sandbox's architecture. The image is tagged `workmux-openshell-<sandbox>:<hash>`, and is rebuilt when its inputs change.

- **Default image:** `ubuntu:24.04` with git, dtach, curl and workmux.
- **Agents:** every image gets the project's [agents](#agents-in-the-sandbox) that workmux can install.
- **Your image (`--image`):** used as the base as it is. It must contain git and dtach, because nothing can be installed in a running sandbox.

Every image gets a workmux matching yours at `/usr/local/bin/workmux`:

| Your workmux                       | The image gets                                                                         |
| ---------------------------------- | -------------------------------------------------------------------------------------- |
| `WORKMUX_OPENSHELL_WORKMUX` is set | That Linux binary, copied in                                                           |
| A build from a source checkout     | A Linux workmux built from that checkout, uncommitted files included, in a build stage |
| A release                          | The release of the same version, installed with workmux's install script               |

:::caution
Published workmux releases don't include remote support yet, so a release workmux gets an image whose workmux cannot serve the control channel. Until they do, build workmux from source, or point `WORKMUX_OPENSHELL_WORKMUX` at a Linux build.
:::

### Agents in the sandbox

The image installs the agents the project names, and the first one is the sandbox's default agent. Name them with `--agent` (repeatable) or `agents:` under the project's `openshell:` key. By default the sandbox gets your default agent's type: a workstation path such as `~/.local/bin/claude`, or a named agent with `type: claude`, means `claude`.

| Agent    | Installed at            | Provider type it needs |
| -------- | ----------------------- | ---------------------- |
| `claude` | `/usr/local/bin/claude` | `claude-code`          |
| `codex`  | `/usr/local/bin/codex`  | `codex`                |

These are the paths OpenShell's example provider profiles allow, so a provider made from one of those profiles gives the agent its credentials. Each agent is installed in a build stage of its own, so your `--image` needs no curl. Claude Code is the glibc build, so it needs a glibc base image. Codex is a static musl build and runs on any base. workmux can't install other agents, so `project add` warns and your `--image` must already contain them.

The sandbox runs the agent's plain CLI, not your workstation's agent command. The image records the default agent in `/etc/workmux/default-agent`. Remote workmux uses it when no config in the sandbox names an agent, and counts it as a configured agent, so a project without panes of its own gets an agent pane. A `--agent` given to `add` is passed to remote workmux as it is, so give it the agent's plain CLI (`-a codex`) rather than a workstation alias.

`project add --openshell` and `remote status` list each agent with the provider type it needs and which attached providers have that type. Both warn when none has it, and so does creating the sandbox:

```text
Agent claude: installed at /usr/local/bin/claude; its credentials come from a provider of type claude-code
warning: no provider of type claude-code is attached to the sandbox, so claude has no credentials there; create one with `openshell provider create --type claude-code` and list it in the project's `openshell: providers:`
```

The agent still starts without one, but it can't authenticate. The sandbox's network policy reaches the agent's endpoints only through its provider.

## Nix devshells

Nix cannot install or build inside an OpenShell sandbox. Instead, for a project with a `flake.nix`, workmux bakes the devshell into the image:

1. On your workstation, workmux shallow-clones the project's git URL.
2. A `nixos/nix` build stage builds the devshell and copies its closure into the image's `/nix/store`.
3. A wrapper at `/usr/local/bin/workmux-devshell` enters it. Agents start as `workmux-devshell bash -c '…'`.

The image is fixed when the sandbox is created. When the clone's `flake.lock` no longer matches the baked one, `remote status` warns, and `workmux remote recreate` rebuilds the image.

A devbox project gets no devshell in a sandbox. Put its toolchain in your `--image`. `project add --openshell` and `remote status` both say so.

## Policy

workmux applies a **base policy** to every project sandbox: OpenShell's default filesystem policy plus what workmux needs.

```yaml
version: 1
filesystem_policy:
  include_workdir: true
  read_only: [/bin, /usr, /lib, /proc, /dev/urandom, /etc, /var/log, /nix]
  read_write: [/tmp, /dev/null, /dev/pts, /dev/ptmx]
landlock:
  compatibility: best_effort
```

The PTY devices are there because the session holder allocates terminals, and `/nix` for the baked devshell. There are no network rules.

Your `--policy` file is merged on top:

- `filesystem_policy`: each list is the union of both, without duplicates. A full filesystem section is always written, so your file can't drop what workmux needs.
- `network_policies`: the rules of both. A rule name in both files is refused.
- Any other section: yours.

`workmux remote status api` shows the sandbox's effective policy (read-only and read-write paths, workdir, network rules) and warns when it differs from the project's.

When you edit the policy file, the next command brings a running sandbox in line. Network rules apply at once. Filesystem grants that are added take effect after `remote stop` and `remote start`. A grant that is removed needs `workmux remote recreate`, because OpenShell can't narrow a live sandbox's filesystem policy; workmux warns instead.

## Registering an OpenShell remote yourself

`workmux remote add NAME --transport openshell --target SANDBOX` registers an existing sandbox that you manage. workmux can start and stop it, but never creates it, and `remote rm` only unregisters it. To have workmux create and own the sandbox, use `project add --openshell` instead.
