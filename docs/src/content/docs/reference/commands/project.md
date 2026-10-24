---
title: "project"
description: Register and unregister projects on a remote or in their own OpenShell sandbox
---

## project add

Register a [remote project](/guide/remote-projects/) in your global config.

```bash
# On a registered remote
workmux project add <name> --remote <remote> --repo <url> [options]

# In its own OpenShell sandbox
workmux project add <name> --openshell --repo <url> [options]
```

The repository is cloned on the remote by the first command that needs it.

**Options:**

- `--remote <remote>`: Remote the project lives on. It must already be registered with [`remote add`](/reference/commands/remote/).
- `--openshell`: Give the project its own OpenShell sandbox as its remote, created on first use.
- `--repo <url>`: Git URL the remote clones the project from.
- `--path <path>`: Path of the clone on the remote. Default: `~/<name>`. `~` and relative paths resolve against the remote's home.
- `--session <name>`: Workstation session for the project's windows. Default: the project name.
- `--placement <project-session|current-session>`: Where the project's windows go in window mode. Default: `project-session`.

**OpenShell options** (with `--openshell`):

- `--image <image>`: Image the sandbox is built from; it must contain git and dtach.
- `--policy <file>`: Policy file merged on top of workmux's base policy.
- `--provider <provider>`: OpenShell provider to attach, for credentials (repeatable).
- `--sandbox <name>`: Sandbox name. Default: derived from the project name.

See [OpenShell projects](/guide/remote-projects/openshell/).

## project rm

Unregister a project from your global config, keeping the rest of the file, comments included. After confirmation, it closes the project's workstation windows and untags a session it adopted. Nothing on the remote is touched: the remote stays registered and the clone stays where it is.

```bash
workmux project rm <name> [-y]
```

An OpenShell project is refused: its sandbox is its own remote, so remove both with [`workmux remote rm <name>`](/reference/commands/remote/#remote-rm).

**Options:**

- `-y, --yes`: Don't ask for confirmation.

## Examples

```bash
workmux remote add devbox --transport ssh --target devbox
workmux project add api --remote devbox --repo git@github.com:me/api.git

workmux project add web --openshell --repo https://github.com/me/web.git --provider github

workmux project rm api
```
