---
title: "project"
description: Register and unregister projects on a remote or in their own OpenShell sandbox
---

## project add

Register a [remote project](/guide/remote-projects/) in your global config. Each kind of project has its own subcommand, which takes only its own options.

```bash
# On a registered remote
workmux project add remote <name> --remote <remote> --repo <url> [options]

# In its own OpenShell sandbox
workmux project add openshell <name> --repo <url> [options] [OpenShell options]
```

The repository is cloned on the remote by the first command that needs it.

**Project options** (both subcommands):

- `--repo <url>`: Git URL the remote clones the project from. For an OpenShell project, an SSH URL on github.com is stored as its HTTPS form, because [OpenShell sandboxes don't support SSH](/guide/remote-projects/openshell/#ssh-is-not-supported-in-openshell-sandboxes); an SSH URL on another host is refused.
- `--path <path>`: Path of the clone on the remote. Default: `~/<name>`. `~` and relative paths resolve against the remote's home.
- `--session <name>`: Workstation session for the project's windows. Default: the project name.
- `--placement <project-session|current-session>`: Where the project's windows go in window mode. Default: `project-session`.

**`project add remote`:**

- `--remote <remote>`: Remote the project lives on. It must already be registered with [`remote add`](/reference/commands/remote/).

**OpenShell options** (`project add openshell` only; the sandbox is the project's remote, created on first use):

- `--image <image>`: Image the sandbox is built from; it must contain git and dtach.
- `--policy <file>`: Policy file merged on top of workmux's base policy.
- `--provider <provider>`: OpenShell provider to attach, for credentials (repeatable).
- `--github host`: Give the sandbox your workstation's `gh` login, kept in a gateway provider workmux manages. Fails when `gh` is missing or logged out, or beside a `--provider` that already gives GitHub credentials. See [Your gh login](/guide/remote-projects/openshell/#your-gh-login-github-host).
- `--agent <agent>`: Agent the sandbox's image installs, such as `claude` or `codex` (repeatable). The first is the sandbox's default agent. Default: your default agent's type. Prints the provider type each agent needs, and warns when no attached provider has it.
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
workmux project add remote api --remote devbox --repo git@github.com:me/api.git

workmux project add openshell web --repo https://github.com/me/web.git --provider github

workmux project rm api
```
