# Pane apps (`dreamer-termapp`)

`dreamer-termapp` runs an app package: a manifest that declares where its
data comes from, how to show it, and which keys run which actions. The app
is the tmux pane's real program, so it looks and works the same in the
desktop and through any plain `tmux attach`.

```sh
dreamer-termapp app.toml            # run it in this terminal
dreamer-termapp --check app.toml    # validate only
dream app run panes --method tmux.topology --method actions.submit --all-panes \
    -- dreamer-termapp app.toml      # run it in a new window with a narrow grant
```

## The manifest

```toml
schema_version = 1
id = "local.panes"          # dotted lowercase
version = "0.1.0"
title = "Panes"
capabilities = ["tmux.topology", "actions.submit"]

[data.panes]
method = "tmux.topology"    # a query, never a mutation
path = "panes"              # where the rows are in the result
events = ["tmux.topology"]  # fetch again on these events; or refresh_ms = 2000
limit = 500

[view]
type = "table"              # table, list, log, or text
source = "panes"
row_key = "id"              # keeps the selection on its row across updates
columns = ["id", "window", "current_command"]

[commands.rename]
key = "r"
label = "rename"
method = "actions.submit"
params = { action = { kind = { rename_window = { window = "${selected.window}", name = "w-${selected.id}" } } } }
```

- **Data sources** call one API method each, with fixed `params`, and take
  the rows from `path`. They are fetched when the app connects, on
  `refresh_ms` (at least 200), and when an event of the listed kinds is
  recorded ([events.md](events.md)).
- **Views:** `table` shows `columns`; `list` and `log` show one `field` per
  row (a log follows its newest row); `text` shows one value. The status
  line shows the connection, the keys, and the last outcome.
- **Commands** call a method when their key is pressed. Their `params` may
  use `${selected.<field>}`: a string that is exactly one placeholder
  becomes the selected row's value with its type, and a placeholder inside
  a longer string becomes its text. A missing field stops the command; it
  is never replaced by nothing. There is no other expression language, no
  shell, and no `eval`.
- **Keys:** Up/Down (or k/j) and PageUp/PageDown move the selection, and `q`
  or Ctrl-C quits. Commands may use other letters and digits, Enter, Esc,
  Tab, Backspace, Delete, Left, Right, Home, End, F1 to F12, and Ctrl-a to
  Ctrl-z.

Validation is strict: unknown keys, unknown methods, mutations as data
sources, malformed placeholders, and duplicate or reserved keys are errors,
each reported with where it is.

## Connection

The app connects to its host's daemon. Started by `dream app run`, it
first registers with its single-use launch ticket and then holds that
narrow grant ([grants.md](grants.md)). A refused call shows in the status
line.

When the daemon goes away, the app keeps showing its last data, marked
`stale` with the reason, and tries again with growing delays. Commands
pressed meanwhile are not sent; nothing is queued to run later. When the
daemon is back it shows `live` and fetches again.

An app that registered with a ticket cannot register again after its
daemon restarted (the ticket is used up), and it does not reconnect with
more authority than it was granted. It stays stale and says to relaunch it.
