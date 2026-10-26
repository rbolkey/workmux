---
title: "sandbox"
description: Manage sandbox backends and tooling
---

Commands for managing sandbox functionality across container (Docker/Podman/Apple Container), Lima and OpenShell backends. `stop`, `prune`, `status` and `shell` act on the backend the current repository's config picks with `sandbox.backend`.

## Container commands

### sandbox build

Build the sandbox container image locally (two-stage: base + agent).

```bash
workmux sandbox build
```

Builds the image locally for the configured agent. This is an alternative to using the pre-built image from `ghcr.io/raine/workmux-sandbox`. Most users should use `workmux sandbox pull` instead.

### sandbox pull

Pull the latest sandbox image from the container registry.

```bash
workmux sandbox pull
```

Pulls the pre-built image for the configured agent from `ghcr.io/raine/workmux-sandbox:{agent}`. This is the recommended way to get and update the sandbox image.

### sandbox init-dockerfile

Export a customizable Dockerfile for building your own sandbox image.

```bash
workmux sandbox init-dockerfile [--force]
```

**Options:**

- `--force` - Overwrite existing Dockerfile

Creates a `Dockerfile.sandbox` in the current directory with the base system setup (Debian, git, workmux) and agent-specific installation (e.g., Claude Code) combined into a single file.

## Lima commands

### sandbox stop

Stop Lima VMs to free resources.

```bash
# Interactive mode - show list and select VM
workmux sandbox stop

# Stop specific VM
workmux sandbox stop <vm-name>

# Stop all workmux VMs
workmux sandbox stop --all

# Skip confirmation prompt
workmux sandbox stop --all --yes
```

**Arguments:**

- `<vm-name>` - Name of the VM to stop (optional, conflicts with `--all`)

**Options:**

- `--all` - Stop all workmux VMs (those starting with `wm-` prefix)
- `-y, --yes` - Skip confirmation prompt

This command helps you stop running Lima VMs created by workmux to free up system resources. When run without arguments, it shows an interactive list of running workmux VMs for you to choose from. The command will ask for confirmation before stopping any VMs unless `--yes` is provided.

**Notes:**

- This command only works with Lima backend and requires `limactl` to be installed
- Only running VMs are shown in interactive mode
- If a specified VM is already stopped, the command reports this and exits successfully
- Non-interactive environments (pipes, scripts) require `--all` or a specific VM name

### sandbox prune

Delete unused Lima VMs to reclaim disk space.

```bash
# Interactive - show VMs and confirm deletion
workmux sandbox prune

# Skip confirmation and delete all workmux VMs
workmux sandbox prune --force
```

**Options:**

- `-f, --force` - Skip confirmation and delete all workmux VMs

Lists all workmux Lima VMs (those starting with `wm-` prefix) with their size, age, and last accessed time, then prompts for confirmation before deleting them. Requires `limactl` to be installed.

## OpenShell commands

An [OpenShell sandbox](/guide/remote-projects/openshell/) holds its project's clone rather than mounting your worktrees, so deleting one loses the work that exists only there. Run these in a repository whose config sets `sandbox.backend: openshell` (or in a window workmux opened for it). Sandboxes are named like Lima VMs, `wm-<project>-<hash8>`, from the clone's path; `stop` and `prune` list and touch only workmux's `wm-` sandboxes. There is no `start`: the next command that needs the sandbox, such as `workmux add` or `workmux open`, creates or starts it.

### sandbox stop

Stop OpenShell sandboxes to free resources.

```bash
# Interactive mode - pick from the running sandboxes; Enter picks this repository's
workmux sandbox stop

# Stop a specific sandbox
workmux sandbox stop <sandbox-name>

# Stop all running workmux sandboxes, without asking
workmux sandbox stop --all --yes
```

The options are Lima's. Stopping keeps the sandbox's workspace, but every process in it ends, agents included: their entries turn stale, and after the next start their workstation panes find their remote panes ended and close. Reopen a worktree with `workmux open`.

