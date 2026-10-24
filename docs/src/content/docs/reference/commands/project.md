---
title: "project"
description: Register a project on a remote or in its own OpenShell sandbox
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
- `--agent <agent>`: Agent the sandbox's image installs, such as `claude` or `codex` (repeatable). The first is the sandbox's default agent. Default: your default agent's type. Prints the provider type each agent needs, and warns when no attached provider has it.
- `--sandbox <name>`: Sandbox name. Default: derived from the project name.

See [OpenShell projects](/guide/remote-projects/openshell/).

## Examples

```bash
workmux remote add devbox --transport ssh --target devbox
workmux project add api --remote devbox --repo git@github.com:me/api.git

workmux project add web --openshell --repo https://github.com/me/web.git --provider github
```
