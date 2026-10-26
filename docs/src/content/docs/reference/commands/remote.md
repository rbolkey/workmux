---
title: "remote"
description: Check and manage the sandboxes that remote projects live in
---

Commands for the remotes that [remote projects](/guide/remote-projects/) live on. A remote is an OpenShell sandbox, which a repository's config asks for with `sandbox.backend: openshell`; there is nothing to register. Run in such a repository, these commands know its sandbox before any other command has run there.

## remote list

List remotes and whether each is reachable. Alias: `ls`.

```bash
workmux remote list
```

## remote status

Show whether a remote is reachable, with its workmux and protocol versions, warnings, and the transport's error when it is unreachable. For an OpenShell project, it also shows the sandbox's effective policy and its agents, each with the provider type that gives it credentials, and warns when no attached provider has that type.

```bash
workmux remote status [name]
```

Checks every remote when no name is given. Exits 1 if any remote it checked is unreachable. A stopped sandbox is reported as `stopped` and is not started.

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

Delete an OpenShell project's sandbox. It first lists work that is unmerged or exists only in the sandbox and asks for confirmation, then closes the project's workstation windows. The next workmux command in the repository creates a new sandbox. See [Deleting a sandbox](/guide/remote-projects/openshell/#deleting-a-sandbox).

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
