# dream

`dream` is Dreamer's command-line interface. It works without a GUI: every
command that touches tmux goes through `dreamerd`'s typed API, and
`dreamerd` is started on demand.

## tmux through the daemon

These commands start `dreamerd` if it is not running. They take the tmux
options a newly started daemon should use: `-L name` or
`--tmux-socket-path PATH` for the server, and `--tmux PATH` for the
binary. If a running daemon drives a different tmux server, they refuse
and suggest `dream shutdown`.

| Command | Effect |
| --- | --- |
| `dream ls [--json]` | Sessions (`$N`), windows (`@N`), and panes (`%N`), with sizes, programs, process IDs, and pane generations. `*` marks the active window or pane |
| `dream act <action> … [--key K] [--json]` | Run one typed action and print its observed outcome |
| `dream actions [ID] [--limit N] [--json]` | The action ledger, newest first, or one action |

Actions:

```text
dream act split  %PANE [-h|-v] [--percent N | --cells N] [--cwd DIR] [-- PROGRAM ARGS...]
dream act resize %PANE [--width N] [--height N]
dream act select %PANE | @WINDOW
dream act zoom   %PANE
dream act rename @WINDOW | $SESSION NAME
dream act send   %PANE TEXT [--enter] [--generation G]
dream act keys   %PANE KEY... [--generation G]
dream act kill   %PANE | @WINDOW
```

- `split -h` places the new pane beside the target; `-v`, the default,
  places it below.
- A program after `--` runs as an exact argument vector, not through a
  shell.
- `send` delivers the text byte for byte. `--enter` appends a carriage
  return.
- `keys` takes tmux key names (`Enter`, `C-c`, `Up`).
- `--generation G` refuses the input if the pane's process has been replaced
  since `dream ls` showed generation `G`.
- `--key K` makes the action idempotent: repeating it with the same key
  returns the first result instead of running again.

The exit status is 0 when the action completed, 1 when it failed or was
refused (the output says why), and 3 when the outcome is unknown. See
[actions.md](actions.md).

## Daemon

| Command | Effect |
| --- | --- |
| `dream ensure-running [tmux options]` | Start `dreamerd` unless it is running |
| `dream status [--json]` | Process, socket, log, clients, version, and tmux endpoint state |
| `dream version` | Client and daemon versions and build family |
| `dream shutdown` | Stop `dreamerd`. tmux keeps running |
| `dream doctor` | Diagnostics: directories and their permissions, socket, startup lock, ledger summary, daemon, and tmux link. Never starts the daemon |

See [daemon.md](daemon.md) for files, directories, and lifecycle.

## Workspaces

`dream config-dir`, `dream workspace list | check | up` work on workspace
files directly; see [workspaces.md](workspaces.md).
