---
title: "Docker Sandboxes backend"
description: Run agents in Docker Sandboxes (sbx) microVMs
---

workmux can run agents in [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (the `sbx` CLI). Each worktree gets its own sbx sandbox, a microVM with its own kernel, and sbx's host-side proxy enforces its network policy.

:::caution[Early support]
The `docker_sbx` backend currently runs the Claude agent only. Other agents fail with an error naming the agent; use the container or Lima backend for them.
:::

## Setup

### 1. Install and set up sbx

Install Docker Sandboxes by following [Docker's instructions](https://docs.docker.com/ai/sandboxes/), then sign in and choose a global network policy:

```bash
sbx login
sbx policy init   # choose allow-all, balanced or deny-all
```

workmux never runs `sbx login` or `sbx policy init` for you. It checks both before creating a sandbox and stops with an error naming the command to run.

workmux also needs Docker (`docker` on `PATH`) to build the sandbox template.

### 2. Enable in config

```yaml
# ~/.config/workmux/config.yaml or .workmux.yaml
sandbox:
  enabled: true
  backend: docker_sbx
```

## Prerequisites

workmux checks these before creating a sandbox and fails closed, never running the agent unsandboxed:

| Prerequisite                 | If missing                                                                                   |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| `sbx` installed              | Error with a link to the install instructions                                                |
| Signed in to Docker          | Error naming `sbx login`                                                                     |
| Global network policy chosen | Error naming `sbx policy init`                                                               |
| Supported host               | Error naming sbx's platforms: macOS 14+ on Apple silicon, Windows 11, Ubuntu 24.04+ with KVM |

## Configuration

The generic `sandbox` settings keep their meaning:

| Option             | Default                                 | Description                                                                                                                    |
| ------------------ | --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `backend`          | `container`                             | Set to `docker_sbx` for Docker Sandboxes                                                                                       |
| `image`            | `ghcr.io/raine/workmux-sandbox:<agent>` | Agent image the sbx template is built from                                                                                     |
| `rpc_host`         | `host.docker.internal`                  | Hostname the guest uses to reach workmux's RPC server. **Global config only.**                                                 |
| `agent_config_dir` | agent's default (`~/.claude/`)          | Host directory mounted as the agent's config directory. **Global config only.**                                                |
| `env_passthrough`  | `[]`                                    | Environment variables to pass through to the sandbox. **Global config only.**                                                  |
| `env`              | `{}`                                    | Environment variables to set with explicit values. **Global config only.**                                                     |
| `host_commands`    | `[]`                                    | Commands to proxy from the sandbox to the host via RPC (see [shared features](/guide/sandbox/features/#host-command-proxying)) |

Settings specific to sbx are nested under `sandbox.docker_sbx`.

## How it works

`workmux add` creates the worktree's sandbox before its window opens, so setup progress and errors appear in your terminal. Each sandboxed pane then runs a supervisor process (`workmux sandbox run`) that:

1. Starts a TCP RPC server on the host's loopback interface, on a random port
2. Writes the host-command shims into the sandbox's state directory
3. Creates the sandbox if it doesn't exist yet, and adds its policy rule
4. Runs the agent command with `sbx exec -it` in the pane's working directory
5. Handles RPC requests from the guest workmux binary

The guest reaches the RPC server through `host.docker.internal`. sbx names the workstation `localhost` in its policy, so workmux adds one `localhost` rule scoped to each sandbox (`sbx policy ls <sandbox>` lists it). The rule lets that sandbox reach services listening on the workstation's loopback interface. workmux never changes your global sbx policy.

Closing a pane ends only that pane's `sbx exec`. sbx stops an idle sandbox about 30 seconds after its last session ends, and the next pane starts it again.

### What's mounted

sbx mounts each directory at its own host path:

| Mount                                          | Access     | Purpose                                                                      |
| ---------------------------------------------- | ---------- | ---------------------------------------------------------------------------- |
| Worktree                                       | read-write | The worktree the agent works in                                              |
| Main repository's `.git`                       | read-write | Git data for commits in a linked worktree                                    |
| Agent config directory                         | read-write | Agent settings and credentials, linked to `/home/agent/.claude` in the guest |
| `~/.local/state/workmux/docker-sbx/<sandbox>/` | read-only  | Host-command shims                                                           |

Sandboxes are created with sbx's generic `shell` agent, never an agent kit: sbx's kits write the agent's settings and credentials into its home, which here is your mounted config directory. workmux runs the agent command itself, as on the container backend.

Your `~/.gitconfig` isn't mounted; workmux passes your git identity instead (see [Git identity](/guide/sandbox/features/#git-identity)).

### Sandbox naming

Sandboxes are named `wm-<project>-<hash>` (e.g. `wm-myproject-a1b2c3d4`), where the hash is of the worktree path, so every worktree gets its own sandbox. List them with `sbx ls`.

### Template

sbx creates sandboxes from templates kept in its own image store. workmux builds the template from `sandbox.image` plus a thin layer (`docker/Dockerfile.sbx`) that adds what sbx expects: a non-root `agent` user (UID 1000, home `/home/agent`), a directory for sbx's proxy certificate, and `BASH_ENV` pointing at `/etc/sandbox-persistent.sh`. The first time a sandbox needs it, workmux builds the template with Docker, saves it and loads it with `sbx template load`. Templates are named `workmux-sbx:<image>-<hash>` (`sbx template ls`).

### Authentication

Claude stores its login in the macOS Keychain, which the sandbox can't reach. Sign in to Claude once inside a sandbox; the login is written to the mounted config directory and persists across sandboxes. See [Credentials](/guide/sandbox/features/#credentials).
