---
title: "Docker Sandboxes backend"
description: Run agents in Docker Sandboxes (sbx) microVMs
---

workmux can run agents in [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (the `sbx` CLI). Each worktree gets its own sbx sandbox, a microVM with its own kernel, and sbx's host-side proxy enforces its network policy.

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
| `agent_config_dir` | agent's default (e.g. `~/.claude/`)     | Host directory mounted as the agent's config directory. **Global config only.**                                                |
| `env_passthrough`  | `[]`                                    | Environment variables to pass through to the sandbox. **Global config only.**                                                  |
| `env`              | `{}`                                    | Environment variables to set with explicit values. **Global config only.**                                                     |
| `host_commands`    | `[]`                                    | Commands to proxy from the sandbox to the host via RPC (see [shared features](/guide/sandbox/features/#host-command-proxying)) |
| `extra_mounts`     | `[]`                                    | Additional host paths to mount (see [shared features](/guide/sandbox/features/#extra-mounts)). **Global config only.**         |
| `network`          | `policy: allow`                         | Network restrictions, enforced by sbx (see [below](#network-restrictions)). **Global config only.**                            |

Settings specific to sbx are nested under `sandbox.docker_sbx`.

## How it works

`workmux add` creates the worktree's sandbox before its window opens, so setup progress and errors appear in your terminal. Each sandboxed pane then runs a supervisor process (`workmux sandbox run`) that:

1. Starts a TCP RPC server on the host's loopback interface, on a random port
2. Writes the host-command shims into the sandbox's state directory
3. Creates the sandbox if it doesn't exist yet, reconciles its [network rules](#network-restrictions), and allows its RPC port
4. Runs the agent command with `sbx exec -it` in the pane's working directory
5. Handles RPC requests from the guest workmux binary
6. When the pane closes, hangs up the processes its `sbx exec` started in the guest

The guest reaches the RPC server through `host.docker.internal`. sbx names the workstation `localhost` in its policy, so each supervisor adds a `localhost:<port>` rule for its own RPC port, scoped to the sandbox, and removes it when the pane closes. Other services on the workstation's loopback interface stay unreachable. workmux never changes your global sbx policy.

### Lifecycle

The sandbox belongs to the worktree, not to a pane:

- With `target: all`, every pane of the worktree runs in the worktree's one sandbox, so shells and agents see the same environment.
- Closing a pane or window, or the agent exiting, ends only that pane's session. sbx keeps a session's processes running when its `sbx exec` goes away, so the supervisor hangs them up itself; other panes' sessions keep running.
- sbx stops an idle sandbox about 30 seconds after its last session ends. The next pane, `workmux open` or `workmux sandbox shell` starts it again in about a second; there's no separate start step.
- `workmux remove` (and `workmux merge`, which removes the worktree) deletes the worktree's sandbox, with its policy rules and any sbx secrets scoped to it.

### What's mounted

sbx mounts each directory at its own host path:

| Mount                                          | Access       | Purpose                                                                       |
| ---------------------------------------------- | ------------ | ----------------------------------------------------------------------------- |
| Worktree                                       | read-write   | The worktree the agent works in                                               |
| Main repository's `.git`                       | read-only    | Git control policy and administrative namespace                               |
| Git objects, refs, logs                        | read-write   | Commits and branch updates                                                    |
| Current worktree admin                         | read-write\* | Index, HEAD, merge, and rebase state                                          |
| Agent config directory                         | read-write   | Agent settings and credentials, linked to its usual place under `/home/agent` |
| OpenCode's `~/.config/opencode/` (opencode)    | read-only    | OpenCode's global config, linked to `/home/agent/.config/opencode`            |
| `~/.local/state/workmux/docker-sbx/<sandbox>/` | read-only    | Host-command shims                                                            |
| `extra_mounts` entries                         | read-only\*  | User-configured paths                                                         |

\* As on the [container backend](/guide/sandbox/container/#whats-mounted), git pointer files, per-worktree config, object information, submodules' hooks and config, and files the repository config includes stay read-only within writable git mounts, so an agent can't plant hooks or change git config. sbx enforces read-only mounts on the host, even against root in the sandbox. Sandboxes require a linked worktree: workmux refuses to sandbox the main worktree, whose admin files are the repository's own. Hooks in the main repository must be regular files, not symbolic or hard links.

See [Credentials](/guide/sandbox/features/#credentials) for each agent's config directory. The links are made when the sandbox is created, so switching `agent` needs a new sandbox (`workmux sandbox prune`). For Claude, workmux also writes the sandbox's own `~/.claude.json` with onboarding marked complete. Pi and OMP download Linux `fd` and `rg` binaries into their config directory; workmux binds a directory inside the sandbox over `~/.pi/agent/bin/` and `~/.omp/agent/tools/` each time a pane starts, so the workstation's binaries are never overwritten.

sbx can mount a host path only at that same path, and a single file only read-only, so under `docker_sbx` an `extra_mounts` entry with a different `guest_path`, or a writable entry that is a file, is an error. sbx can't hide a file either: a read-only file mount leaves its contents readable. `sandbox.container.excluded_files` is therefore an error under `docker_sbx`, as it is on Apple Container, rather than leaving secrets readable.

Sandboxes are created with sbx's generic `shell` agent, never an agent kit: sbx's kits write the agent's settings and credentials into its home, which here is your mounted config directory. workmux runs the agent command itself, as on the container backend.

Mounts are fixed when the sandbox is created. After changing `extra_mounts`, recreate the sandbox with `workmux sandbox prune`; sandboxes created before workmux protected the git metadata need the same.

Your `~/.gitconfig` isn't mounted; workmux passes your git identity instead (see [Git identity](/guide/sandbox/features/#git-identity)).

### Sandbox naming

Sandboxes are named `wm-<project>-<hash>` (e.g. `wm-myproject-a1b2c3d4`), where the hash is of the worktree path, so every worktree gets its own sandbox. List them with `sbx ls`.

### Template

sbx creates sandboxes from templates kept in its own image store. workmux builds the template from `sandbox.image` plus a thin layer (`docker/Dockerfile.sbx`) that adds what sbx expects: a non-root `agent` user (UID 1000, home `/home/agent`), a directory for sbx's proxy certificate, and `BASH_ENV` pointing at `/etc/sandbox-persistent.sh`. The first time a sandbox needs it, workmux builds the template with Docker, saves it and loads it with `sbx template load`. Templates are named `workmux-sbx:<image>-<hash>` (`sbx template ls`).

### Authentication

Agents use the login in their mounted config directory, as on the container backend: Codex, Gemini, OpenCode, pi and OMP start logged in if you've logged in on the workstation. Claude stores its login in the macOS Keychain, which the sandbox can't reach. Sign in to Claude once inside a sandbox; the login is written to the mounted config directory and persists across sandboxes. See [Credentials](/guide/sandbox/features/#credentials).

## Network restrictions

`sandbox.network` means the same as on the [container backend](/guide/sandbox/container/#network-restrictions), and sbx enforces it. It's a global-only setting.

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  enabled: true
  backend: docker_sbx
  network:
    policy: deny
    allowed_domains:
      - api.anthropic.com
      - "*.github.com"
```

Under `policy: deny`, workmux adds a rule scoped to the sandbox for port 443 of each allowed domain (`sbx policy ls <sandbox>` lists them). A wildcard such as `*.github.com` becomes sbx's `**.github.com:443` (subdomains at any depth) with a deny for `github.com:443`, so the base domain stays out unless you list it too. IP literals are refused, as on the container backend. Under `policy: allow`, workmux adds no rules.

Every launch reconciles the rules: it adds missing ones and removes the ones workmux added earlier that your config no longer lists, so a removed domain stops working at the sandbox's next launch. workmux records the rules it added in the sandbox's state directory and never touches rules you added yourself, or your global sbx policy.

Differences from the container backend:

- **The global sbx policy still applies.** sbx allows a request when your global policy or a sandbox rule allows it, and a deny in either wins. Under `policy: allow`, what your global policy blocks stays blocked. Under `policy: deny`, the sandbox can also reach whatever your global policy allows (sbx's `balanced` policy allows package registries, code hosts and AI services); workmux prints a note saying how many destinations that is. Choose `deny-all` with `sbx policy init` for a sandbox that reaches only `allowed_domains`.
- **Private addresses.** sbx doesn't refuse private or loopback addresses an allowed domain resolves to, so `allow_private_ips` changes nothing under `docker_sbx`.
- **`max_connections`** is ignored: sbx has no cap on concurrent connections. workmux prints a note when it's set.

sbx logs requests it blocks: `sbx policy log <sandbox>` lists them with the rule or reason. They don't appear in workmux's log.

## Sandbox management

`workmux sandbox stop`, `prune` and `shell` act on sbx sandboxes when the backend is `docker_sbx`, as they do on Lima VMs:

```bash
# Stop running workmux sandboxes (interactive list, a name, or --all; -y skips the prompt)
workmux sandbox stop
workmux sandbox stop wm-myproject-abc12345
workmux sandbox stop --all --yes

# Delete all workmux sandboxes (wm-* in `sbx ls`), after confirmation
workmux sandbox prune
workmux sandbox prune --force

# Open a shell in the current worktree's sandbox (creating it if needed)
workmux sandbox shell
workmux sandbox shell -- git status
```

A stopped sandbox keeps its state and starts again when a pane needs it. A pruned sandbox is created again by the next `workmux open`. `workmux sandbox shell` has no RPC server, so host commands and status updates don't work in it; `--exec` means the same as without it, since the sandbox is already persistent.
