# Pane apps (`dreamer-termapp`)

`dreamer-termapp` runs an app package: a manifest that declares where its
data comes from, how to show it, and which keys run which actions. The app
is the tmux pane's real program, so it looks and works the same in the
desktop and through any plain `tmux attach`.

The app's model (the manifest, its data and selection, a form's values,
commands, the Luau module, and reloading) is the headless `app-model`
crate. `dreamer-termapp` only turns keys into input for it and draws it
with Ratatui. The desktop draws the same model natively: in the app's
own pane, from the model the running app publishes (see
[app panes](#app-panes)), or as a panel with a model of its own (see
[desktop.md](desktop.md#app-panels)).

```sh
dreamer-termapp app.toml            # run it in this terminal
dreamer-termapp --check app.toml    # validate only (--json: also the grant it needs)
dreamer-termapp --preview app.toml  # draw it once from fixtures, no daemon
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
# all_panes = true          # grant every pane: to read others, or hear their events

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
- **Collectors as data:** a source with `collector = "NAME"` instead of
  `method` shows a collector's rows ([collectors.md](collectors.md)). It
  calls `collect.query` with that name; its `params` are the query's other
  parts (`fields`, `panes`, `since_unix_ms`, `group_by`, ...), and `name`
  is refused there. It asks for as many rows as the source's `limit`
  unless `params` sets `limit`, and the rows are the newest, oldest first.
  Each row's fields become members of the row, so `columns` and `field`
  name them directly (`test`, `severity`); a row's own member of the same
  name (`pane`, `line`, ...) wins, and all fields stay in `fields`. With
  `group_by`, the rows are the groups (`value`, `count`,
  `latest_unix_ms`), which a `chart` of `count` draws. Add
  `events = ["collect.row"]` to fetch again on each new row. A collector
  source adds `collect.query` and every pane to the grant the app needs:
  a collector's rows come from any pane it covers. The collector itself is
  made elsewhere (`dream collect create`, or an agent); a source never
  creates one, and a source naming a collector that does not exist shows
  the daemon's error in the status line.

  ```toml
  [data.failures]
  collector = "test-failures"   # e.g. dream collect create --preset test-failures
  events = ["collect.row"]

  [view]
  type = "table"
  source = "failures"
  row_key = "id"
  columns = ["test", "file", "pane", "time_unix_ms"]
  ```
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

## Building an app

Building an app takes five steps, each a `dream app` command
([cli.md](cli.md#apps)). An agent can do all of them: `dream skills install`
gives Claude Code and Codex the `dreamer-apps` skill, which walks through
them.

1. **Write** the package directory: `app.toml`, and for each view's data
   source a `fixtures/<source>.json` holding what its method returns.
2. **Check:** `dream app check DIR` lists every problem and the grant the
   app needs: its `capabilities`, plus `events.subscribe` when a source
   follows events and `collect.query` when a source is a collector, on its
   own pane, its agent's panes, or every pane with `all_panes` (or a
   collector source).
3. **Preview:** `dream app preview DIR` draws the app once from its
   fixtures, through its module's `transform`, with no daemon. The status
   line says `preview from fixtures`.
4. **Install:** `dream app install DIR` checks it, previews it when it has
   fixtures, and copies it to `<config dir>/apps/<id>/`. A package that
   fails changes nothing. The version it replaces is kept in
   `apps/.last-good/`, and `dream app rollback NAME` swaps it back.
5. **Launch:** `dream app launch NAME` runs it in a new tmux window as a
   registered app with exactly the grant it needs. It keeps running with
   no desktop window open.

An installed app that is running reloads itself when its files change,
and an install replaces its files whole. A version that does not validate
leaves the running one in place.

Events about a pane reach an app only when that pane is in its grant. A
monitor of jobs, which run in their own panes, sets `all_panes = true`.

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

## App panes

A registered app is its pane's host. While registered, it publishes its
model to the daemon whenever it changes (only the newest is kept; a
model superseded while one is being sent is skipped), and the daemon
marks its pane in the topology. The desktop then draws that pane
natively from the published model instead of its terminal, and sends
it the keys typed there; see [desktop.md](desktop.md#app-panes). Keys
from the desktop and keys typed in the pane go through the same input
handling, so `q` from either quits the app. A plain tmux client keeps
seeing the text interface.

An app started without a launch ticket (`dreamer-termapp app.toml` in a
shell) is not registered: the desktop draws its pane as a terminal.
After a daemon restart the pane is drawn as a terminal too, until the
app is relaunched.

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
kind = "inspector"                # inspector, monitor, filter, or operator
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
- **Read-only kinds:** an inspector, monitor, or filter pane may not list
  a method that changes things, nor `terminal.input`. Such a manifest does
  not validate. An operator may list actions.
- **No model:** with no provider ready, the header says why.
  - An inspector then answers in a scripted mode: the prompt is searched
    as plain text in the newest lines of the panes it may read
    (`terminal.query`), and the matching lines are listed.
  - A monitor keeps showing its data source, which needs no model.
  - A filter pane watches what is typed as plain text (see below).
  - An operator says that it needs a provider.

### Filter panes

A filter pane turns a description into [filters](filters.md) and shows
the lines they match as they happen: the derived stream.

```toml
capabilities = ["terminal.query", "tmux.topology", "filters.manage"]

[agent]
kind = "filter"
tools = { methods = ["terminal.query", "tmux.topology", "filters.manage"], all_panes = true }
```

- **Grant:** `terminal.query` and `filters.manage` are required, and
  `filters.manage` belongs to filter panes only. The filters it installs
  only read: they [record and mark lines](filters.md#filters-an-app-installs)
  on the panes in its grant, and never publish, send, or type.
- **With a model:** the model uses `watch_lines`, `list_filters`, and
  `stop_filter` ([mcp.md](mcp.md)), and `find_lines` to try a pattern
  first. "Show me errors from the build panes" becomes a filter.
- **No model:** what you type is watched as plain text, case-insensitive,
  on every pane in the grant except this one, and matched lines are
  highlighted. `stop NAME` removes a filter.
- **Header:** the filters it manages. Each match appears in the
  transcript as `%PANE FILTER: line`. Lines in the filter pane itself are
  never shown, so its own output cannot feed back. A filter whose circuit
  breaker trips says so.
- Filters keep running after the pane quits, so their marks stay; remove
  them with `stop NAME`, or `filters.remove`.

### Operator panes

An operator acts, through the typed API and its narrowed MCP grant only.
What it did stays inspectable above the transcript:
- **Plan.** What the model said before its first action, for each request
  (it is asked to say what it will do, and to which panes or windows,
  before acting).
- **Steps.** Every action or job it started, one line each: `✓`, `…`, or
  `✗`, then the tool, its target (panes, window, program, or id), its host
  (`@devbox`), and its state with what was observed (`completed: created
  pane %9`, `succeeded (exit 0)`). Open actions and running jobs are read
  again every second until they settle (`actions.get`, `jobs.get`, through
  `hosts.call` on a host).
- **Failures** name the tool and its actual target, with the daemon's
  reason, never only an optimistic summary.
- **Cancel.** Esc stops the run and cancels the jobs it started that
  still run.
- **Limits.** `max_turns` and `timeout_s` from the manifest, shown in the
  header (`at most 6 turns; stops after 120 s`).
- Typing into panes goes through an interaction lease ([input.md](input.md)):
  once a person types into a pane, the operator stops typing there.

Asking for more scope at run time is not built yet; an operator has the
grant its manifest declares.

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
