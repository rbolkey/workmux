---
title: "Remote projects"
description: Keep a project's repository, worktrees and agents in an OpenShell sandbox while tmux, the sidebar and the dashboard stay on your workstation
---

A remote project lives on a **remote**: an [OpenShell sandbox](/guide/remote-projects/openshell/) of its own. Its repository, worktrees, hooks and agents stay in the sandbox. Your **workstation**, the machine running tmux, keeps the only tmux server, the sidebar, the dashboard, notifications and the clipboard.

Where a project runs comes from its config, resolved the way workmux always resolves config: your global config merged with the repository's `.workmux.yaml`. A repository whose config says `sandbox.backend: openshell` is a remote project. There is nothing to register: run workmux in a clone of the repository and it targets the sandbox.

You use workmux commands on a remote project the same way you use them on one on the workstation. `add` creates the worktree in the sandbox and the window on your workstation. `merge` merges in the sandbox's clone. The sidebar and dashboard show the workstation's agents and remote agents together. Projects with no `openshell` backend work exactly as before.

## How it works

- **Remote workmux.** The same `workmux` binary runs in the sandbox. It does all the git, worktree, config and hook work there. It reads the project's `.workmux.yaml` from the sandbox's clone.
- **Control channel.** The workstation daemon opens one control channel per remote by running `workmux remote serve` through `openshell sandbox exec`. It speaks over that process's stdin and stdout. The sandbox never connects back to your workstation, so it needs no network route to it.
- **Remote panes.** Agents and shells run in the sandbox inside a **session holder** ([dtach](https://github.com/crigler/dtach)), so they keep running through a dropped connection or a laptop sleep. A workstation pane is only a view onto its remote pane. It runs `workmux remote attach` and reattaches on its own.
- **Status.** Agent hooks in the sandbox write each agent's status to a file. Remote workmux sends those files over the control channel, so the sidebar updates live. Changes made while you were disconnected arrive on reconnect, with their original times.

## Set up a project

Commit the sandbox settings in the repository's `.workmux.yaml`:

```yaml
sandbox:
  enabled: true
  backend: openshell
  openshell:
    image: ghcr.io/me/api-dev:1
```

Then run workmux in your clone of the repository:

```bash
cd ~/src/api
workmux add feature
```

```
Creating remote api on first use…
Cloning https://github.com/me/api.git into ~/api on api
```

The first command that needs the sandbox creates it, clones the repository from your clone's `origin` and provisions it. See [OpenShell projects](/guide/remote-projects/openshell/) for the settings, credentials and lifecycle.

## Which project a command acts on

A command acts on the project it runs in; there is no flag naming another project. Commands that act on a project (`add`, `open`, `close`, `remove`, `merge`, `list`, `path`, `run`, `rename`, `rebase`, `set-base`, `sync-files` and `init`, and plain worktree names given to `send`, `capture` and `wait`) find it in this order:

1. the project of the tmux window, or else the session, you typed the command in. workmux tags each remote project's windows (and, in session mode, its sessions) with the path of your clone, and reads the clone's config again for every command typed there, so a config change applies at once. If the config no longer puts the project in a sandbox, the window is no longer the project's; if the clone is gone, commands typed there fail and say so.
2. the repository of the current directory: its config decides whether the project is on the workstation or in a sandbox

Outside any repository and any project window, they fail and tell you to run them inside a clone of the repository. A command relayed to a sandbox says so once (`Running in OpenShell sandbox wm-api (project api)`), and `list` names the sandbox in its first line.

Run from a remote worktree's window, commands with no worktree name act on that worktree, as they do on the workstation.

A remote project is named after its repository: the directory name of your clone. Its sandbox is named after the clone's path (see [OpenShell projects](/guide/remote-projects/openshell/)), so two clones get two sandboxes, even of the same repository. workmux stores nothing about a project. In the sidebar, dashboard and `workmux status` a remote project shows as `name@remote`, for example `api@api-1a2b3c4d`, so it never merges with a project of the same name on the workstation. Agent selectors accept `feature`, `api:feature` and `api@api-1a2b3c4d:feature`.

`resurrect` and `reap-agents` run on the workstation and then on every remote project with an open window on the workstation, and on the project of the clone you run them in (after the tmux server restarts no window is left, so run `resurrect` in the clone). Each project's output appears under a `<project> (on <remote>):` heading.

## Where the windows go

A remote project's windows go where a local project's do. In window mode, the default, a new window opens in the session you typed the command in, after the current window, and takes focus. `mode`, `window_placement` and `default_session` work as for any project: see [Configuration](/guide/configuration/#basic-options) and [session mode](/guide/session-mode/).

workmux reads these three settings from your global config on the workstation and from the `.workmux.yaml` in the sandbox's clone; the project's value wins, as for any other setting. Everything else comes from the `.workmux.yaml` in the sandbox's clone.

## Commands typed in a remote pane

A remote pane's shell can run workmux too, so an agent in the sandbox can coordinate other agents:

- `add`, `open`, `close`, `remove`, `merge` and `rename` run in the sandbox. Their windows appear on your workstation.
- `send`, `capture` and `wait` run on your workstation, limited to agents of projects on that remote. `send` takes only a name and text; `capture` only `--lines`; `wait` only `--status`, `--timeout` and `--any`.

While the workstation daemon is not connected to the remote, these commands fail with `Workstation not connected` before they do anything.

## Shims and host commands

A remote can run a few commands on your workstation, through the control channel:

- **Built-in shims**: sound playback (so hook sounds play on your workstation) and clipboard read (so image paste into a remote agent works).
- **Host commands**: any other command must be listed in `sandbox.host_commands` in your global config (a repository's `.workmux.yaml` cannot set it). It runs in workmux's host-exec sandbox on your workstation, in a working directory of its own. Everything else is refused.

A remote can act only on the tmux windows and panes of its own projects. Requests for any other window, pane or session are refused.

## When a remote disconnects

Agents keep running in the sandbox. On your workstation:

- **Panes** show `reconnecting to <remote>… (r: retry now, q: close)` with the reason, and retry on their own, backing off from 1 s to 30 s. They redraw when they reattach. `q` closes the workstation pane but leaves its remote pane running. A pane closes by itself only when its remote pane has really ended.
- **Sidebar and dashboard** keep the remote's agents as **stale**: dimmed, with their last known status, and never flagged as interrupted. They refresh when the remote reconnects.
- **`workmux status`** shows a stale agent as, for example, `done (stale)`. `wait` does not count a stale agent's status. It prints that the remote is disconnected and keeps waiting until the remote reconnects or `--timeout` expires. `send` to a stale agent fails.
- **Dashboard** still shows PR and check status for remote branches. workmux queries GitHub from your workstation with the project's git URL, so this works while a sandbox is stopped or unreachable.

To get panes back after their windows were closed outside workmux, or after tmux itself died, run `workmux open NAME` in the repository or `workmux resurrect`. Each pane of the layout attaches to a remote pane that is still running, in the order they started, instead of starting its command again. `workmux close` and `remove` end the worktree's remote panes.

The control channel stays open while any workstation pane shows one of that remote's panes, and for a minute after the last command that used it. It reconnects with backoff after a drop.

## Checking a remote

```bash
workmux remote list      # each remote and whether it is reachable
workmux remote status    # versions, warnings, and the error when unreachable
```

Run in a repository whose config puts it in a sandbox, they include that sandbox even before any other command has run there. An unreachable remote is reported in the transport's own words. `remote status` warns when:

- dtach is missing in the sandbox
- an agent in the sandbox has no status hooks (its agents show no status in the sidebar; run `workmux setup --hooks` there)
- the sandbox runs a different workmux release with the same protocol version (this works, with a warning). A different protocol version is refused, and the error names the version to install.

## Limits

- Commands that would open your editor refuse for a remote project: `add --prompt-editor`, `add --auto-name` without `-p` or `-P`, `open --prompt-editor`, and a `merge` that needs a commit message (staged changes or a squash). Pass the prompt with `-p TEXT`. A prompt file given with `-P FILE` is read on your workstation (up to 1 MiB).
- Command output from a relayed `send`, `capture` or `wait` typed in a remote pane arrives when the command exits, not streamed.
- Files are never synced between your workstation and a sandbox; code moves through git. An existing project's worktrees on the workstation can't be moved into a sandbox.
- Only tmux is supported on the workstation for remote projects.