### sandbox prune

Delete workmux's OpenShell sandboxes after one confirmation, and close the workstation windows of their projects.

```bash
# List the sandboxes, check them for work, and confirm deletion
workmux sandbox prune

# Delete them all without checking or asking
workmux sandbox prune --force
```

**Options:**

- `-f, --force` - Skip the check and the confirmation, and delete every workmux sandbox even when it holds work

Before asking, `prune` checks each sandbox's worktrees and branches, starting a stopped one to look (and stopping it again if nothing is deleted). It refuses, naming each sandbox, while any has uncommitted changes or commits not on any remote, or could not be checked:

```text
1. wm-api-1a2b3c4d (ready, project api)
   Work that exists only in the sandbox:
     /sandbox/api__worktrees/feature (branch feature): uncommitted changes, 2 commits not on any remote, not merged into main

Error: Not deleting: wm-api-1a2b3c4d may hold uncommitted or unpushed work. Commit and push it, or delete anyway with --force
```

### sandbox rebuild

Delete and recreate the current repository's sandbox from the current image and config, carrying its committed work across. Refused while any worktree has uncommitted changes. On a sandbox that doesn't exist yet, it only creates it. See [Rebuilding a sandbox](/guide/remote-projects/openshell/#rebuilding-a-sandbox).

```bash
workmux sandbox rebuild [-y]
```

**Options:**

- `-y, --yes`: Don't ask for confirmation.

## General commands

### sandbox status

Show the current repository's sandbox. It never starts or creates one.

```bash
workmux sandbox status
```

- **OpenShell:** whether the sandbox is reachable, stopped or not created yet, with remote workmux's and the protocol's versions, warnings (a missing session holder, agents without hooks, a devshell built from older flake files), the transport's error when unreachable, the repository URL, the sandbox's effective policy, and its agents with the providers that give them credentials. Exits 1 when the sandbox is unreachable.
- **Lima:** the VM's name and state (running, stopped, or not created yet), its isolation, the toolchain it uses, and, once the VM exists, the mounts workmux gives it.
- **Container:** the image the config resolves to and whether it is present locally. Containers are per worktree and short-lived, so there is no long-lived sandbox to report.
- **No sandbox:** says the repository runs on the workstation.

### sandbox agent

Run the configured agent inside a sandbox with full RPC support. Unlike `shell`, this starts an RPC server so the agent can call workmux commands (e.g., `workmux add` to spawn sub-agents).

```bash
# Run the configured agent (from config or defaults to claude)
workmux sandbox agent

# Run a specific command instead
workmux sandbox agent -- claude -p "coordinate these tasks"
```

**Options:**

- `<command...>` - Command to run instead of the configured agent

This command runs a sandboxed agent in the current directory. It delegates to the same supervisor process used by `workmux sandbox run`, which handles RPC server setup, sandbox dispatch (Lima or container), environment variables, and cleanup.

The key difference from `sandbox shell` is that this starts an RPC server, enabling the guest to call `workmux add` from inside the sandbox. Guest-side `workmux add` detects the sandbox environment and routes through SpawnAgent RPC to the host, where sub-agents are created normally (and sandboxed if the project config says so).

**Requirements:**

- Must be run from inside a git repository (sandbox needs git directories for mounts)
- Sandbox must be configured (image pulled or built)

**Use case:** Running a coordinator agent inside a sandbox so it can spawn sub-agents via `workmux add` while still being isolated from the host.

### sandbox shell

Start an interactive shell in a sandbox. Uses the same mounts and environment as a normal worktree sandbox. Works with the container, Lima and OpenShell backends.

```bash
# Start a new shell (container backend starts a new container, Lima connects to existing VM)
workmux sandbox shell

# Run a specific command instead of bash
workmux sandbox shell -- <command...>

# Exec into an existing container (container backend only)
workmux sandbox shell --exec
```

**Options:**

- `-e, --exec` - Exec into an existing container for this worktree instead of starting a new one (container backend only)
- `<command...>` - Command to run instead of bash

**Backend behavior:**

- **Container:** Starts a fresh container with the same mounts and environment as a normal worktree sandbox. With `--exec`, attaches to an existing container instead.
- **Lima:** Connects to the Lima VM for the current worktree (creating it if needed). The `--exec` flag is not supported since Lima VMs are persistent and `shell` always connects to the existing VM.
- **OpenShell:** Opens the sandbox user's login shell in the sandbox, in the worktree of the project window you run it from, else in the sandbox's clone. It creates or starts the sandbox (and clones the project on first use) if need be. `workmux` typed in that shell is remote workmux, which can't reach your workstation; use a worktree's own panes for that. `--exec` is not supported.

### sandbox install-dev

Cross-compile and install workmux into container images and running Lima VMs for local development.

```bash
# Cross-compile and install into containers and running VMs
workmux sandbox install-dev

# Use release profile (slower build, faster binary)
workmux sandbox install-dev --release

# Skip compilation, copy existing binary
workmux sandbox install-dev --skip-build
```

**Options:**

- `--skip-build` - Skip cross-compilation and copy the previously built binary
- `--release` - Use release profile (default is debug for faster iteration)

This is a developer-only command for getting local workmux builds into sandbox environments. The host macOS binary cannot run inside Linux containers or VMs, so this command cross-compiles for the correct Linux architecture.

For container sandboxes, it builds a thin overlay image (`FROM <image>` + `COPY workmux`) on top of the configured sandbox image, replacing it in-place. For Lima VMs, it copies the binary into each running VM.

**Prerequisites:**

- Rust cross-compilation target: `rustup target add aarch64-unknown-linux-gnu`
- Cross-linker: `brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu`

The binary is installed to `~/.local/bin/workmux` inside the VM, which is already on PATH.

### sandbox run

Run a command inside a sandbox (internal, used by pane setup).

```bash
workmux sandbox run <worktree> -- <command...>
```

This is an internal command generated by `wrap_for_lima()` during pane setup. It runs the host-side supervisor process that:

1. Ensures the Lima VM is running
2. Starts a TCP RPC server on a random port
3. Runs the command inside the VM via `limactl shell`
4. Passes `WM_SANDBOX_GUEST=1`, `WM_RPC_HOST`, `WM_RPC_PORT`, and `WM_RPC_TOKEN` env vars to the guest
5. Exits with the agent command's exit code

The RPC server handles requests from the guest workmux binary:

- `SetStatus`: updates the tmux pane status icon
- `SetTitle`: renames the tmux window
- `Heartbeat`: health check
- `SpawnAgent`: runs `workmux add` on the host to create a new worktree

**Guest-side `workmux add`:** When `workmux add` runs inside a sandbox, it automatically detects the sandbox environment and routes through SpawnAgent RPC instead of trying to create worktrees locally (which would fail due to missing tmux). This enables coordinator agents running in sandboxes to spawn sub-agents. Only a subset of `add` flags are supported over RPC; unsupported flags (`--base`, `--pr`, `--with-changes`, `--count`, `--foreach`, `--name`, `--agent`, `--wait`) are explicitly rejected with clear error messages.

## Quick Setup

```bash
# 1. Enable in config (~/.config/workmux/config.yaml or .workmux.yaml)
#    sandbox:
#      enabled: true

# The pre-built image is pulled automatically on first run.
# To pull it explicitly:
workmux sandbox pull
```

## Example

```bash
# Pull the latest sandbox image
workmux sandbox pull
# Output:
# Pulling image 'ghcr.io/raine/workmux-sandbox:claude'...
# Image 'ghcr.io/raine/workmux-sandbox:claude' is up to date.
```

## See also

- [Sandbox guide](/guide/sandbox/) for full setup instructions
