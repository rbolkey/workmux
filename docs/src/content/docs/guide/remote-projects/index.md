---
title: "Remote projects"
description: Keep a project's repository, worktrees and agents on an SSH host or an OpenShell sandbox while tmux, the sidebar and the dashboard stay on your workstation
---

A remote project lives on a **remote**: an SSH host or an [OpenShell sandbox](/guide/remote-projects/openshell/). Its repository, worktrees, hooks and agents stay on the remote. Your **workstation**, the machine running tmux, keeps the only tmux server, the sidebar, the dashboard, notifications and the clipboard.

You use workmux commands on a remote project the same way you use them on a local one. `add` creates the worktree on the remote and the window on your workstation. `merge` merges in the remote's clone. The sidebar and dashboard show local and remote agents together. Local projects with no remote work exactly as before.

## How it works

- **Remote workmux.** The same `workmux` binary runs on the remote. It does all the git, worktree, config and hook work there. It reads the project's `.workmux.yaml` from the remote's clone.
- **Control channel.** The workstation daemon opens one control channel per remote by running `workmux remote serve` through the transport (`ssh` or `openshell sandbox exec`). It speaks over that process's stdin and stdout. The remote never connects back to your workstation, so it needs no network route to it.
- **Remote panes.** Agents and shells run on the remote inside a **session holder** ([dtach](https://github.com/crigler/dtach)), so they keep running through a dropped connection or a laptop sleep. A workstation pane is only a view onto its remote pane. It runs `workmux remote attach` and reattaches on its own.
- **Status.** Agent hooks on the remote write each agent's status to a file. Remote workmux sends those files over the control channel, so the sidebar updates live. Changes made while you were disconnected arrive on reconnect, with their original times.

## Set up an SSH remote

Register the remote, then a project on it. Several projects can share one SSH remote.

```bash
workmux remote add devbox --transport ssh --target devbox
workmux project add remote api --remote devbox --repo git@github.com:me/api.git
```

`--target` is a host as you would pass it to `ssh`, so your SSH config, keys, agent and jump hosts apply. The project's clone defaults to `~/api` on the remote (`--path` changes it). Its worktrees go in the usual `api__worktrees` directory beside it.

The first command that needs the project clones the repository on the remote and provisions it:

```bash
workmux add feature --project api
```

```
Cloning git@github.com:me/api.git into ~/api on devbox
Provisioning devbox: ...
```

Provisioning runs only when workmux makes the clone. It:

- installs agent status hooks with `workmux setup --hooks --yes`
- installs nix (and devbox) when the project has a `flake.nix` or `devbox.json` and nix is missing. The installer asks for root, so without passwordless sudo the step fails with a warning and you install nix yourself.
- warns when dtach is missing

A failed step is a warning; the command still runs. Agents in a project with a `flake.nix` or `devbox.json` start in its devshell on the remote, as they do in a [Lima sandbox](/guide/sandbox/lima/#nix-and-devbox-toolchain). Set `sandbox.toolchain: off` in the remote's `.workmux.yaml` to turn that off.

### Requirements on an SSH remote

- **dtach** on the `PATH`. Remote panes cannot start without it.
- **workmux** of the same protocol version as yours, on the `PATH` of non-interactive shells. If it is missing or too old, workmux offers to install a matching one (see [Installing workmux on an SSH remote](#installing-workmux-on-an-ssh-remote)).
- A POSIX login shell (sh, bash or zsh). Arguments with backslashes may be altered on a fish remote.

Output your shell's startup files print on the remote does not break the control channel.

### Installing workmux on an SSH remote

When a remote has no workmux, or one too old to serve this workstation, `workmux remote status`, `remote list`, `remote start` and project commands offer to install a matching workmux:

- If the remote runs the same platform as your workstation, workmux uploads its own binary.
- Otherwise it downloads the release of its own version for the remote's platform and checks the release's checksum. Set `WORKMUX_RELEASES_URL` to download from somewhere other than workmux's GitHub releases; a `file://` directory works.

The binary lands in `${XDG_CACHE_HOME:-~/.cache}/workmux/bin/<version>/workmux` on the remote. The workmux on the remote's `PATH` is never touched.

Your answer is saved as `upload_workmux` on the remote's entry in global config:

- `true`: workmux puts the cache directory first on the `PATH` of everything it runs on that remote: the control channel, remote panes, hooks and shims. After you upgrade workmux on your workstation, it installs the new version again without asking.
- `false`: workmux never asks again.

If no one can answer the prompt (for example, stdin is closed), nothing is saved, and the message names the command to run.

To install by hand, or to upload a binary you built for the remote's platform:

```bash
workmux remote upload devbox
workmux remote upload devbox --binary ./target/x86_64-unknown-linux-musl/release/workmux
```

:::caution
Published workmux releases don't include remote support yet. Until they do, the download gives a workmux that cannot serve a control channel. For a remote on another platform, build workmux for it and pass the binary with `--binary`.
:::

## Set up an OpenShell project

```bash
workmux project add openshell api --repo https://github.com/me/api.git
```

An OpenShell project gets a sandbox of its own, which is its remote, named after the project. workmux creates the sandbox on the first command that needs it. See [OpenShell projects](/guide/remote-projects/openshell/).

## Which project a command acts on

Commands that act on a project (`add`, `open`, `close`, `remove`, `merge`, `list`, `path`, `run`, `rename`, `rebase`, `set-base`, `sync-files` and `init`) find a remote project in this order:

1. `--project NAME`
2. the project of the tmux session you typed the command in. workmux tags each remote project's session, so commands typed there need no `--project`.
3. the current directory, for a local project, as before

Run from a remote worktree's window, commands with no worktree name act on that worktree, as they do locally.

A remote project is identified by its registered name. In the sidebar, dashboard and `workmux status` it shows as `name@remote`, for example `api@devbox`, so it never merges with a local project of the same name. Agent selectors accept `feature`, `api:feature` and `api@devbox:feature`.

`resurrect` and `reap-agents` run locally and then on every registered remote project. Each project's output appears under a `<project> (on <remote>):` heading.

## Where the windows go

In window mode, a remote project's windows go in a workstation session named after the project, whichever session you typed the command in. workmux creates the session if it is missing and adopts it if you already made one with that name.

| Project setting              | Effect                                                                     |
| ---------------------------- | -------------------------------------------------------------------------- |
| `session: NAME`              | Use session `NAME` instead of the project name                             |
| `placement: current-session` | Put windows in the session you ran the command from, as for local projects |
| `--mode session` on `add`    | A session per worktree, as in [session mode](/guide/session-mode/)         |

Set them with `--session` and `--placement` on `workmux project add remote` (or `project add openshell`), or in global config. Everything else, including `mode`, comes from the remote's `.workmux.yaml`.

## Configuration

Remotes and projects live only in your global config (`~/.config/workmux/config.yaml`). The commands above write them for you and keep the rest of the file, comments included. If a project's `.workmux.yaml` sets `remotes:` or `projects:`, workmux ignores them with a warning, so a repository cannot redirect where your commands go.

```yaml
remotes:
  devbox:
    transport: ssh
    target: devbox # an SSH host from your SSH config
    builtin_shims: true # sound playback and clipboard read (default: true)
    host_commands: [just] # other workstation commands it may run
    upload_workmux: true # set by your answer to the install offer

projects:
  api:
    remote: devbox
    git_url: git@github.com:me/api.git
    path: ~/api # the clone on the remote (default: ~/<name>)
    session: api # workstation session (default: the project name)
    placement: project-session # or current-session
  web:
    git_url: https://github.com/me/web.git
    path: ~/web
    openshell: # its own sandbox; see OpenShell projects
      image: ghcr.io/me/web-dev:1
      policy: /Users/me/web-policy.yaml
      providers: [github]
```

To unregister a project, run [`workmux project rm <name>`](/reference/commands/project/#project-rm). It closes the project's workstation windows and leaves the remote and its clone alone. `workmux remote rm` refuses while a project uses the remote. An OpenShell project is removed together with its sandbox by `workmux remote rm <name>`.

## Commands typed in a remote pane

A remote pane's shell can run workmux too, so an agent on the remote can coordinate other agents:

- `add`, `open`, `close`, `remove`, `merge` and `rename` run on the remote. Their windows appear on your workstation.
- `send`, `capture` and `wait` run on your workstation, limited to agents of projects on that remote. `send` takes only a name and text; `capture` only `--lines`; `wait` only `--status`, `--timeout` and `--any`.

While the workstation daemon is not connected to the remote, these commands fail with `workstation not connected` before they do anything.

## Shims and host commands

A remote can run a few commands on your workstation, through the control channel:

- **Built-in shims**, on by default: sound playback (so hook sounds play on your workstation) and clipboard read (so image paste into a remote agent works). Turn them off for a remote with `builtin_shims: false` or `workmux remote add --no-builtin-shims`.
- **Host commands**: any other command must be allowlisted for that remote with `host_commands` or `--host-command`. It runs in workmux's host-exec sandbox on your workstation, in a working directory of its own. Everything else is refused.

A remote can act only on the tmux windows and panes of its own projects. Requests for any other window, pane or session are refused.

## When a remote disconnects

Agents keep running on the remote. On your workstation:

- **Panes** show `reconnecting to <remote>… (r: retry now, q: close)` with the reason, and retry on their own, backing off from 1 s to 30 s. They redraw when they reattach. `q` closes the workstation pane but leaves its remote pane running. A pane closes by itself only when its remote pane has really ended.
- **Sidebar and dashboard** keep the remote's agents as **stale**: dimmed, with their last known status, and never flagged as interrupted. They refresh when the remote reconnects.
- **`workmux status`** shows a stale agent as, for example, `done (stale)`. `wait` does not count a stale agent's status. It prints that the remote is disconnected and keeps waiting until the remote reconnects or `--timeout` expires. `send` to a stale agent fails.
- **Dashboard** still shows PR and check status for remote branches. workmux queries GitHub from your workstation with the project's git URL, so this works while a remote is stopped or unreachable. SSH host aliases in the URL are resolved through your SSH config.

To get panes back after their windows were closed outside workmux, or after tmux itself died, run `workmux open NAME --project P` or `workmux resurrect`. Each pane of the layout attaches to a remote pane that is still running, in the order they started, instead of starting its command again. `workmux close` and `remove` end the worktree's remote panes.

The control channel stays open while any workstation pane shows one of that remote's panes, and for a minute after the last command that used it. It reconnects with backoff after a drop.

## Checking a remote

```bash
workmux remote list      # each remote and whether it is reachable
workmux remote status    # versions, warnings, and the error when unreachable
```

An unreachable remote is reported in the transport's own words, so an SSH auth failure looks different from a stopped sandbox. `remote status` warns when:

- dtach is missing on the remote
- an agent on the remote has no status hooks (its agents show no status in the sidebar; run `workmux setup --hooks` there)
- the remote runs a different workmux release with the same protocol version (this works, with a warning). A different protocol version is refused, and the error names the version to install.

## Limits

- Commands that would open your editor refuse for a remote project: `add --prompt-editor`, `add --auto-name` without `-p` or `-P`, `open --prompt-editor`, and a `merge` that needs a commit message (staged changes or a squash). Pass the prompt with `-p TEXT`. A prompt file given with `-P FILE` is read on your workstation (up to 1 MiB).
- Command output from a relayed `send`, `capture` or `wait` typed in a remote pane arrives when the command exits, not streamed.
- Files are never synced between your workstation and a remote; code moves through git. An existing local project can't be moved to a remote.
- Only tmux is supported on the workstation for remote projects.
