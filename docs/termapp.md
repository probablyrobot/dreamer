# Pane apps (`dreamer-termapp`)

`dreamer-termapp` runs an app package: a manifest that declares where its
data comes from, how to show it, and which keys run which actions. The app
is the tmux pane's real program, so it looks and works the same in the
desktop and through any plain `tmux attach`.

The app's model (the manifest, its data and selection, a form's values,
commands, the Luau module, and reloading) is the headless `app-model`
crate. `dreamer-termapp` only turns keys into input for it and draws it
with Ratatui. The desktop draws the same model as a native panel; see
[desktop.md](desktop.md#app-panels).

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

## Agentic panes

An agentic pane is an app package bound to an agent provider and a grant
(spec 13.4). It is a pane app like any other, so it keeps running
without the desktop and works through a plain `tmux attach`.

```toml
schema_version = 1
id = "local.inspector"
version = "0.1.0"
title = "Inspector"
capabilities = ["tmux.topology", "terminal.query"]

[view]
type = "agent"
# source = "runs"                 # a monitor shows the data source it watches

[agent]
kind = "inspector"                # inspector, monitor, or operator
provider = "claude"               # claude or codex; default: the first ready CLI
instructions = "Reply in one short line."
tools = { methods = ["tmux.topology", "terminal.query"], all_panes = true }   # or panes = [3, 4]
max_turns = 4
timeout_s = 300                   # a run is stopped after this (default 300)
```

- **Use:** type a prompt and press Enter. Esc cancels a run in progress,
  and Ctrl-c quits. `q` is just a letter here.
- **Header:** the kind, the provider (and model), the activity, and the
  grant.
- **Transcript:** what happened, newest at the bottom: the prompt, the
  model's text, every tool call with its input, each result, usage, and
  any failure with its reason.
- **Tools:** the model reaches Dreamer only through
  [`dream mcp`](mcp.md), narrowed to `[agent.tools]`: those methods and
  those panes. Every method there must be among the package's
  `capabilities`. With no methods the model gets no tools at all. Claude
  Code also gets none of its own tools (see [providers.md](providers.md)).
- **Bounded context:** context is retrieved by those tool calls, bounded
  by the tools themselves. Nothing streams other terminals to a model.
- **Read-only kinds:** an inspector or monitor may not list a method that
  changes things, nor `terminal.input`. Such a manifest does not validate.
  An operator may list actions.
- **No model:** with no provider ready, the header says why.
  - An inspector then answers in a scripted mode: the prompt is searched
    as plain text in the newest lines of the panes it may read
    (`terminal.query`), and the matching lines are listed.
  - A monitor keeps showing its data source, which needs no model.
  - An operator says that it needs a provider.

Operator panes run actions only through the typed API and the narrowed
MCP grant. Plans, leases, and asking for more scope are not built yet.

## Contributions

A package installed in `<config dir>/apps/<name>/app.toml` (see
`dream config-dir`) can add to the desktop. Its `[contributes]` block
declares what. The desktop decides where each item goes and how it is
drawn, and the package never draws it.

```toml
[contributes]
version = 1                       # the contribution schema; 1 is the default

[[contributes.commands]]          # a palette command
id = "rename-first"               # lowercase letters, digits, -; named <package id>.<id>
title = "Rename the first window"
run = "rename"                    # one of this package's [commands], with a method and fixed params
key = "ctrl-alt-r"                # optional; needs cmd, ctrl, or alt

[[contributes.commands]]
id = "show"
title = "Show windows"
opens = "panel"                   # open this package as a panel instead

[[contributes.sidebar]]           # an entry in the sidebar's Apps section; opens the package as a panel
id = "windows"
title = "Windows"

[[contributes.status]]            # a compact item in the tab strip, read every 2 s
id = "count"
label = "windows"                 # shown before the value
source = "windows"                # one of this package's data sources
field = "name"                    # the first row's field; without one, the number of rows
```

- **Validation:** contributions are validated with the rest of the
  manifest. A contributed command runs one of the package's own commands.
  That command must call a method, and its parameters must have no
  `${selected...}` or `${form...}` placeholders, because the desktop has
  no selection or form to fill them.
- **Grant:** the desktop runs commands and reads status sources over one
  connection per package. That connection keeps only the package's
  `capabilities` (see [grants.md](grants.md)), so a contribution can do
  no more than its package declares.
- **Refused packages:** a package that does not validate is refused, and
  so is one whose `id` an earlier package (by directory name) already
  has. The rest still load.
- **Taken keys:** a keybinding the desktop or an earlier package already
  uses is dropped. Its command stays in the palette without it.
- **Problems:** each one is listed in the sidebar's Apps section and
  printed on the desktop's standard error.
- **Changes:** the desktop reads the directory again when a manifest
  changes.
