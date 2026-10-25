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
workmux project add openshell api --repo https://github.com/me/api.git \
  --image ghcr.io/me/api-dev:1 \
  --policy ./api-policy.yaml \
  --provider github
```

| Option            | Meaning                                                                                                                                                                 |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--image`         | Image the sandbox is built from. It must contain git and dtach. Default: workmux's own image                                                                            |
| `--policy FILE`   | Policy file merged on top of workmux's [base policy](#policy). Stored as an absolute path                                                                               |
| `--provider NAME` | OpenShell provider to attach, for credentials (repeatable). No other provider is attached. `gh-login` is special: see [Your gh login](#your-gh-login-provider-gh-login) |
| `--agent NAME`    | Agent the image installs (repeatable); the first is the sandbox's default agent                                                                                         |
| `--sandbox NAME`  | Sandbox name. Default: the project name                                                                                                                                 |
| `--path`          | The clone's path in the sandbox. Default: `~/<name>`                                                                                                                    |

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

A project added with `--provider gh-login` lists `gh-login` among its `providers:`.

The project's remote is implicit and has the project's name, so commands such as `workmux remote status api` work without a `remote add`.

Git credentials and agent API keys should come from OpenShell providers, so that secrets never sit in the sandbox environment. See [Git and GitHub](#git-and-github) for git.

## Git and GitHub

### Credentials

OpenShell gives the sandbox only a placeholder token (in `GITHUB_TOKEN` or `GH_TOKEN`), and its proxy swaps in the real one on requests to the provider's endpoints. git sends no credentials unless a credential helper supplies them, so on its own `git clone` of a private repository fails with "could not read Username".

When an attached provider's profile reaches `github.com`, workmux's image sets a credential helper for `https://github.com` only. It hands git `x-access-token` and the placeholder from `GH_TOKEN`, else `GITHUB_TOKEN`. The helper adds no exposure: the placeholder is already in the environment, and the proxy resolves it only for the provider's endpoints. It replaces any other helper the image's `/etc/gitconfig` sets for github.com, such as `gh auth git-credential`. workmux never uses git's `store` helper and never puts a token in an image.

What git and agents may do on GitHub is set by the provider's profile and the token's scope:

- **OpenShell's stock `github` profile is read-only:** clone and fetch work; push (`git-receive-pack`) and API writes, such as creating a pull request, are denied. The stock profiles aren't installed in a gateway by default; import one with `openshell provider profile import`.
- **Push and pull requests** need a profile that allows `git-receive-pack` and API writes. The easy path is [`--provider gh-login`](#your-gh-login-provider-gh-login), which gives the sandbox your workstation's `gh` login. For tighter access, [bring your own provider](#bring-your-own-provider) made from a fine-grained personal access token.

### Your gh login (`--provider gh-login`)

```bash
workmux project add openshell api --provider gh-login --repo https://github.com/me/api.git
```

`gh-login` is a provider name workmux reserves. Attaching it gives the sandbox your workstation's `gh` login, with nothing else to create: no token, profile or provider.

- workmux ships a GitHub profile, `workmux-github`, that allows clone, fetch and push, and read-write REST and GraphQL, for git and `gh` (including nix's, from a baked devshell). It imports the profile into the gateway when it's missing or differs from the shipped one.
- On first use, workmux creates the provider `gh-login` from that profile, labelled `workmux.managed=gh-login`, holding the token `gh auth token` prints. It passes the token to `openshell` in the environment, never on its command line, and never logs it.
- A `gh-login` provider on the gateway without that label isn't workmux's: attaching it is an error, and workmux never changes it.
- OpenShell's proxy can only read credentials from the gateway, so the gateway keeps a copy of the token. workmux updates it from `gh` each time it creates the sandbox, runs a command in it (such as `workmux add`) or attaches a pane to it, so a new `gh` login reaches a running sandbox at the next attach.
- `gh` missing or logged out is an error naming `gh auth login`, at `project add` and whenever the token is refreshed. workmux never falls back to no credentials.
- At most one attached provider may give GitHub credentials: `gh-login` beside another provider whose profile reaches github.com is an error, at `project add` and when the sandbox is created. So is a gateway workmux can't ask which providers do.

The gateway normally runs on your workstation, so the copy stays there. If your OpenShell gateway runs on another machine, the token is stored on that machine.

**What it grants.** Everything your `gh` login can do. Its token usually has the `repo` and `workflow` scopes: every repository you can reach, public or private, including pushing changes to GitHub Actions workflows. The agent can also open pull requests and call the API as you. workmux doesn't narrow it; to limit access, use a fine-grained personal access token instead (below).

**Protect your default branch.** OpenShell can't restrict pushes by branch, so with push allowed, an agent can push to `main`. Protect it with a GitHub ruleset or branch protection.

**Revoking access.** Run `gh auth logout` on your workstation: the next attach or command that refreshes the token fails, and the copy is no longer updated. To remove the copy at once, delete the provider (after deleting the sandboxes it is attached to): `openshell provider delete gh-login`.

### Bring your own provider

A fine-grained personal access token limits the agent to the repositories and permissions you grant it (a GitHub App can later do the same). The token's own scope is the limit:

1. Import a GitHub profile into the gateway. OpenShell's stock profiles aren't preinstalled, so even the stock `github` profile must be imported first (`openshell provider profile import --global -f github.yaml`). The stock profile is read-only.
2. Create a provider from the token, passing it through the environment: `GITHUB_TOKEN=… openshell provider create --name api-github --type github --global-profile --credential GITHUB_TOKEN`.
3. Attach it with `--provider api-github`.
4. To allow push for just the project's repository on top of the read-only profile, add a `--policy` file:

   ```yaml
   network_policies:
     api_push:
       endpoints:
         - host: github.com
           port: 443
           protocol: rest
           enforcement: enforce
           rules:
             - allow: { method: GET, path: /me/api.git/info/refs* }
             - allow: { method: POST, path: /me/api.git/git-upload-pack }
             - allow: { method: POST, path: /me/api.git/git-receive-pack }
       binaries:
         - path: /usr/bin/git
   ```

   The proxy swaps in the provider's credential on the paths the policy allows; a push to any other repository is denied.

### SSH is not supported in OpenShell sandboxes

The proxy substitutes credentials only in HTTPS, and no profile allows SSH. workmux doesn't put an SSH key in the sandbox or forward your SSH agent, because either would hand the sandbox a real secret. Instead, SSH GitHub URLs are rewritten to HTTPS, visibly:

- `project add openshell` with an SSH `--repo` on github.com (`git@github.com:me/api.git` or `ssh://git@github.com/me/api.git`) stores the HTTPS form as the project's `git_url` and the original as `ssh_git_url`, and says so:

  ```text
  Note: `--repo` uses SSH, which OpenShell sandboxes don't support; the sandbox reaches github.com over HTTPS with `github` credentials instead.
  ```

  The sandbox's identity is the provider's, which may not be your SSH key's: a work-account key and a personal `gh` login reach different repositories. With no GitHub provider attached, the note adds that public repositories will clone, but private ones and push need `--provider gh-login` or another GitHub `--provider`. workmux can't tell whether a repository is private, so it doesn't fail.

- An SSH `--repo` on any other host fails at `project add`.
- `remote status` and the first-use clone show the URL as `https://github.com/me/api.git (rewritten from SSH: git@github.com:me/api.git)`.
- Inside the sandbox, the image rewrites `git@github.com:` and `ssh://git@github.com/` to `https://github.com/` with git's `url.<base>.insteadOf`, so submodules and URLs an agent types go over HTTPS too. On your workstation, workmux still clones the original URL to look for a flake devshell.

Not covered: SSH hosts other than github.com, and private nix flake inputs (`git+ssh://` inputs may ignore `insteadOf`, and `github:` inputs use nix's `access-tokens`, not git's credential helper).

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
- **Git:** every image gets workmux's [git settings](#git-and-github) appended to its `/etc/gitconfig`, so your `--image` needs `/bin/sh`.
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

| Agent    | Installed at            | Stock provider type |
| -------- | ----------------------- | ------------------- |
| `claude` | `/usr/local/bin/claude` | `claude-code`       |
| `codex`  | `/usr/local/bin/codex`  | `codex`             |

These are the paths OpenShell's example provider profiles allow, so a provider made from one of those profiles gives the agent its credentials. So does a provider made from a custom profile whose `binaries` include the agent's path, such as a `claude-code-oauth` profile that authenticates Claude Code with a subscription OAuth token. Each agent is installed in a build stage of its own, so your `--image` needs no curl. Claude Code is the glibc build, so it needs a glibc base image. Codex is a static musl build and runs on any base. workmux can't install other agents, so `project add` warns and your `--image` must already contain them.

The sandbox runs the agent's plain CLI, not your workstation's agent command. The image records the default agent in `/etc/workmux/default-agent`. Remote workmux uses it when no config in the sandbox names an agent, and counts it as a configured agent, so a project without panes of its own gets an agent pane. A `--agent` given to `add` is passed to remote workmux as it is, so give it the agent's plain CLI (`-a codex`) rather than a workstation alias.

`project add openshell` and `remote status` list each agent with which attached providers have a profile that allows its binary, whatever their type. Both warn when none does, and so does creating the sandbox:

```text
Agent claude: installed at /usr/local/bin/claude; its credentials come from a provider whose profile allows it, such as one of type claude-code
warning: no provider attached to the sandbox has a profile that allows /usr/local/bin/claude, so claude has no credentials there; create one with `openshell provider create --type claude-code` and list it in the project's `openshell: providers:`
```

The agent still starts without one, but it can't authenticate. The sandbox's network policy reaches the agent's endpoints only through its provider.

## Nix devshells

Nix cannot install or build inside an OpenShell sandbox. Instead, for a project with a `flake.nix`, workmux bakes the devshell into the image:

1. On your workstation, workmux shallow-clones the project's git URL.
2. A `nixos/nix` build stage builds the devshell and copies its closure into the image's `/nix/store`.
3. A wrapper at `/usr/local/bin/workmux-devshell` enters it. Agents start as `workmux-devshell bash -c '…'`.

The image is fixed when the sandbox is created. When the clone's `flake.lock` no longer matches the baked one, `remote status` warns, and `workmux remote recreate` rebuilds the image.

A devbox project gets no devshell in a sandbox. Put its toolchain in your `--image`. `project add openshell` and `remote status` both say so.

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

`workmux remote add NAME --transport openshell --target SANDBOX` registers an existing sandbox that you manage. workmux can start and stop it, but never creates it, and `remote rm` only unregisters it. To have workmux create and own the sandbox, use `project add openshell` instead.

## Troubleshooting

### Creating a sandbox fails with "No such image"

If creating or starting a sandbox fails with `ControlSupervisorStartFailed … No such image: sha256:…`, the gateway's supervisor image is gone from Docker. The gateway looks the image up once, when it starts, so it keeps failing until you restart the gateway, which pulls the image again (for a gateway run by launchd: `launchctl kickstart -k gui/$(id -u)/<gateway label>`).

An image prune can remove it, and so can Kubernetes image garbage collection when the Docker engine is shared with Kubernetes (OrbStack or Docker Desktop with Kubernetes turned on): once the Docker disk is over 85% full, the kubelet deletes images no container is using, the gateway's included. To avoid it, turn off the engine's Kubernetes if you don't use it (`orb config set k8s.enable false` in OrbStack), or keep the Docker disk under 85%.
