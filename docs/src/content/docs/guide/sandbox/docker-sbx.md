---
title: "Docker Sandboxes backend"
description: Run agents in Docker Sandboxes (sbx) microVMs
---

workmux can run agents in [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (the `sbx` CLI). Each worktree gets its own sbx sandbox for each agent it runs, a microVM with its own kernel, and sbx's host-side proxy enforces its network policy.

Docker Sandboxes owns the agent; workmux owns the worktree workflow. By default (`agent: kit`), each sandbox is created for sbx's own agent kit: the kit installs and configures the agent in the sandbox's own home, sbx holds the agent's login as an sbx secret, and the agent sees only sbx's placeholder credentials. workmux mounts the worktree, runs the agent command and keeps status tracking, host commands, the clipboard and the rest of its [shared features](/guide/sandbox/features/) working. The compatibility mode, `agent: shell`, mounts the agent's config directory from your workstation as the container and Lima backends do, for agents sbx has no kit for or to keep your workstation's agent settings (see [Agent setup](#agent-setup-kit-or-shell)).

## Setup

### 1. Install and set up sbx

Install Docker Sandboxes by following [Docker's instructions](https://docs.docker.com/ai/sandboxes/), then sign in and choose a global network policy:

```bash
sbx login
sbx policy init   # choose allow-all, balanced or deny-all
```

workmux never runs `sbx login` or `sbx policy init` for you. It checks both before creating a sandbox and stops with an error naming the command to run.

Under `agent: kit`, sign the agent in through sbx. For Claude, either sign in once inside a sandbox (`/login` when Claude says "Not logged in"): sbx keeps the subscription login as its global `anthropic` secret, so every later sandbox starts logged in; or give sbx an API key with `sbx secret set anthropic`. For Codex, sign in to ChatGPT with `sbx secret set openai --oauth`, or give it an API key with `sbx secret set openai`; for Gemini, `sbx secret set google`. sbx gives OpenCode's and Copilot's credentials only to kits a binding in `~/.config/sbx/credentials.yaml` authorizes, and `sbx create` doesn't ask for one, so write the binding first (see Docker's docs); when sbx withholds a credential, workmux shows sbx's note from creating the sandbox. A subscription login is global to sbx; for a different account per sandbox, use API keys scoped to the sandbox (see [Accounts per named agent](#accounts-per-named-agent)).

workmux also needs Docker (`docker` on `PATH`) to build the sandbox template.

### 2. Enable in config

```yaml
# ~/.config/workmux/config.yaml or .workmux.yaml
sandbox:
  enabled: true
  backend: docker_sbx
```

`agent: kit` is the default. To mount your agent config instead, set `agent: shell` in your global config (a project's `.workmux.yaml` can't choose it):

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  docker_sbx:
    agent: shell
```

## Prerequisites

workmux checks these before creating a sandbox and fails closed, never running the agent unsandboxed:

| Prerequisite                 | If missing                                                                                   |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| `sbx` installed              | Error with a link to the install instructions                                                |
| Signed in to Docker          | Error naming `sbx login`                                                                     |
| Global network policy chosen | Error naming `sbx policy init`                                                               |
| Supported host               | Error naming sbx's platforms: macOS 14+ on Apple silicon, Windows 11, Ubuntu 24.04+ with KVM |

workmux reads the sign-in and policy checks from `sbx diagnose`. Any other check it fails, such as disk space, is printed as a warning with sbx's hint, and the launch goes ahead: sbx reports its own error if it can't create or run the sandbox.

## Configuration

The generic `sandbox` settings keep their meaning:

| Option             | Default                                 | Description                                                                                                                    |
| ------------------ | --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `backend`          | `container`                             | Set to `docker_sbx` for Docker Sandboxes                                                                                       |
| `image`            | `ghcr.io/raine/workmux-sandbox:<agent>` | Agent image the sbx template is built from                                                                                     |
| `rpc_host`         | `host.docker.internal`                  | Hostname the guest uses to reach workmux's RPC server. **Global config only.**                                                 |
| `agent_config_dir` | agent's default (e.g. `~/.claude/`)     | Host directory mounted as the agent's config directory, under `agent: shell`. **Global config only.**                          |
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
| `docker_sbx.agent`                  | `kit`                         | `kit` or `shell` (see [Agent setup](#agent-setup-kit-or-shell)). **Global config only.**        |
| `docker_sbx.cpus`                   | all host CPUs (sbx's default) | Number of CPUs for the sandbox (`0` means all)                                                  |
| `docker_sbx.memory`                 | half the host's memory        | Memory for the sandbox in sbx's binary units (`512m`, `8g`); at least `512m`                    |
| `docker_sbx.provision`              | -                             | Shell script run once, as root, when the sandbox is created (see [Provisioning](#provisioning)) |
| `docker_sbx.skip_default_provision` | `false`                       | Skip workmux's built-in provisioning (see [Provisioning](#provisioning))                        |
| `docker_sbx.kits`                   | `[]`                          | sbx mixin kits added when the sandbox is created (see [Kits](#kits)). **Global config only.**   |

sbx has no per-sandbox disk setting, so there is no `disk`. Resources, `provision` and `kits` apply when the sandbox is created; recreate it with `workmux sandbox prune` after changing them. workmux warns at launch when a setting fixed at creation (`agent`, `agent_config_dir`, `extra_mounts`, `cpus`, `memory`, `kits`, `image`, the toolchain) differs from what the sandbox was created with, naming the settings. A change that would leave a file the agent shouldn't read unhidden is an error instead (see [`excluded_files`](#whats-mounted)), as is a change of `docker_sbx.agent` (see [Agent setup](#agent-setup-kit-or-shell)).

## Agent setup: `kit` or `shell`

`sandbox.docker_sbx.agent` decides who sets the agent up in its sandbox. It's read only from your global config: under `shell` the agent holds its real credential, so a repository can't choose it (workmux ignores it in `.workmux.yaml`, with a warning).

**`kit` (default).** workmux creates the sandbox for sbx's [agent kit](https://docs.docker.com/ai/sandboxes/) (`sbx create claude …`), with the same template, worktree, git metadata and other mounts, and no agent config directory. The kit installs the agent's settings in the sandbox's own home, completes Claude's onboarding, trusts the workspace and adds its own network rules for the agent's vendor (`sbx policy ls <sandbox>` lists them; see [Network restrictions](#network-restrictions) for `policy: deny`). sbx holds the agent's credential: Claude's `~/.claude/.credentials.json` in the sandbox holds only sbx's placeholder, and sbx's proxy signs Claude's requests. When it creates the sandbox, workmux adds its [status hooks](/guide/status-tracking/) to the config the kit wrote (Claude's `~/.claude/settings.json`, Codex's `~/.codex/hooks.json` with hooks turned on in `config.toml`, Gemini's `~/.gemini/settings.json`, Copilot's `~/.copilot/hooks/`, OpenCode's plugin in `~/.config/opencode/plugins/`), so the sidebar shows the agent's state as under `shell`. workmux's panes start Codex with `--dangerously-bypass-hook-trust` (see [Agent flags](/guide/sandbox/features/#agent-flags)), so it runs these hooks without asking you to review them; a Codex you start yourself in the sandbox, such as with `workmux sandbox shell -- codex`, asks once, and trusting them records that in the sandbox's `~/.codex/config.toml`. The claude kit's own hooks and status line call `sbx-claude-hook`, and the codex kit's `notify` calls `sbx-codex-hook`; both come with sbx's own agent images, not workmux's template, so workmux drops them when the template doesn't have them. Your workstation's agent settings, hooks, plugins and sessions aren't in the sandbox; configure sbx's own settings (secrets, MCP servers, skills) with sbx.

`kit` supports the agent types sbx has a kit for: Claude, Codex, Copilot, Gemini and OpenCode. workmux publishes no Copilot image, so Copilot needs `sandbox.image` set to an image that provides `copilot` (built `FROM ghcr.io/raine/workmux-sandbox:base`). Under `kit`, these are errors naming `agent: shell`, rather than a silent change of credential posture:

- an agent type sbx has no kit for (agy, pi, OMP, or a custom agent);
- `sandbox.agent_config_dir`, which `kit` doesn't mount;
- `workmux add --fork`, which forks a conversation from your workstation's agent config.

`--continue` (`workmux add`, `workmux open`, and resuming after a restart) continues the agent's conversation in the sandbox's own home, which lasts as long as the sandbox: closing a window or stopping the sandbox keeps it, while `workmux remove` and `workmux sandbox prune` remove it, so a new sandbox has no conversation to continue.

A [named agent](/guide/agents/) whose `env` sets a variable to a workstation path, such as `CLAUDE_CONFIG_DIR: ~/.claude-work`, gets a warning on every launch: the sandbox runs the kit's own setup and login, not that directory's. Give it its own account through sbx instead (see [Accounts per named agent](#accounts-per-named-agent)).

**`shell`.** workmux creates the sandbox with sbx's generic `shell` agent and mounts the agent's config directory from your workstation where the agent looks for it (see [What's mounted](#whats-mounted)), as on the container and Lima backends: the agent keeps your settings, hooks and login, which it holds itself. sbx's kits can't share that home: they write the agent's settings and credentials into it.

Under both, workmux runs the agent command itself with `sbx exec`, never `sbx run`. The mode is fixed when the sandbox is created and recorded with it. A sandbox created under the other mode fails to launch with an error naming how to recreate it (`workmux sandbox prune`, or `sbx rm -f <sandbox>`), so switching to `kit` never keeps running a sandbox with your real credential mounted; sandboxes from before workmux recorded the mode count as `shell`.

### Accounts per named agent

Under `agent: kit`, an agent's account is the sbx secret for its service: `anthropic` for Claude, `openai` for Codex, `google` for Gemini. A secret scoped to a sandbox wins over the global one there, and each [named agent](/guide/agents/) runs in a sandbox of its own (`wm-<project>-<hash>-<agent>`, see [Sandbox naming](#sandbox-naming)), so `cc-work` and `cc-personal` can each have their own API key:

```bash
sbx secret set anthropic --sandbox wm-myproject-a1b2c3d4-cc-work
```

What sbx allows decides how far this goes:

- **Only API keys can be scoped to a sandbox.** A subscription login (Claude's, or ChatGPT's with `sbx secret set openai --oauth`) is global to sbx and serves every sandbox, so two named agents can't use two subscription accounts under `kit`. A `/login` inside a Claude sandbox replaces sbx's global `anthropic` login, for every sandbox without a key of its own; sbx notes that a `codex login` inside a sandbox stays in that sandbox.
- **Claude's login is chosen when the sandbox is created.** The kit sets Claude up for the key scoped to the sandbox at that moment; a key added later isn't used, and Claude stays on the global login. sbx accepts a secret for a sandbox that doesn't exist yet, and removing a sandbox removes the secrets scoped to it. For a sandbox that already exists, remove it with `sbx rm -f <sandbox>`, set the key, and the next pane (or `workmux open`) creates it again with the key.
- **Codex uses a scoped key at once, unless sbx holds a ChatGPT login.** With a global ChatGPT login, the codex kit uses the login and ignores an OpenAI key scoped to the sandbox, so per-agent Codex accounts work only without a global ChatGPT login.
- **A scoped key lasts as long as the sandbox.** Removing the worktree deletes its sandboxes and their scoped secrets, so each new worktree's sandbox needs the key set again.

### Tools that call other agents

Tools such as [consult-llm](https://github.com/raine/consult-llm) (a second opinion from another model) or a cross-model review panel call other models from inside the agent's sandbox. workmux doesn't run the other agent in a sandbox of its own; these tools work with settings you already have, under either mode.

**API calls.** consult-llm's `api` backend calls the provider's API with a key it requires in a variable such as `OPENAI_API_KEY` or `GEMINI_API_KEY`. Give the key to sbx (`sbx secret set openai`, or with `--sandbox` for one sandbox) and give the tool the variable with sbx's placeholder as its value:

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  env:
    OPENAI_API_KEY: proxy-managed
```

The tool sends the placeholder, and sbx's proxy replaces it with the key in requests to the provider's domains; the agent never holds the key, and workmux doesn't warn about the variable (see [Tool credentials](#tool-credentials)). A subscription login doesn't serve API calls: with only a ChatGPT login in sbx, the request goes out with the placeholder and OpenAI refuses it. When sbx holds a ChatGPT login for Codex, scope the API key to the sandbox that calls the API rather than replacing the login globally; in a Codex sandbox, sbx uses the login instead (see [above](#accounts-per-named-agent)). Under `policy: deny`, add the API's domain (such as `api.openai.com`) to `allowed_domains`: the agent's kit allows only its own vendor's hosts (see [Network restrictions](#network-restrictions)).

**Other agents' CLIs.** consult-llm's CLI backends (`codex-cli`, `gemini-cli`, `claude-cli`, `opencode`, `pi`, and `profile`) run another agent's CLI, which has to be installed and signed in where it runs:

- workmux's image for an agent has only that agent's CLI. Add the others in a custom `sandbox.image` (`workmux sandbox init-dockerfile`, see [Template](#template)).
- Under `kit`, the kit signs in only the sandbox's own agent, so another agent's CLI has no login of its own there; one that takes an API key from a variable can use a `proxy-managed` value as above. Under `agent: shell`, mount its config directory with [`extra_mounts`](/guide/sandbox/features/#extra-mounts) (for Codex, `~/.codex` with `guest_path: /home/agent/.codex` and `writable: true`, so it can refresh its login), which gives the sandbox that agent's real credential, as the agent's own config mount does.
- Listing the tool in [`host_commands`](/guide/sandbox/features/#host-command-proxying) runs it on the workstation, outside sbx's network policy and placeholders. Host commands can't read the agents' config directories (`~/.claude`, `~/.codex`, `~/.gemini`, …) or API key variables from your shell, so a CLI backend isn't signed in there.

**Proxies on the workstation.** A profile that sends an agent's traffic to a proxy on your workstation, such as `ANTHROPIC_BASE_URL: http://localhost:18765` in a consult-llm `profile` or a [named agent](/guide/agents/#named-agents), doesn't reach it from a sandbox: `localhost` there is the sandbox. Point it at `http://host.docker.internal:18765` and allow the port with an sbx rule, since sbx names the workstation `localhost` in its policy:

```bash
sbx policy allow network --sandbox wm-myproject-a1b2c3d4-claude localhost:18765
```

Without `--sandbox`, the rule goes in your global sbx policy and serves every sandbox. workmux leaves rules you add alone; one scoped to a sandbox goes when the sandbox does.

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

A sandbox belongs to the worktree and an agent, not to a pane:

- Each agent pane runs in the sandbox of its own agent: the named agent or agent type it runs (see [Sandbox naming](#sandbox-naming)). With the default `<agent>` pane, that's the worktree's selected agent, so `workmux add -a codex` runs in a Codex sandbox even when your default agent is Claude. A layout with Claude and Codex panes in one worktree, such as the `design` layout in [Named layouts with agents](/guide/agents/#named-layouts-with-agents), gets two sandboxes, and workmux creates both before the window opens.
- With `target: all`, plain panes (shells, editors, dev servers) run in the sandbox of the worktree's selected agent, so they see the same environment as that agent.
- Closing a pane or window, or the agent exiting, ends only that pane's session. sbx keeps a session's processes running when its `sbx exec` goes away, so the supervisor hangs them up itself; other panes' sessions keep running.
- sbx stops an idle sandbox about 30 seconds after its last session ends. The next pane, `workmux open` or `workmux sandbox shell` starts it again in about a second; there's no separate start step.
- Removing the worktree deletes its sandboxes, with their policy rules and any sbx secrets scoped to them: `workmux remove`, `workmux merge`, and a `workmux add` that rolls back a worktree it couldn't finish. The sandboxes go only once the removal can no longer fail: a failing `pre_remove` hook leaves both the worktree and its sandboxes. A pane still setting a sandbox up finishes first.

### What's mounted

sbx mounts the worktree, the git metadata and the shims at their host paths, and, under `agent: shell`, the agent's config directories at their usual place under `/home/agent`:

| Mount                                          | Access       | Purpose                                                                |
| ---------------------------------------------- | ------------ | ---------------------------------------------------------------------- |
| Worktree                                       | read-write   | The worktree the agent works in                                        |
| Main repository's `.git`                       | read-only    | Git control policy and administrative namespace                        |
| Git objects, refs, logs                        | read-write   | Commits and branch updates                                             |
| Current worktree admin                         | read-write\* | Index, HEAD, merge, and rebase state                                   |
| Agent config directory (`shell`)               | read-write   | Agent settings and credentials, at its usual place under `/home/agent` |
| OpenCode's `~/.config/opencode/` (`shell`)     | read-only    | OpenCode's global config, at `/home/agent/.config/opencode`            |
| `~/.local/state/workmux/docker-sbx/<sandbox>/` | read-only    | Host-command shims                                                     |
| `excluded_files` entries                       | masked       | An empty file mounted over each, so the agent can't read it            |
| `extra_mounts` entries                         | read-only\*  | User-configured paths                                                  |

\* As on the [container backend](/guide/sandbox/container/#whats-mounted), git pointer files, per-worktree config, object information, submodules' hooks and config, and files the repository config includes or runs stay read-only within writable git mounts, so an agent can't plant hooks or change git config. For a filter, diff or merge driver or fsmonitor, that's every file in the worktree or git dir its command names, so `sh ./scripts/clean.sh` protects the script as well. sbx enforces read-only mounts on the host, even against root in the sandbox. Sandboxes require a linked worktree: workmux refuses to sandbox the main worktree, whose admin files are the repository's own. Hooks in the main repository must be regular files, not symbolic or hard links.

Under `agent: shell`, see [Credentials](/guide/sandbox/features/#credentials) for each agent's config directory. These mounts are made when the sandbox is created; each agent has its own sandbox, so another agent gets its own config directory. For Claude, workmux also writes the sandbox's own `~/.claude.json` with onboarding marked complete. Pi and OMP download Linux `fd` and `rg` binaries into their config directory; workmux binds a directory inside the sandbox over `~/.pi/agent/bin/` and `~/.omp/agent/tools/` each time a pane starts, so the workstation's binaries are never overwritten.

`extra_mounts` entries are mounted at their `guest_path`, or at their host path without one. sbx mounts a single file only read-only, so a writable entry that is a file is an error. A `guest_path` inside the worktree or another mount appears on the workstation too: sbx creates the mount point there (an empty directory or file) if it's missing.

`sandbox.container.excluded_files` works as on the [container backend](/guide/sandbox/container/), and is global-only there too: each listed file that exists in the worktree is hidden behind an empty, read-only file that workmux keeps in the sandbox's state directory. A symbolic link is masked at the file it points to in the worktree. Missing paths and directories are skipped with a warning in the log, as are links that point outside the worktree; the main worktree, which the container backend masks too, isn't mounted here, so such links don't resolve in the sandbox. Like other mounts, masks are made when the sandbox is created, where the container backend masks them on every launch. So a listed file the sandbox has no mask over, because it was listed or created after the sandbox, is an error at launch naming the file, until you recreate the sandbox with `workmux sandbox prune` (or `sbx rm -f <sandbox>`): the agent could read it. A masked file that's deleted or no longer listed only gets a warning; sbx puts an empty file back where a masked file was deleted when it next starts the sandbox.

Mounts are fixed when the sandbox is created. After changing `extra_mounts` or `excluded_files`, recreate the sandbox with `workmux sandbox prune`; sandboxes created before workmux protected the git metadata need the same.

Your `~/.gitconfig` isn't mounted; workmux passes your git identity instead (see [Git identity](/guide/sandbox/features/#git-identity)).

### Sandbox naming

Sandboxes are named `wm-<project>-<hash>-<agent>` (e.g. `wm-myproject-a1b2c3d4-claude`), where the hash is of the worktree path and the agent is the pane's agent: a [named agent](/guide/agents/) such as `cc-personal`, or an agent type by its own name such as `claude` or `codex`. So every worktree gets its own sandbox for each agent it runs. An agent name sbx wouldn't accept (anything but lowercase letters, digits and single `-`) or one longer than 24 characters is shortened and slugified, with a short hash of the name so that two agents never share a sandbox. List them with `sbx ls`.

Sandboxes named `wm-<project>-<hash>`, from before the agent was part of the name, aren't used any more: removing their worktree deletes them, and the next pane creates the agent's own sandbox.

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

Under `agent: kit`, sbx holds the agent's login (see [Setup](#1-install-and-set-up-sbx)) and the agent sees only placeholders. Under `agent: shell`, agents use the login in their mounted config directory, as on the container backend: Codex, Gemini, OpenCode, pi and OMP start logged in if you've logged in on the workstation. Claude stores its login in the macOS Keychain, which the sandbox can't reach. Sign in to Claude once inside a sandbox; the login is written to the mounted config directory and persists across sandboxes. See [Credentials](/guide/sandbox/features/#credentials).

### Tool credentials

Give tools such as `gh` and `git` their credentials with sbx's own secrets, not with `env` or `env_passthrough`:

```bash
# For every sandbox
sbx secret set github --command 'gh auth token'
# For one sandbox
sbx secret set github --sandbox wm-myproject-a1b2c3d4 --command 'gh auth token'
```

sbx keeps the secret on the workstation and its proxy adds it to requests for the service's domains, so the agent only ever holds a placeholder (such as `GH_TOKEN=gho_sbxproxy…`). `git` over HTTPS to GitHub needs no credential helper: the proxy authenticates it too. Services are `anthropic`, `copilot`, `cursor`, `devin`, `droid`, `github`, `google`, `groq`, `mistral`, `nebius`, `openai`, `openrouter` and `xai`; `sbx secret ls` lists the ones you've set. workmux never sets, reads or passes a secret: it reads only their names, with `sbx secret ls`.

A variable in `env` or `env_passthrough` that holds one of these credentials (`GH_TOKEN`, `GITHUB_TOKEN`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` and the like, as `sbx secret import` recognises them) replaces sbx's placeholder with the real value, which the agent can then read and send anywhere it can reach. When sbx holds a secret for that service, globally or for the sandbox, workmux warns at launch, naming the variable and `sbx secret set`; remove the variable from your config. When sbx holds no such secret, workmux says nothing. A variable set to sbx's own placeholder, `proxy-managed`, holds no credential and gets no warning: a tool that requires a key variable can be given one with `env` (for example `OPENAI_API_KEY: proxy-managed`), and sbx's proxy supplies the real key in its requests.

sbx also sets git config of its own in the sandbox (keeping its `.sbx` directories out of `git status`); workmux keeps it alongside your [git identity](/guide/sandbox/features/#git-identity).

## Provisioning

The `provision` script runs once, after workmux creates the sandbox, sets it up (network rules; under `agent: kit`, workmux's status hooks; under `agent: shell`, config directory parents and Claude's onboarding state) and runs its [built-in provisioning](#built-in-provisioning), never on later launches. It runs as root with `bash -c`, in the worktree; the template has no `sudo`, so don't use it. Its downloads go through sbx's proxy and network policy like any other request in the sandbox; sbx's `balanced` policy reaches package mirrors over HTTPS only, which is why the [template](#template) switches the image's Debian sources to HTTPS. A changed script applies the next time the sandbox is created (`workmux sandbox prune`, or `workmux remove`). If the script fails, workmux deletes the sandbox and stops with its error, so the next launch creates and provisions it again. The same holds for every step after `sbx create`: workmux records a sandbox's settings in its state directory only once it's fully set up, and a launch that finds a sandbox a launch began creating without finishing (say, after workmux was killed while provisioning) deletes it and creates it again. A sandbox without the record that no launch began creating predates the record: when it's stopped, workmux deletes it and creates it again; while it's running, panes may be in it, so workmux warns and launches into it as it is, or fails if it would have to hide a file (`excluded_files`) the sandbox has no mask over. Close the worktree's panes, and the first launch once sbx has stopped it recreates it.

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

sbx's `balanced` global policy blocks most of them. Under `policy: allow`, whenever the worktree uses a toolchain workmux adds a rule scoped to the sandbox for each, and reconciles them like the [network rules](#network-restrictions): turning the toolchain off removes them at the next launch. Under `policy: deny`, workmux adds none beyond `allowed_domains`: a repository can add a `devbox.json` or `flake.nix`, and that mustn't open more than your global config allows. Each launch prints a note naming the toolchain hosts `allowed_domains` doesn't list; add the ones you want to `allowed_domains` in your global config, or installing Nix and Devbox fails. A flake's inputs on other hosts aren't in the list: under `deny`, add them to `allowed_domains`; under `allow`, your global sbx policy decides.

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

A kit can add network rules, credentials and install steps to the sandbox. sbx adds a kit's network rules as rules scoped to the sandbox, so they widen its access beyond `network.allowed_domains`, and workmux leaves them alone (`sbx policy ls <sandbox>` lists them; under `policy: deny` workmux warns about them, see [Network restrictions](#network-restrictions)). `kits` is therefore a global-only setting: workmux ignores it in a project's `.workmux.yaml`, with a warning. Use only mixin kits: workmux picks the sandbox's agent kit, or sbx's `shell` agent, itself (see [Agent setup](#agent-setup-kit-or-shell)). sbx keeps composed kits in its template store (`sbx template ls`, as `sandbox-kit-assembled`).

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

Under `policy: deny`, workmux adds a rule scoped to the sandbox for port 443 of each allowed domain (`sbx policy ls <sandbox>` lists them), and nothing else: the [toolchain's hosts](#nix-and-devbox-toolchain) are reachable only if `allowed_domains` lists them. A wildcard such as `*.github.com` becomes sbx's `**.github.com:443` (subdomains at any depth) with a deny for `github.com:443`, so the base domain stays out unless you list it too. IP literals are refused, as on the container backend. Under `policy: allow`, workmux adds none but the toolchain's.

Every launch reconciles the rules: it adds missing ones and removes the ones workmux added earlier that your config no longer lists, so a removed domain stops working at the sandbox's next launch. workmux records the rules it added in the sandbox's state directory and never touches rules you added yourself, or your global sbx policy.

Differences from the container backend:

- **The global sbx policy still applies.** sbx allows a request when your global policy or a sandbox rule allows it, and a deny in either wins. Under `policy: allow`, what your global policy blocks stays blocked. Under `policy: deny`, the sandbox can also reach whatever your global policy allows (sbx's `balanced` policy allows package registries, code hosts and AI services), and a sandbox rule can't take that back; workmux warns on every launch, saying how many destinations that is. For a sandbox that reaches only `allowed_domains`, as on the container backend, choose `deny-all` (`sbx policy reset`, then `sbx policy init deny-all`).
- **Kit rules.** A kit adds network rules scoped to the sandbox when sbx creates it: under `agent: kit`, the agent kit's hosts for its vendor (the claude kit allows `api.anthropic.com`, `platform.claude.com`, `downloads.claude.ai`, `claude.com`, `mcp-proxy.anthropic.com` and `bridge.claudeusercontent.com` on port 443, and `deb.debian.org` and `dhi.io` on port 80), and any [mixin kit's](#kits). They're sbx's rules (`provisioned` in `sbx policy ls <sandbox> --json`), not yours or workmux's, and the kit needs them, so workmux never removes them. Under `policy: deny`, every launch warns, naming each kit host and port that `allowed_domains` doesn't cover, from the launch that creates the sandbox on. Under `deny`, list your agent's API domains in `allowed_domains` (for Claude, `api.anthropic.com`, `platform.claude.com`, `claude.com` and `downloads.claude.ai`), so the warning names only what the kit adds beyond them; `allowed_domains` covers port 443 only, so a kit's port-80 hosts stay in it. A domain workmux denies itself (a wildcard's base) stays blocked: sbx's deny wins over the kit's allow.
- **A project can't choose `docker_sbx` for a wider policy.** When `backend: docker_sbx` comes from a repository's `.workmux.yaml`, your global config sets `policy: deny` and your global sbx policy allows more than `deny-all`, workmux refuses to launch: a repository can't move a `deny` sandbox onto a wider policy. Set `backend: docker_sbx` in your global config to accept your global sbx policy (you then get the warning only), or choose `deny-all`.
- **Private addresses.** sbx doesn't refuse private or loopback addresses an allowed domain resolves to, so `allow_private_ips: false` can't be enforced under `docker_sbx`; workmux warns on every launch under `policy: deny`, unless every allowed domain sets `allow_private_ips: true` and the global policy allows nothing. A refusal would leave `deny` unusable, so this is a warning, not an error.
- **`max_connections`** is ignored: sbx has no cap on concurrent connections. workmux prints a note when it's set.

sbx logs requests it blocks: `sbx policy log <sandbox>` lists them with the rule or reason. They don't appear in workmux's log.

## Sandbox management

`workmux sandbox stop`, `prune` and `shell` act on sbx sandboxes when the backend is `docker_sbx`, as they do on Lima VMs:

```bash
# Stop running workmux sandboxes (interactive list, a name, or --all; -y skips the prompt)
workmux sandbox stop
workmux sandbox stop wm-myproject-abc12345-claude
workmux sandbox stop --all --yes

# Delete all workmux sandboxes (wm-* in `sbx ls`), after confirmation
workmux sandbox prune
workmux sandbox prune --force

# Open a shell in the current worktree's sandbox for the selected agent (creating it if needed)
workmux sandbox shell
workmux sandbox shell -- git status

# Open a shell in another agent's sandbox of the worktree
workmux sandbox shell --agent codex
```

A stopped sandbox keeps its state and starts again when a pane needs it. A pruned sandbox is created again by the next `workmux open`. `workmux sandbox shell` has no RPC server, so host commands and status updates don't work in it; `--exec` means the same as without it, since the sandbox is already persistent.
