---
title: "remote"
description: Register, check and manage the remotes that remote projects live on
---

Commands for the remotes that [remote projects](/guide/remote-projects/) live on. Remotes are stored in your global config.

## remote add

Register a remote.

```bash
workmux remote add <name> --transport <ssh|openshell> --target <target> [options]
```

**Options:**

- `--transport <ssh|openshell>`: How to reach the remote.
- `--target <target>`: An SSH host as in your SSH config, or an OpenShell sandbox name.
- `--no-builtin-shims`: Disable the built-in shims (sound playback, clipboard read) for this remote.
- `--host-command <command>`: Allow the remote to run this command on the workstation (repeatable).

An OpenShell project needs no `remote add`: [`project add --openshell`](/reference/commands/project/) gives it a remote of its own.

## remote list

List remotes and whether each is reachable. Alias: `ls`.

```bash
workmux remote list
```

## remote status

Show whether a remote is reachable, with its workmux and protocol versions, warnings, and the transport's error when it is unreachable. For an OpenShell project, it also shows the sandbox's effective policy.

```bash
workmux remote status [name]
```

Checks every remote when no name is given. Exits 1 if any remote it checked is unreachable. A stopped sandbox is reported as `stopped` and is not started.

`remote status`, `remote list` and `remote start` offer to [install a matching workmux](/guide/remote-projects/#installing-workmux-on-an-ssh-remote) on an SSH remote that lacks one.

## remote upload

Install a workmux matching this one on an SSH remote, in its cache directory (`~/.cache/workmux/bin/<version>/`), and use it from then on (`upload_workmux: true`).

```bash
workmux remote upload <name> [--binary <path>]
```

**Options:**

- `--binary <path>`: A workmux built for the remote's platform. Default: this build when the platforms match, else the release of this version for the remote's platform.

## remote start

Start a remote's stopped sandbox, creating it if it does not exist yet.

```bash
workmux remote start <name>
```

## remote stop

Stop a remote's sandbox. The workspace is kept, but every process in it ends, agents included.

```bash
workmux remote stop <name>
```

## remote recreate

Delete and recreate an OpenShell project's sandbox, carrying its committed work across. Refused while any worktree has uncommitted changes. See [Recreating a sandbox](/guide/remote-projects/openshell/#recreating-a-sandbox).

```bash
workmux remote recreate <name> [-y]
```

**Options:**

- `-y, --yes`: Don't ask for confirmation.

## remote rm

Remove a remote from your global config. Refused while a project uses the remote; unregister those with [`project rm`](/reference/commands/project/#project-rm) first.

For an OpenShell project's remote, this deletes the sandbox and the project. It first lists work that is unmerged or exists only in the sandbox and asks for confirmation, then closes the project's workstation windows. See [Deleting a sandbox](/guide/remote-projects/openshell/#deleting-a-sandbox).

```bash
workmux remote rm <name> [-y]
```

**Options:**

- `-y, --yes`: Don't ask for confirmation.

## remote attach

Show a remote pane in this terminal, reattaching after drops. Workstation panes of remote worktrees run this; you rarely run it yourself.

```bash
workmux remote attach <remote> <project>/<worktree>/<slot>
```

When the connection drops it shows `reconnecting to <remote>…`. Press `r` to retry at once or `q` to close the pane; the remote pane keeps running. It exits when the remote pane ends.
