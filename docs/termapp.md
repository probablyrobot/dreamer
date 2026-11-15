# Pane apps (`dreamer-termapp`)

`dreamer-termapp` runs an app package: a manifest that declares where its
data comes from, how to show it, and which keys run which actions. The app
is the tmux pane's real program, so it looks and works the same in the
desktop and through any plain `tmux attach`.

The app's model (the manifest, its data and selection, a form's values,
commands, the Luau module, and reloading) is the headless `app-model`
crate. `dreamer-termapp` only turns keys into input for it and draws it
with Ratatui, so other presentations can draw the same model.

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
  row (a log follows its newest row); `text` shows one value; `gauge`
  shows one numeric `field` (0 to 1, or a percentage) as a bar with an
  optional `label` field; `chart` draws a numeric `field` across the rows
  as a sparkline; `form` is described below. The status line shows the
  connection, the keys, and the last outcome.
- **Commands** call a `method`, or a module `handler`, when their key is
  pressed. Their `params` may use `${selected.<field>}` and, in a form,
  `${form.<field>}`: a string that is exactly one placeholder
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

## Modules

A package may name a Luau module beside its manifest (`module =
"app.luau"`). It runs in the same sandbox as daemon scripts
([scripts.md](scripts.md)): no files, processes, or network, with budgets,
and its `dreamer.*` calls go over the app's own connection with the app's
grant. It may define:

- `transform(rows)`: called with the fetched rows, returns the rows to
  show; use it for computed columns or to filter rows.
- Command handlers: a command with `handler = "name"` calls the module's
  function `name` with `{ selected = <row>, form = <values> }`; a string it
  returns is shown in the status line.

The module's top level runs when the package loads. Keep it to defining
functions: it may run while the app is not yet connected.

## Forms

A `form` view lists typed fields and names the command Enter runs:

```toml
[view]
type = "form"
source = "none"              # any data source; forms show no rows
submit = "send"
fields = [
  { name = "name", label = "Name", kind = "text", required = true },
  { name = "count", kind = "number" },
  { name = "loud", kind = "toggle" },
]
```

Tab and the arrows move between fields; letters and digits type, so `q`
is a letter here (Esc or Ctrl-C quits); Space flips a toggle; Enter
submits. The values reach the command as `${form.<name>}` or, for a
handler, as `form`, typed: text as strings, numbers as numbers, toggles
as booleans. A required field left empty, or a number field that is not a
number, stops the submit and says so.

## Reloading

The runtime looks at the manifest and the module twice a second. When
either changed, it validates the manifest, compiles the module, and runs
its top level. Only when all of that succeeds does it swap to the new
version: the selection stays on its row, form values stay by field name,
and the title says how many reloads there were. A version that fails at
any step is not applied; the running one keeps going and the status line
says why.

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
