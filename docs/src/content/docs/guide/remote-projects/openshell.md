---
title: "OpenShell projects"
description: Give each project its own OpenShell sandbox, cloned from git, with no workstation mounts
---

An OpenShell project lives in an [OpenShell](https://github.com/NVIDIA/OpenShell) sandbox of its own. The sandbox is the project's remote, so projects are isolated from each other and from your workstation. Everything in [Remote projects](/guide/remote-projects/) applies; this page covers what is specific to OpenShell.

- The repository is cloned inside the sandbox, and its worktrees live beside the clone.
- workmux never requests a bind mount, so no workstation file is visible in the sandbox.
- The control channel runs over `openshell sandbox exec`, so the sandbox's network policy needs no rule for workmux.
- The OpenShell sandbox is the project's sandbox. In it, workmux runs no container or Lima sandbox: a `sandbox:` setting that asks for one, from the project's `.workmux.yaml` or global config, or `add --sandbox`, is ignored there with a note. The rest of the config, hooks included, still applies.

## Set up a project

Commit the sandbox settings in the repository's `.workmux.yaml`, and list providers in your global config (`~/.config/workmux/config.yaml`):

```yaml
# .workmux.yaml, in the repository
sandbox:
  enabled: true
  backend: openshell
  openshell:
    image: ghcr.io/me/api-dev:1
    policy: api-policy.yaml
    agents: [claude]
```

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  openshell:
    providers: [github]
```

| `sandbox.openshell` key | Meaning                                                                                                                                                                                                    |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `image`                 | Image the sandbox is built from. It must contain git and dtach. Default: workmux's own image                                                                                                               |
| `policy`                | Policy file merged on top of workmux's [base policy](#policy), relative to the repository                                                                                                                  |
| `providers`             | OpenShell providers to attach, for credentials. No other provider is attached. Global config only. `gh-login` is special: see [Your gh login](#your-gh-login-provider-gh-login)                          |
| `agents`                | Agents the image installs; the first is the sandbox's default agent                                                                                                                                       |
| `path`                  | The clone's path in the sandbox. Default: `~/<project>`                                                                                                                                                    |

Settings merge as any other setting does: the repository's value overrides your global config's, key by key. `providers` is the exception. It is read only from global config, and a repository's `.workmux.yaml` setting it is ignored with a warning, so a repository can't commit `providers: [gh-login]` and receive your GitHub login. Every OpenShell sandbox gets the same providers; tokens scoped to what your projects need bound what each one can reach.

The project is named after the repository: the directory name of your clone. The sandbox clones the repository from your clone's `origin`. The sandbox is named after your clone in the format of a project's Lima VM name: `wm-`, up to 7 characters of the directory name and the first 8 hex digits of the sha256 of the clone's full path, such as `wm-api-79949073`, within OpenShell's rules (lowercase letters, digits and hyphens, up to 19 characters). Config can't name a sandbox, so no repository can reach another's. Two clones get two sandboxes, even of the same repository, and a clone moved to another path gets a new one.

The sandbox is the project's remote, so there is nothing to register. Run in the repository, `workmux sandbox status` shows the sandbox before any other command has created it.

Git credentials and agent API keys should come from OpenShell providers, so that secrets never sit in the sandbox environment. See [Git and GitHub](#git-and-github) for git.

### Other sandbox settings

The `sandbox:` settings every backend reads mean the same for OpenShell as for containers and Lima, and are read only from global config, as there:

- **`host_commands`**: the commands the sandbox may run on your workstation (see [Shims and host commands](/guide/remote-projects/#shims-and-host-commands)). Each one listed is a way out of the sandbox, so list only what the project needs; a command you used for Lima, such as a credential helper, may not be needed here.
- **`env_passthrough`**: workstation environment variables set in the sandbox for each command workmux runs there, and so in the panes it starts. A variable that is unset on your workstation is left out.
- **`env`**: variables set to the values in your config, in the same places. A variable in both `env` and `env_passthrough` gets the value from `env`.
- **`extra_mounts`**: ignored, since OpenShell never mounts workstation directories. workmux says so, with a note, when it creates or starts the sandbox.

:::caution
`env_passthrough` and `env` put the real value in the sandbox. A variable that a provider also supplies, such as `CLAUDE_CODE_OAUTH_TOKEN` or `GITHUB_TOKEN`, then overrides the provider's placeholder, and the agent can read the secret. Leave such variables to the provider.
:::

## Git and GitHub

### Credentials

OpenShell gives the sandbox only a placeholder token (in `GITHUB_TOKEN` or `GH_TOKEN`), and its proxy swaps in the real one on requests to the provider's endpoints. git sends no credentials unless a credential helper supplies them, so on its own `git clone` of a private repository fails with "could not read Username".

When an attached provider's profile reaches `github.com`, workmux's image sets a credential helper for `https://github.com` only. It hands git `x-access-token` and the placeholder from `GH_TOKEN`, else `GITHUB_TOKEN`. The helper adds no exposure: the placeholder is already in the environment, and the proxy resolves it only for the provider's endpoints. It replaces any other helper the image's `/etc/gitconfig` sets for github.com, such as `gh auth git-credential`. workmux never uses git's `store` helper and never puts a token in an image.

What git and agents may do on GitHub is set by the provider's profile and the token's scope:

- **OpenShell's stock `github` profile is read-only:** clone and fetch work; push (`git-receive-pack`) and API writes, such as creating a pull request, are denied. The stock profiles aren't installed in a gateway by default; import one with `openshell provider profile import`.
- **Push and pull requests** need a profile that allows `git-receive-pack` and API writes. The easy path is [`gh-login`](#your-gh-login-provider-gh-login), which gives the sandbox your workstation's `gh` login. For tighter access, [bring your own provider](#bring-your-own-provider) made from a fine-grained personal access token.

### Your gh login (provider `gh-login`)

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  openshell:
    providers: [gh-login]
```

`gh-login` is a provider name workmux reserves. Attaching it gives the sandbox your workstation's `gh` login, with nothing else to create: no token, profile or provider.

- workmux ships a GitHub profile, `workmux-github`, that allows clone, fetch and push, and read-write REST and GraphQL, for git and `gh` (including nix's, from a baked devshell). It imports the profile into the gateway when it's missing or differs from the shipped one.
- On first use, workmux creates the provider `gh-login` from that profile, labelled `workmux.managed=gh-login`, holding the token `gh auth token` prints. It passes the token to `openshell` in the environment, never on its command line, and never logs it.
- A `gh-login` provider on the gateway without that label isn't workmux's: attaching it is an error, and workmux never changes it.
- OpenShell's proxy can only read credentials from the gateway, so the gateway keeps a copy of the token. workmux updates it from `gh` each time it creates the sandbox, runs a command in it (such as `workmux add`) or attaches a pane to it, so a new `gh` login reaches a running sandbox at the next attach.
- `gh` missing or logged out is an error naming `gh auth login`, when the sandbox is created and whenever the token is refreshed. workmux never falls back to no credentials.
- At most one attached provider may give GitHub credentials: `gh-login` beside another provider whose profile reaches github.com is an error when the sandbox is created. It is also an error when workmux can't ask the gateway which providers give GitHub credentials.

The gateway normally runs on your workstation, so the copy stays there. If your OpenShell gateway runs on another machine, the token is stored on that machine.

**What it grants.** Everything your `gh` login can do. Its token usually has the `repo` and `workflow` scopes: every repository you can reach, public or private, including pushing changes to GitHub Actions workflows. The agent can also open pull requests and call the API as you. workmux doesn't narrow it; to limit access, use a fine-grained personal access token instead (below).

**Protect your default branch.** OpenShell can't restrict pushes by branch, so with push allowed, an agent can push to `main`. Protect it with a GitHub ruleset or branch protection.

**Revoking access.** Run `gh auth logout` on your workstation: the next attach or command that refreshes the token fails, and the copy is no longer updated. To remove the copy at once, delete the provider (after deleting the sandboxes it is attached to): `openshell provider delete gh-login`.

### Bring your own provider

A fine-grained personal access token limits the agent to the repositories and permissions you grant it (a GitHub App can later do the same). The token's own scope is the limit:

1. Import a GitHub profile into the gateway. OpenShell's stock profiles aren't preinstalled, so even the stock `github` profile must be imported first (`openshell provider profile import --global -f github.yaml`). The stock profile is read-only.
2. Create a provider from the token, passing it through the environment: `GITHUB_TOKEN=… openshell provider create --name api-github --type github --global-profile --credential GITHUB_TOKEN`.
3. Attach it by listing `api-github` in `sandbox.openshell.providers`.
4. To allow push for just the project's repository on top of the read-only profile, add a `policy` file:

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

The proxy substitutes credentials only in HTTPS, and no profile allows SSH. workmux doesn't put an SSH key in the sandbox or forward your SSH agent, because either would hand the sandbox a real secret. Instead, workmux rewrites SSH GitHub URLs to HTTPS and says so:

- When your clone's `origin` is an SSH URL on github.com (`git@github.com:me/api.git` or `ssh://git@github.com/me/api.git`), the sandbox clones its HTTPS form, and says so when it is created:

  ```text
  Note: The repository's origin uses SSH, which OpenShell sandboxes don't support; the sandbox reaches github.com over HTTPS with `github` credentials instead.
  ```

  The sandbox's identity is the provider's, which may not be your SSH key's: a work-account key and a personal `gh` login reach different repositories. With no GitHub provider attached, the note adds that public repositories will clone, but private ones and push need `gh-login` or another GitHub provider. workmux can't tell whether a repository is private, so it doesn't fail.

- An SSH origin on any other host fails: set `origin` to an `https://` URL.
- `sandbox status` and the first-use clone show the URL as `https://github.com/me/api.git (rewritten from SSH: git@github.com:me/api.git)`.
- Inside the sandbox, the image rewrites `git@github.com:` and `ssh://git@github.com/` to `https://github.com/` with git's `url.<base>.insteadOf`, so submodules and URLs an agent types go over HTTPS too. On your workstation, workmux still clones the original URL to look for a flake devshell.

Not covered: SSH hosts other than github.com. For private nix flake inputs, in the devshell bake and in the sandbox, see [Nix devshells](#nix-devshells).

## Lifecycle

workmux creates and starts the sandbox for you, and stops, rebuilds or deletes it only when you ask, with the [`workmux sandbox`](/reference/commands/sandbox/#openshell-commands) commands Lima VMs use.

- **Create.** The first command that acts in the project, such as `workmux add`, builds the image, creates the sandbox and clones the repository, with the notice `Creating remote api on first use; …`.
- **Start.** A command that acts in a stopped sandbox starts it first (`Starting remote api…`).
- **Never just to look.** Commands that only report never create or start a sandbox: `list`, `path`, `status`, `wait`, `sandbox status`, the sidebar and the dashboard. With no sandbox yet, they say `workmux add <branch>` creates it; with a stopped one, that the next command that needs it starts it. `resurrect` and `reap-agents` don't create or start one either; they skip it with a notice and don't count it as a failure.

```bash
# run in your clone of the repository
workmux sandbox status   # the sandbox's state, versions, policy and agents
workmux sandbox shell    # a shell in the sandbox, in the current worktree
workmux sandbox stop     # stop sandboxes; their workspaces survive
workmux sandbox rebuild  # delete and recreate, carrying committed work across
workmux sandbox prune    # delete workmux's sandboxes, after naming the work they hold
```

**Stopping ends every process in the sandbox**, agents included. The worktrees and status files survive, but agents do not: once the sandbox starts again, workstation panes find their remote panes ended and close. There is no start command; the next command that needs the sandbox, such as `workmux open`, starts it. Reopen the worktree with `workmux open`.

`stop` and `prune` act on every sandbox whose name has workmux's `wm-` prefix, and on no other: a sandbox named by an older workmux, without the prefix, is no longer found, and the next command in its repository creates a `wm-` one. Delete the old one with `openshell sandbox delete <name>` once its work is pushed.

### Deleting sandboxes

`workmux sandbox prune` deletes workmux's sandboxes and everything in them, because nothing is mounted from your workstation. Before it asks for confirmation, it checks each one, worktree by worktree and branch by branch, as `workmux remove` checks a worktree:

- **Uncommitted changes** refuse, naming each sandbox, as does a sandbox that could not be checked.
- **Commits on no remote-tracking branch**, which exist only in the sandbox and are lost with it, and **branches not merged into their base** (the base `workmux add` recorded, else the default branch) are named in the confirmation.

It starts a stopped sandbox to check, and stops it again if nothing is deleted. Once a sandbox is deleted, it closes its project's workstation windows. The next command in the repository that needs a sandbox creates a new one. `--force` skips the check and the confirmation and deletes them all, uncommitted changes included.

### Rebuilding a sandbox

Some changes need a new sandbox: a narrowed policy, a changed image, or a new devshell. workmux never rebuilds on its own; it warns and points at `workmux sandbox rebuild`. A rebuild:

1. refuses while any worktree has uncommitted changes, and names them
2. confirms, naming unmerged work, because agents' remote panes are lost (`-y` skips this)
3. bundles every ref and keeps the bundle in `$XDG_STATE_HOME/workmux/recreate/<sandbox>/` until the restore succeeds
4. deletes and creates the sandbox
5. restores the clone: branches, tags, remotes, upstreams and every worktree at its path
6. reinstalls agent hooks

If the restore fails, run `workmux sandbox rebuild` again. It resumes from the kept bundle without deleting the sandbox again.

## Image

workmux builds the sandbox image with `docker build` on the Docker daemon your OpenShell gateway uses, so it is native to the sandbox's architecture. The image is tagged `workmux-openshell-<sandbox>:<hash>`, and is rebuilt when its inputs change.

- **Default image:** `ubuntu:24.04` with git, dtach, curl and workmux.
- **Git:** every image gets workmux's [git settings](#git-and-github) appended to its `/etc/gitconfig`, so your `image` needs `/bin/sh`.
- **Agents:** every image gets the project's [agents](#agents-in-the-sandbox) that workmux can install.
- **Your image (`image`):** used as the base as it is. It must contain git and dtach, because nothing can be installed in a running sandbox.

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

The image installs the agents the project names, and the first one is the sandbox's default agent. Name them with `sandbox.openshell.agents`. By default the sandbox gets your default agent's type: a workstation path such as `~/.local/bin/claude`, or a named agent with `type: claude`, means `claude`.

| Agent    | Installed at            | Stock provider type |
| -------- | ----------------------- | ------------------- |
| `claude` | `/usr/local/bin/claude` | `claude-code`       |
| `codex`  | `/usr/local/bin/codex`  | `codex`             |

These are the paths OpenShell's example provider profiles allow, so a provider made from one of those profiles gives the agent its credentials. So does a provider made from a custom profile whose `binaries` include the agent's path, such as a `claude-code-oauth` profile that authenticates Claude Code with a subscription OAuth token. Each agent is installed in a build stage of its own, so your `image` needs no curl. Claude Code is the glibc build, so it needs a glibc base image. Codex is a static musl build and runs on any base. workmux can't install other agents, so creating the sandbox warns and your `image` must already contain them.

The sandbox runs the agent's plain CLI, not your workstation's agent command. The image records the default agent in `/etc/workmux/default-agent`. Remote workmux uses it when no config in the sandbox names an agent, and counts it as a configured agent, so a project without panes of its own gets an agent pane. A `--agent` given to `add` is passed to remote workmux as it is, so give it the agent's plain CLI (`-a codex`) rather than a workstation alias.

`sandbox status` lists each agent and the attached providers whose profile allows its binary, whatever the provider's type. It warns when none does, and so does creating the sandbox:

```text
agent:    claude: installed at /usr/local/bin/claude; its credentials come from a provider whose profile allows it, such as one of type claude-code
warning: no provider attached to the sandbox has a profile that allows /usr/local/bin/claude, so claude has no credentials there; create one with `openshell provider create --type claude-code` and list it in sandbox.openshell.providers in your global config
```

The agent still starts without one, but it can't authenticate. The sandbox's network policy reaches the agent's endpoints only through its provider.

## Nix devshells

An OpenShell sandbox follows `sandbox.toolchain` as a Lima sandbox does. When it resolves to `flake` (the default `auto` with a `flake.nix` in the repository, or `toolchain: flake`), the sandbox has nix and the project's devshell, baked into its image:

1. On your workstation, workmux shallow-clones the project's git URL.
2. A `nixos/nix` build stage builds the devshell. The image gets nix, the devshell's closure and the flake's inputs, in a store under `/nix` that the sandbox user owns.
3. A wrapper at `/usr/local/bin/workmux-devshell` enters the devshell. Agents start as `workmux-devshell bash -c '…'`, so they start at once, with nothing to build.

`toolchain: off` gives a sandbox without nix, with `/nix` read-only and no nix network rules. So does `toolchain: devbox`, or `auto` when the repository has a `devbox.json`.

### Working on the flake

nix in the sandbox is for working on the flake itself: `nix flake check`, `nix develop` while you change the flake, or `nix profile install` to test that it installs. It builds without nix's own build sandbox (`sandbox = false`), which needs kernel namespaces OpenShell sandboxes don't have. The devshell and the flake's inputs are in its store already, and are garbage collection roots, so `nix develop` doesn't fetch them again. `nix profile install` puts commands in `~/.nix-profile/bin`, which isn't on `PATH`.

`nix develop` sees your edits to the flake at once. Panes and agents keep the baked devshell until `workmux sandbox rebuild` builds a new image, so changing the environment for agents still means committing the flake change, pushing it and rebuilding the sandbox.

nix reaches the binary cache (`cache.nixos.org`), the flake registry (`channels.nixos.org`, `releases.nixos.org`) and GitHub (`github.com`, `api.github.com`, `codeload.github.com`, read-only), through the `workmux-nix` rule in the sandbox's [policy](#policy). For private `github:` inputs, the sandbox's `nix` command gives nix the placeholder token of the provider that gives GitHub credentials (`GH_TOKEN` or `GITHUB_TOKEN`) as its access token for github.com, and OpenShell's proxy swaps in the real token. `git+https://github.com/…` inputs use git's credential helper, as git does. The sandbox has no network interface besides loopback, so nix warns `you don't have Internet access` and fetches anyway. Other hosts, such as another binary cache, need a rule in your policy file for the nix binary (`/nix/store/*/bin/nix`). The token and a fix for garbage collection come from the image's `nix` command (`/usr/local/bin/nix` and the `nix-*` commands beside it); a nix your devshell puts first on `PATH` reads the same configuration but goes without them.

**Private flake inputs.** The bake fetches private GitHub inputs only for a sandbox that attaches `gh-login`, because that project already gives its sandbox your `gh` login. workmux hands the build the token `gh auth token` prints as a Docker build secret, never on a command line and never in the image. Only the step that fetches the inputs gets it, before anything of the project's is built:

- `github:` inputs get it as nix's `access-tokens` for github.com.
- `git+https://github.com/…` inputs get it from a git credential helper.
- `git+ssh://git@github.com/…` inputs go over HTTPS with it, because the build has no SSH.

The step that builds the devshell has no secret, so the flake's builders and `shellHook` can't read the token. Any other project's bake gets no token: a private input fails it with nix's `HTTP error 404` or `Cannot find Git revision`, and workmux says private GitHub flake inputs need `gh-login` in `sandbox.openshell.providers`. Private inputs on other git servers aren't covered.

The image is fixed when the sandbox is created. It follows what the bake reads: `flake.nix`, `flake.lock`, and the files and directories the flake names by path (`./nix/shell.nix`, `"${self}/nix"`), followed through the `.nix` files among them. When any of them in the clone no longer match the baked ones, `sandbox status` warns, and `workmux sandbox rebuild` gives the sandbox a new image. Changes to the rest of the repository don't. Neither does a file the flake reads through a path it builds from a string at evaluation time.

A devbox project gets no devshell and no nix in a sandbox. Put its toolchain in your `image`. `sandbox status` says so.

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

The PTY devices are there because the session holder allocates terminals, and `/nix` for the baked devshell. There are no network rules. A sandbox with nix also gets `/nix` read-write and the `workmux-nix` network rule (see [Working on the flake](#working-on-the-flake)).

Your `policy` file is merged on top:

- `filesystem_policy`: each list is the union of both, without duplicates. A full filesystem section is always written, so your file can't drop what workmux needs.
- `network_policies`: the rules of both. A rule name in both files is refused.
- Any other section: yours.

`workmux sandbox status` shows the sandbox's effective policy (read-only and read-write paths, workdir, network rules) and warns when it differs from the project's.

When you edit the policy file, the next command brings a running sandbox in line. Network rules apply at once. Filesystem grants that are added take effect once `workmux sandbox stop` has stopped the sandbox and the next command has started it. A grant that is removed needs `workmux sandbox rebuild`, because OpenShell can't narrow a live sandbox's filesystem policy; workmux warns instead.

## Troubleshooting

### Creating a sandbox fails with "No such image"

If creating or starting a sandbox fails with `ControlSupervisorStartFailed … No such image: sha256:…`, the gateway's supervisor image is gone from Docker. The gateway looks the image up once, when it starts, so it keeps failing until you restart the gateway, which pulls the image again (for a gateway run by launchd: `launchctl kickstart -k gui/$(id -u)/<gateway label>`).

An image prune can remove it, and so can Kubernetes image garbage collection when the Docker engine is shared with Kubernetes (OrbStack or Docker Desktop with Kubernetes turned on): once the Docker disk is over 85% full, the kubelet deletes images no container is using, the gateway's included. To avoid it, turn off the engine's Kubernetes if you don't use it (`orb config set k8s.enable false` in OrbStack), or keep the Docker disk under 85%.
