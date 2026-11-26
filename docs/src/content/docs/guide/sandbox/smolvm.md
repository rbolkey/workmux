---
title: "smolvm backend"
description: Run each sandboxed pane in its own smolvm microVM, with credentials kept outside the VM
---

workmux can use [smolvm](https://github.com/smol-machines/smolvm) for sandboxing. Each sandboxed pane runs in its own **smol machine**, a microVM with its own kernel. workmux creates the machine when the pane starts and deletes it when the pane exits, as the container backend does with containers. A machine is ready in about 0.3 seconds because it starts from a per-project **pack** that already holds the image, `provision` and the Nix or Devbox toolchain.

smolvm also keeps secrets out of the VM. A token workmux passes in reaches the agent as a placeholder, and smolvm swaps in the real value on the host, only on requests to that token's hosts. See [Credentials](#credentials).

## Setup

### 1. Install smolvm

Install smolvm 1.36.1 or later by following [smolvm's instructions](https://github.com/smol-machines/smolvm). smolvm runs on macOS on Apple silicon, or on Linux with KVM (`/dev/kvm`). workmux checks both when the backend is selected, and stops with an error naming the version it found and the minimum. workmux never installs smolvm.

### 2. Enable in config

```yaml
# ~/.config/workmux/config.yaml or .workmux.yaml
sandbox:
  enabled: true
  backend: smolvm
```

The first `workmux add` builds the project's pack before the window opens, with progress on the terminal. Later panes reuse it.

### 3. Accept the Git metadata gap

smolvm can't protect every file of a worktree's Git metadata (see [Git metadata boundary](#git-metadata-boundary)). On first use in a project, workmux explains the gap and asks for acceptance, once per project. For unattended startup, accept it in your global config:

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  smolvm:
    accept_reduced_git_metadata_isolation: true
```

## Configuration

smolvm settings are nested under `sandbox.smolvm`, with Lima's names where the idea is the same:

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  enabled: true
  backend: smolvm
  env_passthrough:
    - GH_TOKEN
  smolvm:
    cpus: 4
    memory: 8GiB
    provision: |
      apt-get update
      apt-get install -y --no-install-recommends ripgrep jq
```

| Option                                         | Default                                       | Description                                                                                                                     |
| ---------------------------------------------- | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `backend`                                      | `container`                                   | Set to `smolvm` for smol machines                                                                                               |
| `smolvm.cpus`                                  | smolvm's (4)                                  | vCPUs for each machine                                                                                                          |
| `smolvm.memory`                                | smolvm's (half the host's RAM, at most 8 GiB) | Memory for each machine, in Lima's format (`8GiB`, `512MiB`)                                                                    |
| `smolvm.provision`                             | -                                             | Shell script run as root once per pack build, without credentials (see [Packs](#packs)). A blank script turns a global one off. |
| `smolvm.credentials`                           | `{}`                                          | Variables to pass as placeholders, and where their values come from (see [Credentials](#credentials)). **Global config only.**  |
| `smolvm.accept_reduced_git_metadata_isolation` | `false`                                       | Accept the [Git metadata gap](#git-metadata-boundary) without a prompt. **Global config only.**                                 |
| `image`                                        | `ghcr.io/raine/workmux-sandbox:{agent}`       | The image packs are built from                                                                                                  |
| `toolchain`                                    | `auto`                                        | `auto`, `off`, `devbox` or `flake` (see [Toolchains](#nix-and-devbox-toolchains))                                               |
| `network`                                      | `policy: allow`                               | Egress policy, enforced by smolvm (see [Network](#network)). **Global config only.**                                            |
| `rpc_host`                                     | `host.smolvm.internal`                        | Hostname the guest uses for workmux's RPC. **Global config only.**                                                              |

`cpus`, `memory` and `provision` can be set in a project's `.workmux.yaml`, so a repository can commit the sandbox it needs. The global-only keys are ignored with a warning when a project sets them. The other `sandbox` keys (`target`, `env`, `env_passthrough`, `host_commands`, `extra_mounts`, `agent_config_dir`) mean the same as on the other backends.

`sandbox.container.excluded_files` is an error when a pane starts, since smolvm can't mask a single file and the files would stay readable. The container runtime settings `devices`, `group_add`, `cap_add`, `security_opt` and `oci_runtime` don't apply and are ignored, as on Lima.

## How it works

Each sandboxed pane runs a supervisor (`workmux sandbox run`) that:

1. Checks that smolvm is installed and recent enough.
2. Builds the project's pack, if it's missing or stale.
3. Deletes machines left behind by supervisors that were killed.
4. Starts workmux's RPC server on a random port.
5. Creates the pane's machine from the pack, starts it, and runs the agent with `smolvm machine exec` as your user id, with `HOME=/tmp`.
6. Deletes the machine when the agent exits, whatever its exit status.

The guest reaches the RPC server at `host.smolvm.internal`, smolvm's gateway to the host. Status tracking, [host commands](/guide/sandbox/features/#host-command-proxying), sound, the [clipboard proxy](/guide/sandbox/features/#clipboard-proxy) and [coordinator agents](/guide/sandbox/features/#coordinator-agents) work as on the other backends. The gateway gives the guest every service listening on the host's loopback, as the other backends' gateways do.

Panes don't share machines, so they can't see each other's processes or guest files. A pane runs with its own agent's config and credentials, so `workmux add -a codex` gets Codex's even when your default agent is Claude. Under `target: all`, plain panes get machines of their own for the selected agent (`-a`, else the configured one), with its variables.

### Machine names

Machines are named `wm-<project>-<hash>-<agent>-<random>` and labelled with the project, worktree, agent and owning supervisor, so `smolvm machine ls` shows which is which. Removing a worktree (`workmux remove`, `merge`, or a failed `add`) deletes any of its machines still running.

### What's mounted

| Mount                                                    | Access               | When it's mounted |
| -------------------------------------------------------- | -------------------- | ----------------- |
| Worktree directory                                       | read-write           | at boot           |
| Main repository's `.git`                                 | read-only            | at boot           |
| `objects`, `refs`, `logs`, `rr-cache`                    | read-write           | at boot           |
| The worktree's admin directory (`.git/worktrees/<name>`) | read-write           | at boot           |
| Submodules' `objects`, `refs`, `logs`, `rr-cache`        | read-write           | at boot           |
| Agent config directory, at the container backend's paths | read-write           | at boot           |
| Shims for host commands                                  | read-only            | after start       |
| `extra_mounts` entries (directories only)                | read-only by default | after start       |

Claude's `~/.claude.json` comes from `~/.claude-sandbox-config/`, as on Apple Container. The main worktree isn't mounted, so a symlink into it doesn't resolve in the guest.

### Managing machines

```bash
# Delete running machines (interactive, by name, or all)
workmux sandbox stop
workmux sandbox stop --all --yes

# Delete orphaned machines and packs no project uses
workmux sandbox prune

# Start a machine with the worktree's mounts and open a shell
workmux sandbox shell

# Open a shell in a running pane's machine
workmux sandbox shell --exec
```

Machines don't outlive their panes, so there's nothing to restart. `prune` lists what it would delete and asks first; `--force` skips the question.

## Packs

A pack is built once per project and image, and holds:

- the image (`sandbox.image`);
- the result of `provision`, run as root;
- Nix or Devbox, with the default branch's shell realised in the store, when the toolchain needs it.

A pack is rebuilt when any of its inputs changes: the image name, `provision`, the toolchain mode, the default branch's `flake.nix`/`flake.lock` or `devbox.json`/`devbox.lock`, the workmux version, or the smolvm version. Running panes keep their machines. The old pack is deleted once no machine created from it remains.

Packs live in `~/.local/state/workmux/smolvm-packs/`. smolvm also keeps a storage seed per pack in its own cache, which it evicts on its own.

### What `provision` can't see

`provision` runs in a machine with open network access and none of your credentials: no `env_passthrough`, no named agents' `env` and no `credentials`. A step that needs a private token, such as installing from a private registry, fails. Put such tools in a [custom image](/guide/sandbox/container/#custom-images) built where the token is available, or install them from the pane.

### Images

The image name is part of the pack's key, the digest isn't. After a new image is published under the same tag, mark the packs stale:

```bash
# Rebuild packs from the registry's image
workmux sandbox pull

# Build the image locally, save it for smolvm, and rebuild packs from it
workmux sandbox build
```

Both mark the image's packs stale in every project and rebuild the current project's pack at once. Other projects rebuild on their next pane. `pull` doesn't download the image itself: smolvm pulls it during the pack build. `build` saves the local image with `docker save` (or the configured runtime's equivalent) and builds packs from that archive until the next `pull`.

### Nix and Devbox toolchains

With a toolchain, each pane runs the agent inside `nix develop` or `devbox run` against its own worktree's files, so a branch with a different flake gets its own shell. The pack is a warm cache for the default branch's shell. It keeps Nix's and Devbox's caches, with the flake inputs Nix fetched, in `/var/cache/workmux-toolchain`, which is the pane's `XDG_CACHE_HOME`, since boot empties `HOME` (`/tmp`). A pack without a toolchain has no such directory, so a pane whose branch adds one caches under `HOME`. Devbox goes into the pack as its binary rather than its launcher, which would download the binary into each pane's empty `HOME`.

The pack's shell is warmed from the default branch's tracked files, so a flake that imports other files from the repository (`import ./nix/foo.nix`) warms too. Submodule contents aren't included, and Git LFS files stay as pointers. Changing only an imported file doesn't rebuild the pack; the pack key covers just the manifest and lock files.

Warming is best effort. If it fails, the pack is built anyway with a warning that carries the tool's error, and each pane builds its own shell. The pack stays cold until its key changes, or until `workmux sandbox build` or `workmux sandbox pull` marks it stale.

Under `toolchain: auto`, Nix or Devbox goes into the pack only when the **default branch** has `flake.nix` or `devbox.json` (Devbox wins when both exist). A worktree that adds a flake on its own branch gets the `nix develop` wrapper but no Nix in its machine, and fails with `nix: not found`. Set the mode explicitly in that case:

```yaml
sandbox:
  toolchain: flake # or: devbox
```

## Credentials

The agent's config directory is mounted as on every backend (see [Credentials](/guide/sandbox/features/#credentials)), so settings, sessions and `--resume` work. Variables are different: when workmux knows the hosts a variable is for, the agent sees a placeholder in that variable, and smolvm swaps in the real value on the host, only on requests to those hosts. The real value never enters the VM.

### Automatic placeholders

These variables become placeholders with no configuration, whether they come from `env_passthrough` or a `credentials` entry's source:

| Variable                   | Hosts                                  |
| -------------------------- | -------------------------------------- |
| `ANTHROPIC_API_KEY`        | `api.anthropic.com`                    |
| `CLAUDE_CODE_OAUTH_TOKEN`  | `api.anthropic.com`                    |
| `OPENAI_API_KEY`           | `api.openai.com`                       |
| `GEMINI_API_KEY`           | `generativelanguage.googleapis.com`    |
| `GH_TOKEN`, `GITHUB_TOKEN` | `api.github.com`, `uploads.github.com` |

`GH_TOKEN` and `GITHUB_TOKEN` also make git over HTTPS to `github.com` work with no git config. workmux derives git's `Authorization` header from the token and hands it to smolvm, and the guest sees only a placeholder in `WM_GITHUB_GIT_AUTH`. This needs the token from `env_passthrough`, not from the keychain. When both are set, `GH_TOKEN` is used.

A variable workmux has no hosts for passes its real value, as on other backends.

smolvm substitutes placeholders in HTTPS requests on port 443 over HTTP/1.1. WebSocket traffic isn't substituted. Substitution goes by the placeholder's value, so a script that copies a placeholder into another variable still gets it swapped on the allowed hosts.

A [named agent's](/guide/agents/#named-agents) `from_env: NAME` works as on the container and Lima backends: the pane's command carries `"$NAME"` unexpanded, and the shell in the VM expands it. So `NAME` must reach the VM through `env_passthrough` or `env`, and a variable with known hosts arrives as its placeholder. The host pane's command line never holds the value.

Each pane starts a new machine, so a value that changes on the host reaches the next pane.

### Listing other variables

`sandbox.smolvm.credentials` maps a variable, by the name the agent sees, to how it's passed:

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  env_passthrough:
    - LINEAR_API_KEY
  smolvm:
    credentials:
      LINEAR_API_KEY:
        hosts: [api.linear.app]
      GH_TOKEN:
        hosts: [api.github.com, uploads.github.com]
        from_keychain: github-token/workmux
      GITHUB_GIT_AUTH:
        hosts: [github.com]
        mode: { set_header: Authorization }
        from_keychain: github-git-auth/workmux
```

Here `gh` and the GitHub API get `GH_TOKEN` from the keychain, and git over HTTPS to `github.com` gets its `Authorization` header from a second keychain item, so the token never enters your shell. That item holds the whole header value, `Basic ` followed by the base64 of `x-access-token:<token>`. `GH_TOKEN` lists its hosts, so workmux derives no git header from it to clash with this one.

Each entry takes:

- `hosts`: the hostnames smolvm sends the real value to: DNS names, or `*.example.com` for subdomains only (no IP addresses or ports). Listing hosts for a built-in variable replaces its built-in hosts. A built-in variable with listed hosts gets no git header.
- `mode`: `header` (the default) swaps the placeholder wherever it appears in a request header; `{ set_header: NAME }` sets the named header to the whole value on every request to the hosts.
- `from_keychain: SERVICE[/ACCOUNT]`: where smolvm reads the value, instead of your shell's environment. With a source, the variable doesn't need to be in `env_passthrough`.

An entry for a variable nothing passes into the sandbox (not in `env_passthrough` and without a source) isn't passed, and workmux logs a note. An entry without `hosts` for a variable that has no built-in hosts is an error when a pane starts, since its real value would otherwise reach the guest.

### Keychain sources

smolvm reads a `from_keychain` source itself, so workmux never sees the value and never writes it to its state, config or logs.

- `gh` keeps its own keychain item (`gh:github.com`) encoded as a `go-keyring-base64:` string, and that string is what a `from_keychain` source would send. Pass `GH_TOKEN` from the environment instead: `export GH_TOKEN=$(gh auth token)`.
- The first request that uses a keychain value can take several seconds, and the first time smolvm reads an item macOS asks whether to allow it. Until you answer that dialog, requests that need the value wait: Claude Code shows "No response from the API" and retries. Choose **Allow**.

### macOS: a Claude Code token in the Keychain

Claude's subscription login on macOS stays in the Keychain, out of the mounted `~/.claude`. To log a sandboxed Claude in with a token instead, create one with `claude setup-token` and store it in the Keychain:

```bash
claude setup-token
security add-generic-password -s claude-code -a work -w
# paste the token at the prompt
```

Then list it as a credential:

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  smolvm:
    credentials:
      CLAUDE_CODE_OAUTH_TOKEN:
        from_keychain: claude-code/work
```

Sandboxed Claude panes start logged in, and the agent sees only a placeholder in `CLAUDE_CODE_OAUTH_TOKEN`. Claude prefers the token to a stored login.

### Login files

Some agents keep their login in their config directory: Claude's `.credentials.json`, `auth.json` for Codex, OpenCode and Pi, and Gemini's `oauth_creds.json`. That directory is mounted, so the agent in the VM can read the login. To keep the login out, use a token variable and a config directory without the file (see [`agent_config_dir`](/guide/sandbox/features/#custom-config-directory)).

### What each mechanism protects

- **Placeholders:** the secret never enters the VM, but the agent can still use it against the allowed hosts while the pane runs.
- **Mounted config directories:** any login file in them is readable and writable by the agent, as on the other backends.

### Under `policy: deny`

A built-in variable's hosts are narrowed to those in `allowed_domains`. If none is allowed, the variable isn't passed, with a note. Git's header needs `github.com` in `allowed_domains`. smolvm refuses to create a machine whose `credentials` list a host that isn't allowed, since the machine couldn't reach it.

## Network

`sandbox.network` keeps its meaning, and smolvm enforces it on the host for each machine:

- `policy: allow`: open egress.
- `policy: deny`: only the hosts in `allowed_domains`. Private addresses are blocked unless an exact host sets `allow_private_ips: true`, which reopens that host's private addresses. As on the container backend, an exact host (`github.com`) matches only that name, and a wildcard (`*.github.com`) matches only its subdomains. Unlisted names don't resolve.
- `policy: deny` with an empty `allowed_domains`: the machine reaches only workmux's RPC gateway. DNS names still resolve, since smolvm doesn't filter DNS in this case.

The list is read when each machine is created, so a change applies to the next pane. smolvm pins a listed host's addresses at that point.

### Differences from the container backend

- A listed host is reachable on every port, not only 443. SSH to `github.com`, for example, works when `github.com` is listed.
- `max_connections` isn't enforced. workmux logs a note when it's set.

### Blocked connections

smolvm records blocked lookups and connections per machine:

```bash
smolvm machine ls
smolvm machine egress-events --name <machine>
```

On Linux hosts, workmux also starts machines, and each pack build's base machine, with smolvm's seccomp and Landlock confinement of its VMM (`SMOLVM_SECCOMP=enforce`, `SMOLVM_LANDLOCK=enforce`).

## Git metadata boundary

The main repository's `.git` is mounted read-only, with `objects`, `refs`, `logs`, `rr-cache` and the worktree's admin directory as writable shares inside it. smolvm enforces the read-only mount on the host, so the main repository's config and hooks stay read-only even to guest root.

These stay writable to the agent, and can redirect or reconfigure git commands you later run in the worktree on the host:

- the worktree's `.git` file;
- its whole admin directory (`.git/worktrees/<name>`), including `gitdir`, `commondir`, `config.worktree`, `HEAD` and `index`;
- its submodule git directories, which live in the admin directory;
- `objects/info/alternates`, inside the writable `objects` share.

workmux states this gap and asks for acceptance once per project, before the window opens. The acceptance lives in `~/.local/state/workmux/smolvm-acknowledgements/git-metadata-isolation-v1/`, outside anything mounted into the guest. Without a terminal, startup fails unless `sandbox.smolvm.accept_reduced_git_metadata_isolation` is true in your global config. Use Docker, Podman or Apple Container when these files must be protected.

workmux also refuses to start when the repository's git config includes or runs a file inside the worktree or a writable share.

### Git operations that fail in the guest

The `.git` root is read-only, so git operations that rewrite `packed-refs` fail in the guest: deleting a branch, `git fetch --prune` and `git pack-refs`. A commit can print `Unable to create '.../.git/packed-refs.lock': Read-only file system` and still succeed. The container backend mounts the same directory read-only and behaves the same way. Run these operations on the host.

## Limits

- **Linked worktrees only.** smolvm sandboxes run in linked worktrees, such as those `workmux add` creates. The main worktree is refused, because its `.git` directory would be writable to the agent. A coordinator agent on the main branch therefore runs on the host, or in a worktree of its own.
- **Directories only.** smolvm mounts directories, so a file in `extra_mounts` is an error, and `excluded_files` is refused.
- **11 mounts at boot.** A machine boots with at most 11 mounts, and a usual layout takes 8 or 9. Submodules add shares and can go over the limit, which is an error. Shims and `extra_mounts` are mounted after start and don't count.
- **No `:` in paths.** A mounted path containing `:` is refused.
- **Hostnames only in `allowed_domains`.** Under `policy: deny`, each entry must be a hostname of letters, digits and hyphens, or `*.domain`, as smolvm's allowlist takes them; anything else is an error when the config loads.
- **Path length.** smolvm passes the boot mounts on the guest's 2048-byte kernel command line, and a usual layout repeats the repository's path in six of them. Keep the main repository's path under about 120 characters, less with long worktree names or submodules. Over the limit, the pane shows smolvm's `Invalid kernel command line … too long`; move the repository to a shorter path.

## Installing local builds

`workmux sandbox install-dev` cross-compiles your local workmux for Linux and installs it into a directory that new machines mount read-only ahead of the image's workmux on `PATH`. Machines already running keep the image's workmux. See [Installing local builds](/guide/sandbox/features/#installing-local-builds) for the cross-compilation setup.
