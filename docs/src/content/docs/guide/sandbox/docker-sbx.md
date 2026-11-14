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
| `toolchain`        | `auto`                                  | Toolchain mode: `auto` (detect devbox.json/flake.nix), `off`, `devbox`, or `flake` (see [below](#nix-and-devbox-toolchain))    |

Settings specific to sbx are nested under `sandbox.docker_sbx`:

```yaml
sandbox:
  enabled: true
  backend: docker_sbx
  docker_sbx:
    cpus: 8
    memory: 8g
    provision: |
      apt-get update && apt-get install -y ripgrep jq
```

| Option                              | Default                       | Description                                                                                     |
| ----------------------------------- | ----------------------------- | ----------------------------------------------------------------------------------------------- |
| `docker_sbx.cpus`                   | all host CPUs (sbx's default) | Number of CPUs for the sandbox (`0` means all)                                                  |
| `docker_sbx.memory`                 | half the host's memory        | Memory for the sandbox in sbx's binary units (`512m`, `8g`); at least `512m`                    |
| `docker_sbx.provision`              | -                             | Shell script run once, as root, when the sandbox is created (see [Provisioning](#provisioning)) |
| `docker_sbx.skip_default_provision` | `false`                       | Skip workmux's built-in provisioning (see [Provisioning](#provisioning))                        |
| `docker_sbx.kits`                   | `[]`                          | sbx mixin kits added when the sandbox is created (see [Kits](#kits)). **Global config only.**   |

sbx has no per-sandbox disk setting, so there is no `disk`. Resources, `provision` and `kits` apply when the sandbox is created; recreate it with `workmux sandbox prune` after changing them.

## How it works

`workmux add` creates the worktree's sandbox before its window opens, so setup progress and errors appear in your terminal. Each sandboxed pane then runs a supervisor process (`workmux sandbox run`) that:

1. Starts a TCP RPC server on the host's loopback interface, on a random port
2. Writes the host-command shims into the sandbox's state directory
3. Creates the sandbox if it doesn't exist yet, reconciles its [network rules](#network-restrictions), and allows its RPC port
4. Runs the agent command with `sbx exec -it` in the pane's working directory
5. Handles RPC requests from the guest workmux binary
6. When the pane closes, hangs up the processes its `sbx exec` started in the guest

The guest reaches the RPC server through `host.docker.internal`. sbx names the workstation `localhost` in its policy, so each supervisor adds a `localhost:<port>` rule for its own RPC port, scoped to the sandbox, and removes it when the pane closes. Other services on the workstation's loopback interface stay unreachable. workmux never changes your global sbx policy.

Everything that goes through the RPC server works as on the other backends: [host commands](/guide/sandbox/features/#host-command-proxying) (which run on your workstation, outside sbx's network policy and placeholders), [sound notifications](/guide/sandbox/features/#sound-notifications), [image paste](/guide/sandbox/features/#clipboard-proxy), status tracking, and [coordinator agents](/guide/sandbox/features/#coordinator-agents), whose `workmux add` gives each new worktree its own sandbox.

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

sbx creates sandboxes from templates kept in its own image store. workmux builds the template from `sandbox.image` plus a thin layer (`docker/Dockerfile.sbx`) that adds what sbx expects: a non-root `agent` user (UID 1000, home `/home/agent`), a directory for sbx's proxy certificate, and `BASH_ENV` pointing at `/etc/sandbox-persistent.sh`. The layer also switches the image's Debian package sources to HTTPS, since sbx's `balanced` policy refuses plain HTTP. When a sandbox is created, workmux builds the template with Docker, saves it and loads it with `sbx template load`. Templates are named `workmux-sbx:<image>-<hash>` (`sbx template ls`).

The template follows the image in Docker's store: when the image changes (a pulled update, a local build, `install-dev`), workmux builds and loads the template again the next time it creates a sandbox. As on the container backend, workmux checks the official `ghcr.io/raine/workmux-sandbox` images for updates in the background and pulls a newer one before the next template build; custom images are never pulled once present. Existing sandboxes keep the template they were created from; recreate them with `workmux sandbox prune` to pick up a new one.

The image commands handle the template too:

```bash
# Pull the image and refresh the template
workmux sandbox pull

# Build the image locally (with Docker) and refresh the template
workmux sandbox build

# Export a Dockerfile for a custom image, ending with the sbx layer
workmux sandbox init-dockerfile
```

With `backend: docker_sbx` they use Docker, whatever `sandbox.container.runtime` says. The Dockerfile from `init-dockerfile` ends with the sbx layer so a custom image already has what sbx needs; add root steps before its `USER agent`. workmux applies the layer again when it builds the template, which changes nothing in an image that has it.

### Authentication

Agents use the login in their mounted config directory, as on the container backend: Codex, Gemini, OpenCode, pi and OMP start logged in if you've logged in on the workstation. Claude stores its login in the macOS Keychain, which the sandbox can't reach. Sign in to Claude once inside a sandbox; the login is written to the mounted config directory and persists across sandboxes. See [Credentials](/guide/sandbox/features/#credentials).

### Tool credentials

Give tools such as `gh` and `git` their credentials with sbx's own secrets, not with `env` or `env_passthrough`:

```bash
# For every sandbox
sbx secret set github --command 'gh auth token'
# For one sandbox
sbx secret set github --sandbox wm-myproject-a1b2c3d4 --command 'gh auth token'
```

sbx keeps the secret on the workstation and its proxy adds it to requests for the service's domains, so the agent only ever holds a placeholder (such as `GH_TOKEN=gho_sbxproxy…`). `git` over HTTPS to GitHub needs no credential helper: the proxy authenticates it too. Services are `anthropic`, `copilot`, `cursor`, `devin`, `droid`, `github`, `google`, `groq`, `mistral`, `nebius`, `openai`, `openrouter` and `xai`; `sbx secret ls` lists the ones you've set. workmux never sets, reads or passes a secret: it reads only their names, with `sbx secret ls`.

A variable in `env` or `env_passthrough` that holds one of these credentials (`GH_TOKEN`, `GITHUB_TOKEN`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` and the like, as `sbx secret import` recognises them) replaces sbx's placeholder with the real value, which the agent can then read and send anywhere it can reach. When sbx holds a secret for that service, globally or for the sandbox, workmux warns at launch, naming the variable and `sbx secret set`; remove the variable from your config. When sbx holds no such secret, workmux says nothing.

sbx also sets git config of its own in the sandbox (keeping its `.sbx` directories out of `git status`); workmux keeps it alongside your [git identity](/guide/sandbox/features/#git-identity).

## Provisioning

The `provision` script runs once, after workmux creates the sandbox, sets it up (config directory links, Claude's onboarding state, network rules) and runs its [built-in provisioning](#built-in-provisioning), never on later launches. It runs as root with `bash -c`, in the worktree; the template has no `sudo`, so don't use it. Its downloads go through sbx's proxy and network policy like any other request in the sandbox; sbx's `balanced` policy reaches package mirrors over HTTPS only, which is why the [template](#template) switches the image's Debian sources to HTTPS. A changed script applies the next time the sandbox is created (`workmux sandbox prune`, or `workmux remove`). If the script fails, workmux deletes the sandbox and stops with its error, so the next launch creates and provisions it again.

```yaml
sandbox:
  backend: docker_sbx
  docker_sbx:
    provision: |
      apt-get update
      apt-get install -y ripgrep fd-find jq
```

### Built-in provisioning

Before the `provision` script, workmux runs its own built-in provisioning, as Lima's [default provisioning](/guide/sandbox/lima/#default-provisioning) does. The [template](#template) already contains the system packages, the agent CLI and workmux, so this only:

- writes `/etc/profile.d/workmux-shims.sh`, so login shells (Codex runs commands with `bash -lc`) keep the host-command shims on `PATH`
- installs Nix and Devbox when the worktree uses a [toolchain](#nix-and-devbox-toolchain)

If it fails, workmux deletes the sandbox and stops, as for `provision`; `sbx policy log <sandbox>` shows requests sbx blocked. Set `skip_default_provision: true` to skip it when your `sandbox.image` provides these already; `provision` runs either way. To provision once for every sandbox instead, build a custom `sandbox.image`.

## Nix and Devbox toolchain

`toolchain` works as on [Lima](/guide/sandbox/lima/#nix-and-devbox-toolchain): with `devbox.json` or `flake.nix` in the worktree (or `toolchain: devbox` or `flake`), workmux installs Nix and Devbox when it creates the sandbox and runs the agent command in the environment they declare (`devbox run` or `nix develop`). `toolchain: off` does neither. `workmux sandbox shell` runs its command outside the toolchain, as on Lima; [host commands](/guide/sandbox/features/#host-command-proxying) run in the workstation's toolchain environment, as on the other backends.

The Nix store lives in the sandbox, so packages download once per sandbox and are gone after `workmux sandbox prune`. Agent commands see the image's own `PATH`, with the shims, `~/.local/bin` and Nix in front.

Installing and using Nix and Devbox reaches these hosts, on port 443:

| Host                                                                             | Used for                                   |
| -------------------------------------------------------------------------------- | ------------------------------------------ |
| `install.determinate.systems`                                                    | Nix installer, and a binary cache          |
| `cache.nixos.org`                                                                | Nix binary cache                           |
| `get.jetify.com`, `get.jetpack.io`, `releases.jetify.com`, `releases.jetpack.io` | Devbox installer                           |
| `github.com`, `release-assets.githubusercontent.com`                             | Devbox's binary                            |
| `www.nixsearch.com`                                                              | Devbox package versions                    |
| `github.com`, `api.github.com`, `codeload.github.com`                            | nixpkgs, and flake inputs hosted on GitHub |

sbx's `balanced` global policy blocks most of them, so whenever the worktree uses a toolchain workmux adds a rule scoped to the sandbox for each, under `policy: allow` as well as `deny`, and reconciles them like the [network rules](#network-restrictions): turning the toolchain off removes them at the next launch. Under `deny` they widen what the sandbox reaches, GitHub included. A flake's other inputs (hosts other than GitHub) aren't covered: add them to `allowed_domains`.

## Kits

sbx [kits](https://docs.docker.com/ai/sandboxes/customize/use-kits/) package tools for sandboxes. `sandbox.docker_sbx.kits` adds mixin kits (`sbx create --kit`) to each sandbox when it's created: OCI references such as `docker.io/sbx/task:latest`, git URLs, directories or ZIP files. sbx composes them onto workmux's template.

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  backend: docker_sbx
  docker_sbx:
    kits:
      - docker.io/sbx/task:latest
```

A kit can add network rules, credentials and install steps to the sandbox. sbx adds a kit's network rules as rules scoped to the sandbox, so they widen its access beyond `network.allowed_domains`, and workmux leaves them alone (`sbx policy ls <sandbox>` lists them). `kits` is therefore a global-only setting: workmux ignores it in a project's `.workmux.yaml`, with a warning. Use only mixin kits: workmux creates sandboxes with sbx's `shell` agent itself, as described [above](#whats-mounted). sbx keeps composed kits in its template store (`sbx template ls`, as `sandbox-kit-assembled`).

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

Under `policy: deny`, workmux adds a rule scoped to the sandbox for port 443 of each allowed domain (`sbx policy ls <sandbox>` lists them), and for the [toolchain's hosts](#nix-and-devbox-toolchain) when the worktree uses one. A wildcard such as `*.github.com` becomes sbx's `**.github.com:443` (subdomains at any depth) with a deny for `github.com:443`, so the base domain stays out unless you list it too. IP literals are refused, as on the container backend. Under `policy: allow`, workmux adds none but the toolchain's.

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
