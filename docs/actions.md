# Actions

An action is one typed change to tmux: split a pane, send input, rename a
window. Clients submit actions with `actions.submit` and never send tmux
command strings. Every action is recorded in the action ledger and settles
with an outcome observed in tmux. You need this page if you are writing a
client, a script, or an extension that changes tmux. From a shell,
`dream act` runs the common ones ([cli.md](cli.md#panes-and-actions)).

| Action | Effect | Reported when completed |
| --- | --- | --- |
| `split_pane` | Split a pane horizontally or vertically. Size in percent or cells; an optional exact argv and working directory | The new pane's id, size, and window |
| `resize_pane` | Set width, height, or both | The size tmux actually gave the pane (tmux may clamp) |
| `select_pane`, `select_window` | Make active | That the target is active |
| `zoom_pane` | Toggle zoom | Whether the window is now zoomed |
| `rename_window`, `rename_session` | Rename. Names are stored literally, `#` included | The name tmux now reports |
| `send_input` | Exact bytes to the program in the pane | That the bytes were delivered. What the program does with them is not observed |
| `send_keys` | tmux key names such as `Enter` or `C-c` | That the keys were delivered |
| `kill_pane`, `kill_window` | Close | That the target is gone |
| `new_window` | Add a window at the end of a session. It runs the default shell, or an exact `argv` | The new window and its pane |
| `new_session` | Start a detached session with one window. It runs the default shell, or an exact `argv`, in an optional working directory. tmux names it unless `name` is given; a name already taken is refused | The new session and its first pane (`new_pane`) |
| `move_pane` | Move a pane, with its program, into an existing `window`, or into a new window named `new_window` in its session. The pane keeps its id; a window left empty closes | Which window the pane is in afterwards, and its name |

## Lifecycle

`accepted` → `prepared` → `dispatching` → `completed`, `failed`, or
`outcome_unknown`.

- **Before dispatch:** the target is checked against tmux's current state.
  A missing pane, window, or session, or an invalid argument, ends in
  `failed`, and nothing is sent.
- **`send_input` and `send_keys` with a `generation`:** the action is
  refused if the pane's process was replaced since that generation was
  read. This means typed input never reaches a respawned program by
  accident.
- **`dispatching`** is written and synced to disk before the command reaches
  tmux.
- **After dispatch:** Dreamer reads tmux back. A rejected command is
  `failed`, with tmux's message. A timeout, a lost connection, or an effect
  that cannot be seen is `outcome_unknown`.
- **`outcome_unknown` is never retried automatically,** and neither are
  destructive actions or input.

## Restarts and crashes

The ledger is an append-only file of JSON lines, one per transition. When
the daemon starts, it settles anything a previous process left unfinished:

- **An action that never reached `dispatching`** becomes `failed`, with the
  reason "nothing was sent".
- **An action in `dispatching`** becomes `outcome_unknown`. The command may
  have run, and it is not sent again.

A torn final line from a crash during a write is ignored. Corruption
anywhere else stops the daemon from starting rather than losing history.

## Idempotency

`idempotency_key` makes a submission safe to repeat, even after a daemon
restart:

- **Same key, same action:** returns the first record, and nothing runs
  again.
- **Same key, different action:** refused with code `-32003`.

## Ordering

The daemon runs one action at a time per tmux endpoint, so conflicting
actions never interleave.
