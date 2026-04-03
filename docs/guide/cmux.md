---
description: Use cmux as an alternative multiplexer backend (macOS)
---

# cmux

::: warning Experimental
The cmux backend is new and experimental. Expect rough edges and potential issues.
:::

[cmux](https://www.cmux.dev/) can be used as an alternative to tmux on macOS. Detected automatically via `$CMUX_SOCKET_PATH`.

## Differences from tmux

| Feature              | tmux                 | cmux                    |
| -------------------- | -------------------- | ----------------------- |
| Agent status in tabs | Yes (window names)   | Yes (sidebar status)    |
| Tab ordering         | Insert after current | Appends to end          |
| Scope                | tmux session         | cmux window (OS window) |
| Session mode         | Yes                  | No (window only)        |
| Pane size control    | Percentage-based     | 50/50 splits only       |
| Dashboard preview    | Yes                  | Yes                     |

- **Flat workspace model**: cmux has no session hierarchy. Each worktree gets its own workspace (tab). `workmux add --session` is not supported — use window mode instead.
- **Tab ordering**: New workspaces appear at the end of the tab bar (no "insert after" support like tmux).
- **Sidebar status**: cmux shows agent status icons and colors in its native sidebar, not in window names like tmux.
- **Pane splits**: All splits are 50/50 — `resize-pane` is not supported by cmux.
- **Per-process identification**: `cmux identify` returns the workspace of the calling process, not the globally selected tab. This ensures `workmux close` targets the correct workspace.

## Requirements

- macOS (cmux is built on libghostty and is macOS-only)
- cmux >= 0.63.1 (for `new-workspace --name` and `--cwd` flags)
- Must be running inside a cmux terminal

## Setup

No configuration required. workmux auto-detects cmux via `$CMUX_SOCKET_PATH` when running inside a cmux terminal.

To explicitly select the cmux backend:

```bash
export WORKMUX_BACKEND=cmux
```

### tmux inside cmux

If you run tmux inside cmux, workmux will use the tmux backend (tmux detection takes priority). This is the correct behavior — workmux manages panes in the innermost multiplexer.

## Known limitations

- **No session mode**: Use window mode. `workmux add --session` returns an error.
- **No pane resize**: cmux's `resize-pane` returns `not_supported`. Splits default to 50/50.
- **No PID tracking**: cmux's `surface-health` does not expose process PIDs. Agent liveness is validated via surface existence checks only.
- **`send-key ctrl+c` unreliable**: The cmux backend uses `send` with `\x03` escape sequence for interrupt signals.
- **Load degradation**: cmux performance degrades with ~100+ workspaces. Run integration tests in smaller batches.
