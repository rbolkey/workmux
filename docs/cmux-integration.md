# cmux Integration for workmux

## Overview

cmux is a macOS terminal multiplexer built on libghostty (Ghostty's terminal emulation engine). workmux supports cmux as its fifth multiplexer backend, alongside tmux, WezTerm, Zellij, and Kitty.

cmux uses a flat workspace model (no session hierarchy), similar to Zellij. Each worktree maps to a cmux workspace with sidebar status integration and direct click-to-navigate support.

## Backend Detection

workmux auto-detects cmux via `$CMUX_SOCKET_PATH`. Detection priority:

1. `$WORKMUX_BACKEND` env var (explicit override)
2. `$TMUX` → tmux (wins even inside cmux, for tmux-inside-cmux scenarios)
3. `$CMUX_SOCKET_PATH` → cmux
4. `$WEZTERM_PANE` → WezTerm
5. `$ZELLIJ` → Zellij
6. `$KITTY_WINDOW_ID` → Kitty
7. None → tmux (default)

## Concept Mapping

| workmux | tmux | cmux | notes |
|---|---|---|---|
| Window/Tab | window | workspace | tabs in the tab bar |
| Pane | pane | surface (within a pane) | `new-split` creates surfaces |
| Pane ID | tmux pane ID | `surface:N` ref | stable within cmux session |
| Window ID | tmux window ID | `workspace:N` ref | stable within cmux session |
| Detection env | `$TMUX` | `$CMUX_SOCKET_PATH` | |
| IPC | CLI (`tmux` binary) | CLI (`cmux` binary) over Unix socket | |

Sessions are not supported — cmux has a flat workspace model. `workmux add --session` returns an error on cmux.

## Implementation

### Source files

| File | Purpose |
|---|---|
| `src/multiplexer/cmux.rs` | Full `Multiplexer` trait implementation (~1,150 lines) |
| `src/multiplexer/handshake.rs` | `CmuxHandshake` using `wait-for` latch semantics |
| `src/multiplexer/mod.rs` | `DetectedBackends` struct, cmux detection + registration |
| `src/multiplexer/types.rs` | `BackendType::Cmux` variant |

### Key design decisions

1. **Flat workspace model** follows Zellij pattern: `should_exit_on_jump()=false`, session methods return errors
2. **`CmuxHandshake`** uses `wait-for` latch semantics (signal persists until consumed — simpler than tmux's lock/unlock)
3. **`cmux identify`** for per-process workspace identification (not the globally "selected" tab)
4. **Direct close** instead of deferred nohup scripts: cmux handles `close-workspace` asynchronously server-side
5. **Surface-to-workspace mapping** via `Mutex<HashMap>` with fallback discovery query
6. **JSON everywhere** — all listing commands use `--json` + serde (no fragile output parsing)

### Trait method mapping

| Multiplexer trait method | cmux CLI command |
|---|---|
| `is_running()` | `cmux ping` |
| `current_pane_id()` | `cmux identify` (caller's surface ref) |
| `create_window()` | `cmux new-workspace --name <name> --cwd <path>` |
| `select_window()` | `cmux select-workspace --workspace <ref>` |
| `window_exists()` | `cmux --json list-workspaces` + filter |
| `kill_window()` | `cmux close-workspace --workspace <ref>` |
| `split_pane()` | `cmux new-split <direction> --workspace --surface` |
| `respawn_pane()` | `cmux respawn-pane --workspace --surface --command` |
| `send_keys()` | `cmux send --workspace --surface "text\n"` |
| `send_key()` | `cmux send-key --workspace --surface <key>` |
| `capture_pane()` | `cmux read-screen --workspace --surface --lines N` |
| `set_status()` | `cmux set-status <key> <text> --icon --color --workspace` |
| `clear_status()` | `cmux clear-status <key> --workspace` |
| `paste_multiline()` | `cmux set-buffer` + `cmux paste-buffer` |
| `create_handshake()` | `CmuxHandshake` using `cmux wait-for` |
| `get_live_pane_info()` | `cmux --json surface-health` + `sidebar-state` |
| `validate_agent_alive()` | Surface existence check via `surface-health` |

### Known upstream limitations

| Limitation | Workaround |
|---|---|
| No PID in `surface-health` | Surface existence check only for liveness validation |
| No `resize-pane` support | Use cmux defaults; log warning once |
| `send-key ctrl+c` unreliable | Use `send` with `\x03` escape sequence |
| No `--cwd` in `respawn-pane` | Wrap command with `cd <path> && <cmd>` |
| cmux load degrades after ~100 workspaces | Run integration tests in smaller batches |

## Requirements

- macOS (cmux is macOS-only)
- cmux >= 0.63.1 (for `new-workspace --name` and `--cwd` flags)
- Must be running inside a cmux terminal (`$CMUX_SOCKET_PATH` set)

## Testing

### Learning tests

36+ learning tests in `tests/learning/cmux/` document cmux CLI behavior and verify assumptions:

- `test_cmux_workspace_lifecycle.py` — creation, listing, renaming, `--name`/`--cwd` flags
- `test_cmux_prerequisite.py` — `select-workspace`, `send-key`, `read-screen --lines`, `respawn-pane`
- `test_cmux_identify_semantics.py` — per-process `identify` vs global selection
- `test_cmux_self_close.py` — workspace self-close and HOME independence
- `test_cmux_find_window.py` — substring matching semantics

### Integration tests

Run integration tests with the `--backend=cmux` flag:

```bash
# Must be inside a cmux terminal (CMUX_SOCKET_PATH set)
pytest tests/ --backend=cmux -v

# Run specific suites to avoid load degradation
pytest tests/test_workmux_close.py --backend=cmux -v
pytest tests/test_workmux_merge.py --backend=cmux -v
```

CI skips cmux tests automatically when `$CMUX_SOCKET_PATH` is not set.

## References

- cmux docs: https://www.cmux.dev/docs/getting-started
- workmux multiplexer trait: `src/multiplexer/mod.rs`
- Existing backends: `src/multiplexer/{tmux,wezterm,zellij,kitty}.rs`
