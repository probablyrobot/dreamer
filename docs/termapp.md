# Pane apps (`dreamer-termapp`)

A pane app is a small program with a user interface that runs as a tmux
pane's own program: a table of panes, a dashboard of test failures, a
chat with an agent. Because it is the pane's real program, it works
through any plain `tmux attach`, and keeps running with no desktop window
open. The Dreamer desktop draws the same app natively.

An app is a package: a directory with a manifest, `app.toml`. There are
four ways to write one.

| Kind | You write | Use it when |
| --- | --- | --- |
| [Manifest app](#the-manifest) | Only `app.toml`: data sources, views, and commands on keys | Showing API data in tables, lists, and forms |
| [Script app](#script-apps-appts) | `app.ts`, TypeScript run sandboxed by the app host | The app needs state and logic. `dream app new` starts one |
| [Process app](#process-apps) | A program in any language that sends its view as JSON | The app needs files, the network, or other programs |
| [Web app](#web-pages-and-web-apps) | A web page in the package | The view is a page, drawn by the desktop |

`dreamer-termapp` is the app host: it runs the package, draws it in the
pane, and publishes its view to the desktop.

Contents:

- [A first app](#a-first-app)
- [Building an app](#building-an-app): [templates](#starting-from-a-template),
  [live preview](#live-preview), [testing](#testing-an-app)
- [The manifest](#the-manifest): data sources, views, commands, keys
- [View trees](#view-trees): layouts and every component;
  [text, input, and status](#text-input-and-status-components),
  [terminals and images](#terminal-and-image-components),
  [web pages](#web-pages-and-web-apps),
  [agent sessions](#agent-sessions-transcript-and-composer),
  [bound streams](#bound-streams), [the tree on the wire](#the-tree-on-the-wire)
- [Process apps](#process-apps)
- [Script apps](#script-apps-appts): [the app](#the-app),
  [globals](#dreamer-ui-and-the-other-globals), [`fetch`](#fetch),
  [types and checking](#types-and-dream-app-check),
  [keys and focus](#keys-and-focus), [limits](#limits-and-failure)
- [Modules](#modules), [forms](#forms), [reloading](#reloading)
- [Connection](#connection) and [app panes](#app-panes)
- [Agentic panes](#agentic-panes): [filter panes](#filter-panes),
  [operator panes](#operator-panes)
- [Contributions](#contributions)

## A first app

The shortest way to a running app is a template. This writes a script app
that lists the panes, checks it, draws it once as text, installs it, and
launches it in a new tmux window:

```sh
dream app new panes                 # writes ./panes: app.toml, app.ts, fixtures
dream app check panes
dream app preview panes --text
dream app install panes             # installed as local.panes
dream app launch local.panes
```

The smallest app you can write by hand is one file. Save this as
`windows/app.toml`:

```toml
schema_version = 1
id = "local.windows"
version = "0.1.0"
title = "Windows"
capabilities = ["tmux.topology"]

[data.windows]
method = "tmux.topology"
path = "windows"
events = ["tmux.topology"]

[view]
type = "table"
source = "windows"
row_key = "id"
columns = ["id", "name"]
```

Then validate it and run it in the current terminal (`q` quits):

```sh
dreamer-termapp --check windows/app.toml
dreamer-termapp windows/app.toml
```

Run this way, the app is not registered with the daemon, so the desktop
draws its pane as a terminal. `dream app install windows` and
`dream app launch local.windows` run it as a registered app, which the
desktop draws natively ([app panes](#app-panes)).

`dreamer-termapp` takes these forms:

```sh
dreamer-termapp app.toml            # run it in this terminal
dreamer-termapp --check app.toml    # validate only (--json: also the grant it needs)
dreamer-termapp --preview app.toml  # draw it once from fixtures, no daemon
dreamer-termapp --replay app.toml   # replay fixtures/events.jsonl (dream app test)
dreamer-termapp --live app.toml     # a live preview from fixtures (dream app preview --open)
dream app run panes --method tmux.topology --method actions.submit --all-panes \
    -- dreamer-termapp app.toml      # run it in a new window with a narrow grant
```

## Building an app

Building an app takes these steps, each a `dream app` command
([cli.md](cli.md#apps)). An agent can do all of them: `dream skills install`
gives Claude Code and Codex the `dreamer-apps` skill, which walks through
them. `DIR` is the package directory, or its `app.toml`.

1. **Start:** `dream app new NAME --template T` writes a working package
   ([below](#starting-from-a-template)). Or write the package directory
   yourself: `app.toml`, and for each view's data source a
   `fixtures/<source>.json` holding what its method returns (for a
   script app, `app.ts` and a `fixtures/load.json` holding what its
   `load` returns).
2. **Check:** `dream app check DIR` builds a script app's JavaScript from
   its `app.ts` and checks its uses of the API and the builders
   ([types](#types-and-dream-app-check)), and lists every problem and the
   grant the app needs: its `capabilities`, plus `events.subscribe` and
   `events.unsubscribe` when a source follows events and `collect.query`
   when a source is a collector, on its own pane, its agent's panes, or
   every pane with `all_panes` (or a collector source).
3. **Preview:** `dream app preview DIR --text` draws the app once from its
   fixtures, through its module's `transform`, with no daemon, as text on
   a screen of 80x24 (`--size WxH` for another). The status line of a
   manifest app says `preview from fixtures`. For a script or process
   app, `--open` shows it live in the desktop and reloads it on every
   save, and `--png FILE` writes the desktop's drawing of it as an image
   ([below](#live-preview)).
4. **Test:** `dream app test DIR` replays the app's events and compares
   each screen with the one expected ([below](#testing-an-app)).
5. **Install:** `dream app install DIR` checks it, previews it when it has
   fixtures, and copies it to `<config dir>/apps/<id>/` (or
   `apps/NAME/` with `--name NAME`). A package that fails changes
   nothing. `dream app list` shows what is installed. The version it replaces is kept in
   `apps/.last-good/`, and `dream app rollback NAME` swaps it back.
6. **Launch:** `dream app launch NAME` runs the installed package `NAME`
   (its id, unless it was installed with `--name`) in a new tmux window
   of the first session (`--session $N` for another; `--beside %N` for a
   pane beside that one) as a registered app with exactly the grant it
   needs. It keeps running with no desktop window open.

An agent follows the same steps with the app tools of `dream mcp` and of
Dreamer's pi extension ([mcp.md](mcp.md#app-tools)).

An installed app that is running reloads itself when its files change,
and an install replaces its files whole. A version that does not validate
leaves the running one in place.

Events about a pane reach an app only when that pane is in its grant. A
monitor of jobs, which run in their own panes, sets `all_panes = true`.

### Starting from a template

`dream app new NAME [--template T] [--dir DIR]` writes a script app into
`DIR` (`./NAME` by default), which must be missing or empty. `NAME` is
lowercase letters, digits, and `-`; the app's id is `local.NAME`.

| Template | The app | Capabilities |
| --- | --- | --- |
| `table` (the default) | The panes in a table; Enter selects the pane | `tmux.topology`, `actions.submit`, `all_panes` |
| `form` | A select, an input, and a button that rename a window | `tmux.topology`, `actions.submit`, `all_panes` |
| `chat` | A transcript (`log`) and a composer (`input`); each message gets an answer from a function in `app.ts` to replace | `daemon.version` |

There is no `web` template: `--template web` is refused. For a page,
start from another template and add a `web` node, or write a web app
([web pages and web apps](#web-pages-and-web-apps)).

The package has `app.toml`, `app.ts`, `fixtures/load.json`,
`fixtures/events.jsonl`, `fixtures/calls.json` when the app calls a
method in its test, `types/dreamer.d.ts` and `tsconfig.json` (as
`dream app types` writes them), the JavaScript built from `app.ts`, and
the expected screens of its test, recorded as it is made. So it passes
`dream app check`, `preview`, and `test` straight away. The templates
are part of the `dream` program.

### Live preview

`dream app preview DIR --open [--session $N]` opens a preview of a script
or process app in a new tmux window (of the first session, or `$N`),
named `preview <directory name>`. The desktop draws it natively, like
any [app pane](#app-panes); a plain tmux client sees its text
interface. A manifest app is previewed with `--text` only.

- **From fixtures.** The app runs as `dream app preview --text` runs it:
  a script app draws `fixtures/load.json` as its data, and every call
  it makes is answered from `fixtures/calls.json` (refused outside its
  capabilities, as in a test). Nothing it does reaches the daemon or
  tmux, so a preview never renames a window or runs an action. Keys,
  clicks, and typing work as in any app pane.
- **Reload on save.** The window's program is `dream app preview DIR
  --pane`. It builds the app's JavaScript from `app.ts` whenever
  `app.ts` or `app.toml` changes, and runs `dreamer-termapp --live`, the
  app host. The host looks at the package five times a second and
  starts the app again from its fixtures when any file in it changed:
  the JavaScript built, `app.toml`, the fixtures, a process app's
  program. Hidden files, `types/`, `node_modules/`, and
  `fixtures/expect/` do not count. The last view stays on the screen
  until the new one is drawn; the app's state starts over.
- **The status line** says `preview from fixtures`, then how many times
  it reloaded. A save that does not build shows `not built:` and the
  first error, and the last version that built keeps running; the first
  type mistake (`dream app check` lists them all) is shown the same way.
  An app that fails is shown stale with why, as in any pane; `r`
  starts it again.
- **Ending it.** `q` (when the app has stopped) or Ctrl-C ends the host,
  and the pane closes with it.

The preview's app id is the package's id followed by `.preview`.

`dream app preview DIR --png FILE` writes the app as the desktop draws
it, as a PNG of the preview pane's rectangle at the display's
resolution, in the window's theme. It uses the newest preview pane of
the package that is open, or opens one as `--open` does and leaves it
open. It waits until the desktop has drawn the newest version of the
app's view, then asks the desktop for the image through the daemon (see
[topology.md](topology.md#images-of-app-panes)). A desktop window must
show the pane's tmux window; one opened by the preview is the active
window of its session, so a desktop window showing that session shows
it.

- With no desktop running (no daemon, or a daemon no desktop is
  connected to), it fails at once with `no Dreamer window is running;
  --png needs one`.
- **A web app** ([above](#web-pages-and-web-apps)) is previewed too: its
  preview pane runs it as it runs anywhere (it has no fixtures; its
  page's calls are refused, since a preview's grant is empty), and the
  image is its page alone, from Chromium in the desktop
  ([desktop.md](desktop.md#agents-and-pages)), not a capture of the
  screen. `--open` shows it the same way; it is not reloaded on a save.
- On macOS the desktop captures its own window with the system's
  `screencapture`, so the first capture may ask to allow Dreamer, or the
  terminal that started it, to record the screen. Other platforms answer
  that this desktop captures panes on macOS only.
- An image without a display (a software renderer in the app host) is
  not available.

### Testing an app

`dream app test DIR [--update] [--size WxH]` runs the app as the preview
does, from its fixtures and with no daemon, and replays
`fixtures/events.jsonl` against it. It draws the screen as text at the
start and after each event, and compares each with
`fixtures/expect/NNN.txt`: `000.txt` is the first view, `001.txt` the
screen after the first event, and so on.

- **Events** are one per line, as the [app process
  protocol](app-protocol.md) sends them; blank lines and lines starting
  with `#` are skipped:

  ```jsonl
  {"kind":"key","key":"Down"}
  {"kind":"text","text":"hello"}
  {"kind":"key","key":"Enter"}
  {"kind":"select","node":"panes","index":2}
  {"kind":"scroll","node":"log","steps":3}
  {"kind":"component","node":"name","action":"submit","value":"x"}
  ```

  Keys are named as in [keys and focus](#keys-and-focus). A `component`
  event is for script and process apps; a manifest app is driven with
  keys, text, select, and scroll.
- **A screen** is what `dream app preview --text` prints, then a line for
  each thing the text does not show: `--- focus: KEY` for the focused
  node, and `--- KEY: row ROW selected` for each table or list with a
  selected row.
- **Calls** to `dreamer.*` (and a process app's calls) are answered from
  `fixtures/calls.json`: an object of results by method name, the same
  result for every call of that method. A call to a method outside the
  app's capabilities is refused as it would be in a pane, and a method
  with no result in the file is refused with an error the app sees.
- **Settling.** After each event the test waits until the app has handled
  it and every call it led to, then takes the screen: it asks for the
  whole tree (`resync`) and answers each call before asking again.
  Timers are not waited for. A process app must send its whole tree only
  when asked (first, and after `resync`), as the Rust SDK does.
- **Manifest apps** (version 1 and 2) take events as in a pane, but their
  commands do not run: the status line names the commands an event would
  run, with `not run in a test`.
- **Failing.** Each screen that differs is reported with the event before
  it and the rows that differ, `-` expected and `+` drawn; a missing
  expected screen, and one no event draws any more, are reported too.
  The command then fails. An app that stops, quits before its last event,
  or takes more than 5 seconds to settle fails the test with why.
- **Recording.** `--update` writes every screen as the expected one and
  removes the ones no event draws. Use it after a deliberate change, and
  read the screens it wrote. Record and test at the same `--size`.

`dreamer-termapp --replay app.toml [--size WxH]` does the replay and
prints the screens as JSON (`{"screens": [...]}`); `dream app test` runs
it and compares.

## The manifest

`app.toml` declares where the app's data comes from, how to show it, and
which keys run which commands. This one has every part of a version 1
manifest:

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
type = "table"              # table, list, log, text, gauge, chart, or form
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
  recorded ([events.md](events.md)). A source keeps at most `limit` rows
  (500 by default, 1 to 10000).
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
  or Ctrl-C quits. Commands may use any other single printable ASCII
  character (letters, digits, punctuation), Enter, Esc, Tab, Backspace,
  Delete, Left, Right, Home, End, F1 to F12, and Ctrl-a to Ctrl-z.

Validation is strict: unknown keys, unknown methods, mutations as data
sources, malformed placeholders, and duplicate or reserved keys are errors,
each reported with where it is.

## View trees

A manifest with `schema_version = 2` lays out several views at once as
a tree of nodes. It names the `root` node and declares every node
under `[nodes.<key>]`; it has no `[view]`. Data sources, commands, the
module, and contributions are as in version 1, and version 1 manifests
keep working unchanged.

```toml
schema_version = 2
id = "local.overview"
version = "0.1.0"
title = "Overview"
capabilities = ["tmux.topology"]
root = "main"

[data.panes]
method = "tmux.topology"
path = "panes"
events = ["tmux.topology"]

[data.windows]
method = "tmux.topology"
path = "windows"

[nodes.main]
type = "split"
sizes = [60, 40]              # weights, one per child; equal when left out
children = ["panes", "side"]

[nodes.panes]
type = "table"
title = "Panes"
source = "panes"
row_key = "id"
columns = ["id", "current_command"]

[nodes.side]
type = "tabs"
children = ["windows", "sizes"]

[nodes.windows]
type = "list"
title = "Windows"             # a tab's name
source = "windows"
row_key = "id"
field = "name"

[nodes.sizes]
type = "table"
title = "Sizes"
source = "windows"
row_key = "id"
columns = ["name", "width", "height"]
```

- **Layout nodes** list their `children` by key, in order:
  - `column` stacks them; text and gauges take the height they need,
    the others share the rest.
  - `row` puts them side by side, sharing the width.
  - `split` divides the space by `sizes`, side by side, or one above the
    other with `direction = "column"`.
  - `tabs` shows one child at a time; each child's `title` (else its key)
    names its tab.
  - `scroll` stacks them at their natural height and scrolls to keep the
    focused one in view.
- **Data nodes** are `table`, `list`, `log`, `text`, `gauge`, `chart`,
  and `form`, with the same fields as a version 1 `[view]` (`source`,
  `row_key`, `columns`, `field`, `label`, `fields`, `submit`). A tree
  has at most one form.
- **Text, input, and status components** (`markdown`, `code`, `diff`,
  `button`, `input`, `select`, `toggle`, `badge`, `progress`, `status`)
  are described [below](#text-input-and-status-components).
- **`transcript` and `composer`** show and write to an agent session;
  see [below](#agent-sessions-transcript-and-composer).
- **Terminal and image components** (`terminal`, `image`) are described
  [below](#terminal-and-image-components), and **web pages** (`web`)
  [below that](#web-pages-and-web-apps).
- **`title`**, on any node, is a heading; in a terminal the node gets a
  border with it.
- **Keys** of nodes are lowercase letters, digits, `-`, and `_`. Every
  node except the root has exactly one parent, and every node must be
  reached from the root. Validation names each mistake with its node.

Keys: Tab moves the focus to the next table, list, form, or input
component shown; Left and Right switch the page of the tabs holding the
focus (or the first tabs). Up and Down move the focused table's or
list's selection, and in a focused form they move between fields. Commands act on the focused
node's selected row (`${selected.<field>}`). Tab, Left, and Right are
the runtime's in a tree, so commands may not use them.

Every data source a node shows is fetched as in version 1, and the
sources' `events` and the shortest `refresh_ms` apply. A module's
command handlers work as before; `transform` is not called for a tree.
`dream app preview` reads a fixture for every source shown.

On the desktop, a click on a row, field, or input component and wheel
steps over a table or list go to the node under the pointer, which
takes the focus; Copy takes the focused node's selected row, form
field, input's text, or select's option
([desktop.md](desktop.md#input-in-app-views)).

### Text, input, and status components

Each is a node like the others, drawn natively on the desktop and as a
terminal interface in the pane, in the colours of the one scheme: the
desktop takes them from the theme its chrome uses, and the terminal from
its own palette (the scheme's ANSI colours), never as fixed colours.
`examples/apps/components/` in the source tree shows every one.

| `type` | Shows | Fields |
| --- | --- | --- |
| `markdown` | Headings, paragraphs, `-`/`*`/`+` and numbered lists, quotes, rules, fenced code blocks (highlighted by their language), inline code, strong and emphasis, and links as their text | `text`, or `source` and `field` |
| `code` | Source code, highlighted: keywords, strings, numbers, comments, and types | `text` (or `source`, `field`), `language` |
| `diff` | A unified diff: file and hunk headers, added and removed lines marked | `text`, or `source` and `field` |
| `badge` | A short label in a tone | `text` (or `source`, `field`), `tone` |
| `progress` | A ratio as one thin bar, with its label | `ratio` (0 to 1) and `text` (the label), or `source`, `field`, and `label` as for a gauge |
| `status` | A status line: short items side by side, each in a tone | `items` (`{ text, tone }`), or `source` and `field` (one item per row) |
| `button` | A button | `label`, `run` |
| `input` | One line of text | `label`, `value` (the starting text), `placeholder`, `submit` |
| `select` | One of a few options | `label`, `options`, `value` (the starting option; the first otherwise), `run` |
| `toggle` | On or off | `label`, `on` (at the start), `run` |

- **Content.** `text` gives the content in the manifest. With a
  `source` instead, it is the first row's `field` (the whole row when
  there is no `field`), and it changes as the source is fetched again.
- **Tones** are `neutral` (the default), `accent`, `success`, `warning`,
  and `danger`.
- **Languages** highlighted are `rust`, `ts` (and `typescript`, `js`,
  `javascript`), `python`, `sh` (and `bash`, `zsh`, `shell`), `go`, `c`
  (and `cpp`), `json`, `toml`, `yaml`, and `lua` (and `luau`). Any other
  language is shown plain. Highlighting is by tokens, not a parser.
- **Values.** The app keeps each input component's value by its key: an
  input's text, a select's option, and a toggle's state. A reload keeps
  them (a select's only while it is still an option), and so does every
  update to the tree, as it keeps the focus.

The input components take the focus with Tab or a click, like a table:

| Component | Keys | Click | Event |
| --- | --- | --- | --- |
| `button` | Enter or Space presses it | Presses it | `press`: runs `run` |
| `input` | Letters, digits, and Space type (so `q` is a letter here; Esc or Ctrl-C quits), Backspace deletes, Enter submits | Focuses it | `change` on every edit; `submit` on Enter: runs `submit` |
| `select` | Up and Down move to the option before or after; Enter or Space to the next, wrapping | Picks the option clicked | `change`: runs `run` |
| `toggle` | Enter or Space flips it | Flips it | `change`: runs `run` |

Text typed through the desktop's text input or pasted goes into a
focused `input` as one edit, as into a form field. A key the focused
component does not take (a command's letter while a button is focused)
works as usual.

A command a component runs, and any other command, reads the values:
as `${value.<node>}` in a method's `params` (an input's and a select's
as text, a toggle's as a boolean), and as `values` (by node key) in a
handler's argument, beside `selected` and `form`. A handler also gets
the `event` that ran it: `node`, `type` (`press`, `change`, or
`submit`), and `value` (the component's value after it; null for a
button). A command only components or a form run needs no `key`:

```toml
[nodes.name]
type = "input"
label = "Name"
submit = "greet"            # Enter

[nodes.save]
type = "button"
label = "Save"
run = "save"

[commands.greet]
handler = "greet"           # function greet(arg) return "hello, " .. arg.values.name end

[commands.save]
method = "actions.submit"
params = { action = { kind = { rename_window = { window = 1, name = "${value.name}" } } } }   # window @1
```

Validation names each mistake with its node: a `run` or `submit` that is
no command, a text or status component with neither its content
(`text`, `ratio`, or `items`) nor a `source`, a select
without options or with a `value` not among them, a button or toggle
without a `label`, an input component with a `source`, and a command
with no key that nothing runs.

### Terminal and image components

A `terminal` shows a tmux pane's screen, live, inside the app. An `image`
shows a picture from the app's package.

```toml
capabilities = ["terminal.read"]

[nodes.left]
type = "terminal"
title = "build"
pane = 3                    # %3

[nodes.logo]
type = "image"
src = "img/logo.png"        # a file in the package
```

**`terminal`**

- **The grant.** The app needs `terminal.read` in `capabilities`, and its
  grant must include the pane. A manifest's `terminal` panes are part of
  the grant it needs (`--check --json`, `dream app check`), so `dream
  app launch` grants them. A script or process app names its panes at run
  time, so launch it with them (`dream app run ... --pane %3`) or with
  `all_panes = true`.
- **Bound, not copied.** The pane's screen never passes through the
  app's code or its tree. The node only names the pane, and each
  renderer follows it:
  - the pane's own terminal reads it with `terminal.read` on the app's
    connection, about four times a second, and shows as many of its rows
    as fit, keeping the cursor's row in view;
  - the desktop draws it with its terminal renderer
    ([desktop.md](desktop.md#app-panes)).
- **Read only.** Keys never go to the pane shown, and it never takes the
  focus.
- **A pane outside the grant is refused.** The host marks the node
  refused, and every renderer shows `refused:` and why instead of the
  pane. The daemon refuses the host's read of it, and the app's status
  line says `terminal %N:` and the daemon's reason, until something else
  replaces it. The daemon also
  refuses a published tree that shows such a pane without marking it, so
  a process app cannot show a pane by leaving the mark out.
- A pane that is gone shows `pane %N:` and why.

**`image`**

- **From the package only.** `src` is a path relative to the package
  directory, ending in `.png`, `.jpg`, or `.jpeg`. The host refuses an
  absolute path, one with `..`, and a file that a link leads out of the
  package. `--check` refuses the others in a manifest.
- **What is shown.** A PNG or JPEG of at most 8 MiB and 8192 pixels on
  a side. The host reads only the file's header, for its size.
- **Where.**
  - A terminal shows `[image: NAME WxH]`, or `[image: NAME: why]` when
    the image is refused.
  - The desktop reads and decodes the file off its UI thread, then draws
    the picture at its own size, shrunk to fit the node.

### Web pages and web apps

A `web` node is a web page, drawn by Chromium in the desktop
([desktop.md](desktop.md#app-pages)). A whole app can be a page: a
version 2 manifest with `[web]` and no view of its own (no `root`,
`[script]`, or `[process]`) is a **web app**, and its view is its page.
`examples/apps/panes-web/` in the source tree is one: a page that lists
the panes.

```toml
schema_version = 2
id = "local.panes-web"
version = "0.1.0"
title = "Panes (web)"
capabilities = ["tmux.topology"]

[web]
# url = "http://localhost:5173"      # load the pages from a dev server instead
allow = ["https://tmux.github.io"]   # origins the pages may also navigate to
```

```text
local.panes-web/
  app.toml
  web/index.html     # the web app's page; anything else it loads beside it
```

In a tree, a node `type = "web"` with `src` is a page; a script uses
`ui.web({ key, src })`. `src` is:

- a path in the package's `web/` directory (`index.html` when empty);
- or an address on the app's origin or one of its `web.allow` origins.

**The app's origin.** The package's `web/` directory is served on
`dreamer-app://<app id>/`, from the package and nowhere else: a path
that leaves the directory, also through a link, is not found. With
`web.url`, a local address (`http`, or `https`, on `localhost`,
`127.0.0.1`, or `[::1]`, such as a dev server), the pages load from
there instead, and that address's origin is the app's.

**The bridge.** A page on the app's origin has `window.dreamer.call(method,
params)`: one API method by name, its parameters and its result as the
API's JSON ([api.md](api.md)), as `dreamer.call` in a script. It
resolves to the result, or rejects with an error that has the API's
`code` and a `message`.

- Each call goes from the page to the desktop showing it, and from there
  to the daemon (`apps.call`), which runs it as the app in that pane:
  under the app's grant, with the same checks as the app's own requests
  ([grants.md](grants.md#web-pages-of-apps)). A method outside the grant
  is refused with `-32005`. The page has nothing the app does not.
- Requests answered once only: subscriptions, and what only an app host
  does (registering, publishing its view, narrowing its grant), are
  refused.
- Reading and driving pages (`web.read`, `web.click`, `web.type`,
  `web.screenshot`) needs `web.inspect` in `capabilities`
  ([grants.md](grants.md#reading-and-driving-pages)).
- A download the page starts asks the user (Save or Cancel, in the pane)
  only with `web.download` in `capabilities`; without it, it is refused
  ([grants.md](grants.md#saving-downloads)).
- A page on any other origin, an allowed one or a frame inside the app's
  page, has no `window.dreamer`, and its requests to the bridge are
  refused.

**Navigation** stays on the app's origin and its `web.allow` origins:
any other address is refused and the page stays where it is. A page
opens no windows.

**Profile.** Each app's pages have a browser profile of their own,
`web/app-<app id>/` in the desktop's state directory: their cookies and
storage are kept apart from every other app's and from plain web panes'.

**Elsewhere.** The pane's own terminal shows `[web: ADDRESS; open in
Dreamer to see the page]`, or `[web: SRC: why]` for a refused node. A
desktop panel shows the address.

**Checks.** `--check` (and `dream app check`) refuses a `web.url` that
is not local, a `web.allow` entry that is not an origin (`https://host`
or `https://host:port`, no path), and a `web` node whose `src` leaves
`web/`, names another site, or names a page that is not in the package.
A script names its pages at run time; the host refuses a `src` that is
none of the above, and the node shows why.

### Agent sessions: `transcript` and `composer`

A `transcript` shows an agent session's conversation
([agents.md](agents.md#sessions)), and a `composer` writes prompts to it.
Both are bound to the session: its events go from the daemon straight to
each renderer, which follows the session itself
([bound streams](#bound-streams)). The app's own code never sees a token.
`examples/apps/chat/` in the source tree is a chat app made of the two.

```toml
capabilities = ["agent.sessions.create", "agent.sessions.list"]

[data.sessions]
method = "agent.sessions.list"
path = "sessions"

[commands.new]
key = "Ctrl-n"
method = "agent.sessions.create"
params = { grant = { methods = ["tmux.topology"] } }

[nodes.chat]
type = "transcript"
source = "sessions"           # the first row's `field` is the session id
field = "id"

[nodes.prompt]
type = "composer"
source = "sessions"
field = "id"
placeholder = "Ask the agent"
```

- **The session.** `session = "<id>"` names one, or `source` and `field`
  take it from the first row of a data source, as above. An app sees only
  the sessions it created (unless it holds the owner grant), so an app
  usually creates its own with `agent.sessions.create`; the session's
  grant must be within the app's.
- **What the transcript shows:** each prompt sent; the answer as
  markdown, drawn as it streams (finished blocks are parsed once, so a
  long answer does not get slower to draw); thinking, folded to one line
  until shown; a card per tool call with what it ran (`$ command`, a
  path, or its input) and its newest output lines, a unified diff marked
  as one; a card per approval request with its answer buttons, and who
  answered once it is answered; errors and stopped turns; the plan; and
  the session's state with the latest usage (tokens, and cost when the
  harness reports it) on its last line. It keeps the newest 2,000
  entries.
- **What the composer holds:** text over several lines, the prompts sent
  (the newest 100), and attachments. Its text is the app's like an
  input's: it is kept across updates and reloads, and is
  `${value.<node>}` to a command.
- **Attachments.** `@%N` in the text attaches the visible text of pane
  `N` (it needs `terminal.read`, and the pane in the grant), and `@name`
  the newest 20 rows of the collector `name` (it needs `collect.query`);
  an `@name` that names no collector stays as typed. Ctrl-s attaches the
  selection: tmux's newest buffer, which a finished selection in Dreamer
  becomes (it needs `clipboard.read_tmux_buffer`). Each attachment goes
  after the prompt as a block headed by what it is (`Pane %3:`,
  `Selection:`), fenced, at most its last 200 lines and 16 KB. One that
  cannot be read is left out and the status line says why.
- **The grant.** A transcript adds `agent.sessions.events`,
  `agent.sessions.approve`, `agent.sessions.deny`, and
  `terminal.unsubscribe` to the grant the app needs; a composer adds
  `agent.sessions.send`, `agent.sessions.cancel`, `agent.sessions.events`,
  and `terminal.unsubscribe`. Sending, stopping,
  answering, and attaching are calls on the app's own connection, so its
  grant decides them.

Keys, when the node has the focus (Tab moves it there):

| Node | Keys | Clicks (desktop) |
| --- | --- | --- |
| `composer` | Letters, digits, and Space type (so `q` is a letter here); Backspace deletes; Enter sends; Ctrl-j (Shift-Enter on the desktop) starts a new line; Up and Down step through the prompts sent; Esc stops the running turn; Ctrl-s attaches the selection; Ctrl-x removes the last attachment. Ctrl-c quits the app | Send, Stop (while a turn runs), Attach selection, and an attachment's × |
| `transcript` | `y` allows the oldest approval request waiting (with its first option, if it offers options), `1` to `9` allow it with that option, `n` denies it; `t` shows or folds thinking; Up, Down, PageUp, PageDown, Home, and End scroll | An approval card's buttons; thinking shows or folds it |

Pasted text and text from the desktop keep their line breaks in a
composer. In a terminal the composer shows its newest six lines, and
`working · Esc stops` while a turn runs.

### Bound streams

A node with `bind` draws a daemon stream instead of content in the tree:

| Node | `bind` | Stream |
| --- | --- | --- |
| `transcript`, `composer` | `{ "session": "<id>" }` | `agent.sessions.events` with `follow`, from the event after the last one held |
| `log` | `{ "tap": <pane> }` | `taps.subscribe` at the `text` level: the pane's committed lines, the newest 2,000 |

In a manifest, a log's `tap = <pane>` binds it (instead of a `source`);
it adds `taps.subscribe`, `taps.unsubscribe`, and the pane to the grant
the app needs. A process app sets `bind` on the nodes it sends.

Each renderer follows the streams itself, from the tree:

- **The app host**, for its terminal interface, on its own connection,
  so under the app's grant.
- **The desktop**, for its native rendering, on the user's connection,
  with `for_pane` set to the app's pane: the daemon then also checks the
  app's grant (the method, the panes, its expiry, and for a session that
  the app may see it), so the desktop never shows an app a stream the app
  may not read. A refusal is drawn in the node (`not shown:` and why).

A stream is followed while the tree binds it and stops when it no longer
does. After a lost connection it is followed again, a session from the
event after the last one held, so nothing is drawn twice or missed.

### The tree on the wire

The app host turns its model into a tree of components with keys and
publishes it ([topology.md](topology.md#app-panes)): the whole tree
first, then only the changes, so an update to one row is a change to
that row's node and nothing else. A version 1 manifest is published as a
tree too: one data node keyed `view`. An agentic pane has no tree.

The tree is JSON: a frame with `title`, `status`, `focus` (the focused
node's key), and `root`. A node has `key`, `type`, an optional `title`,
its content, an optional `bind` ([bound streams](#bound-streams)), and
`children`:

| `type` | Content |
| --- | --- |
| `column`, `row`, `scroll` | None |
| `split` | `direction` (`row` or `column`), `sizes` |
| `tabs` | `active`, the index of the page shown |
| `table` | `columns`, and `selected`, the selected row's key; its rows are `item` children |
| `list` | `selected`; its rows are `item` children |
| `log` | Its lines are `item` children; bound to a tap, none |
| `item` | `cells`: one per column, or one |
| `text` | `text` |
| `gauge` | `ratio` (0 to 1) and `label` |
| `chart` | `values` |
| `form` | `fields` (`label`, `value`, `toggle`, `required`) and `focus`, the field being edited |
| `markdown`, `diff` | `text` |
| `code` | `text` and `language` |
| `button` | `label` |
| `input` | `label`, `value`, `placeholder` |
| `select` | `label`, `options`, and `selected`, the index of the option shown |
| `toggle` | `label` and `on` |
| `badge` | `text` and `tone` |
| `progress` | `ratio` (0 to 1) and `label` |
| `status` | `items`, each with `text` and `tone` |
| `transcript` | `thinking` (shown, not folded) and `back` (lines scrolled back from the newest, in a terminal); the conversation comes from its `bind` |
| `composer` | `label`, `value` (lines separated by `\n`), `placeholder`, and `attachments` (their labels) |
| `terminal` | `pane`, the pane's number; `refused`, why it is not shown (left out when it is) |
| `image` | `src`, the path in the package; the host adds `path` (the file it resolved to), `width` and `height` in pixels, or `refused` |
| `web` | `src`, as the app gave it; the host adds `url` (the page's address), `origin` (the app's), `app` (its id), `dir` (the package's `web/` directory, when the page is from there), `allow`, or `refused` |

A row's key is its `row_key` value, or its place when the view has
none. A change is one of these ops, each naming its node by the keys on
the path from the root:

- `frame`: the new `title`, `status`, and `focus`.
- `root`: a whole new root `node` (the root's key changed).
- `props`: a node's own `title`, content (`kind`), and `bind`, not its
  children.
- `children`: a node's child `keys` in their new order, with the new
  ones in full (`added`); children whose key was there before stay as
  they were.

Renderers keep what they hold per key (scroll positions, the selection
last scrolled to) across changes, so an update never resets them.

## Process apps

A process app's own program draws its view tree, in any language. The
manifest names the program and has the `process` capability:

```toml
schema_version = 2
id = "local.counter"
version = "0.1.0"
title = "Counter"
capabilities = ["process", "tmux.topology"]

[process]
command = ["./counter", "--fast"]   # the program and its arguments; no shell
```

- **The program.** A program with a `/` in it is relative to the package
  directory; a bare name is looked up on `PATH`. It runs in the package
  directory, with the app host's environment minus the launch ticket.
- **The capability.** A `[process]` without `process` in
  `capabilities` is refused with that reason, by `--check`, `dream app
  check` and `install`, and when the app is run. `process` is not an API
  method, so it is not part of the grant the app needs; the other
  capabilities are.
- **What the program replaces.** A process app has no `[view]`, `root`,
  `[nodes]`, `[data]`, `[commands]`, `module`, `[agent]`, or
  `[contributes]`; its program does that work.
- **The conversation.** The program speaks the
  [app process protocol](app-protocol.md) on its standard input and
  output: it sends its tree (whole, then keyed patches), gets input as
  events, and calls the API through the host. The Rust SDK
  (`dreamer-sdk`) does this part for Rust apps.
- **Its grant.** The host forwards a call only when its method is among
  the manifest's capabilities, and makes it on the host's own
  connection: with the grant from `dream app run` or `launch`, or, when
  started without a ticket, a connection narrowed to those capabilities.
  The `process` capability does not sandbox the program: it runs with
  your privileges, like any program you start.
- **In a pane.** The tree is drawn and published like any app's, so the
  desktop draws it natively ([app panes](#app-panes)). Every key goes to
  the program except `Ctrl-c`, which quits.
- **Supervision.** The program's I/O never holds up the pane. When it
  exits with status 0 the app ends. When it fails, its last view stays,
  marked `stale:` with why and the last line of its standard error; `r`
  starts it again and `q` quits. The manifest is not reloaded while it
  runs.
- **Preview.** `--preview` (and `dream app preview`) runs the program
  with no daemon and draws its first view; its calls are answered from
  `fixtures/calls.json` ([testing an app](#testing-an-app)), or with an
  error.

A [script app](#script-apps-appts) is the other way to draw a tree from
code: TypeScript run by the host itself, sandboxed, with no `process`
capability.

## Script apps (`app.ts`)

A script app's logic is one TypeScript file, run by the app host in an
embedded JavaScript engine (QuickJS). It returns a view tree, and the host
draws and publishes it like a process app's. There is no Node, no package
install, and no build tool to set up.

```toml
schema_version = 2
id = "local.panes"
version = "0.1.0"
title = "Panes"
capabilities = ["tmux.topology", "actions.submit"]
all_panes = true

[script]
source = "app.ts"    # or a .js file, run as it is
```

```ts
import { app, ui, dreamer } from "dreamer";

interface Pane { id: number; current_command: string }

app({
  state: { filter: "" },
  async load() {
    const t = await dreamer.tmux.topology();
    return { panes: t.panes as Pane[] };
  },
  view(s, data: { panes: Pane[] }) {
    return ui.column({ key: "main", children: [
      ui.input({ key: "f", label: "Filter", value: s.filter, onChange: (v: string) => ({ filter: v }) }),
      ui.table({
        key: "t",
        columns: ["id", "current_command"],
        rows: data.panes.filter((p) => p.current_command.includes(s.filter)),
        onEnter: async (p: Pane) => {
          await dreamer.actions.submit({ action: { kind: { select_pane: { pane: p.id } } } });
        },
      }),
    ]});
  },
});
```

- **The manifest.** A script app has `schema_version = 2` and a
  `[script]` whose `source` is a `.ts` or `.js` file in the package. Like a
  process app it has no `[view]`, `root`, `[nodes]`, `[data]`,
  `[commands]`, `module`, `[agent]`, or `[contributes]`, and it does not
  take the `process` capability. Its `capabilities` and `all_panes` are
  the grant it needs, as for any app.
- **Types are stripped, not checked.** `dream app check`, `dream app
  preview`, and `dream app install` strip the TypeScript types and write
  the JavaScript to `build/app.js` beside the manifest; the host runs that
  file. Types become spaces, so a line and column in the JavaScript are
  the same in `app.ts`, and errors name them: `app.ts:3:1: ...`. Only
  syntax that strips away is accepted: an `enum`, a `namespace` with
  code, a parameter property, or `import x = require(...)` is an error
  where it is. The TypeScript type annotations themselves are not checked;
  `dream app check` checks the uses of the API and the builders instead
  ([types and check](#types-and-dream-app-check)).
- **A build matches its source.** The last line of `build/app.js` records
  which `app.ts` it was built from. The host refuses a build that does
  not match the `app.ts` beside it and says to run `dream app check`. So
  after editing `app.ts`, run `dream app check DIR` before running
  `dreamer-termapp app.toml` directly. An installed package carries its
  build.
- **One file.** The only module an app can import is `dreamer`; its
  `app`, `ui`, and `dreamer` are also globals.
- **The network.** `fetch` reaches only the hosts `[net]` lists, and only
  with the `net.fetch` capability ([`fetch`](#fetch)).

### The app

`app({ ... })` is called once, with:

| Field | Means |
| --- | --- |
| `state` | The starting state, an object |
| `load(ctx)` | Optional, may be async: the app's `data`. Called when the host connects to the daemon and each time it reconnects, on `ctx.reload()`, and every `refresh` ms while connected. A newer load wins over an older one still running. Until the first one ends, the pane says it is loading |
| `refresh` | Optional: reload every this many milliseconds, at least 200 |
| `view(state, data, ctx)` | The view: a node, or `ui.screen(node, { status, focus, title })` for the status line, the focused node's key, and the title (the manifest's otherwise). Called after every change |
| `onKey(key, ctx)` | Optional: a key no component took |
| `onEvent(topic, payload, ctx)` | Optional: an event from a subscription the app made (`events.subscribe`) |

`ctx` holds `state`, `data`, and `live` (whether the host is connected),
`set(partial)` to change the state, and `reload()`.

**Handlers** are functions on nodes. Each gets its value and then `ctx`,
and returns a part of the state to change (an object, merged into the
state) or a promise of one; anything else changes nothing. The view is
drawn again after every handler, and when its promise settles.

| Node | Handler | Called with |
| --- | --- | --- |
| `button` | `onPress` | `ctx` |
| `input` | `onChange`, `onSubmit` (or `onEnter`) | The text, on every edit; on Enter |
| `select` | `onChange` | The option and its index |
| `toggle` | `onChange` | `true` or `false` |
| `table`, `list` | `onSelect`, `onEnter` | The row (the object given in `rows`) and its key, when the selection moves; on Enter |

An error thrown by a handler, by `load`, or by a timer is shown on the
status line as `error: ...` until the next handler or load succeeds; the
app keeps running.

### `dreamer`, `ui`, and the other globals

The globals are `app`, `ui`, `dreamer`, `setTimeout`, `setInterval`,
`clearTimeout`, `clearInterval`, `console` (`log`, `warn`, `error`), and
`fetch` (with `Headers` and `Response`), beside the language's own
built-ins (`JSON`, `Math`, `Date`, `Promise`, ...). There is no file
system, no processes, and no way to load other code.

- **`dreamer`** has one async function for each API method the app's
  capabilities allow, named as in the API: `dreamer.tmux.topology()`,
  `dreamer.actions.submit({ action })`. Each takes the method's
  parameters and resolves to its result, both as the API's JSON
  ([api.md](api.md)). `dreamer.call(method, params)` calls a method by
  name. Every call goes through the host, as a process app's does: a
  method outside the capabilities is refused there without reaching the
  daemon, and the daemon holds the host to the app's grant. A refused or
  failed call rejects with a `dreamer.DreamerError` that has the API's
  `code` and a `message`.
- **`ui`** builds nodes. Each takes an object of the node's fields (see
  [the tree on the wire](#the-tree-on-the-wire)) with an optional `key`
  (its place among its siblings otherwise) and `title`:
  - layout: `column`, `row`, `scroll`, `tabs` (`active`), and `split`
    (`sizes`, `direction`), which also take `(children, fields)`;
  - text: `text`, `markdown`, `code` (`language`), `diff`, and `badge`
    (`tone`), which also take `(text, fields)`;
  - data: `table` (`columns`, `rows`, `rowKey`, `selected`), `list`
    (`rows`, `field`, `rowKey`, `selected`), `log` (`lines`), `gauge`
    and `progress` (`ratio`, `label`), `chart` (`values`), and `status`
    (`items`). A row's key is its `rowKey` field (`id` by default), else
    its place; a table's cells are the row's `columns` fields as text;
  - controls: `button` (`label`), `input` (`label`, `value`,
    `placeholder`), `select` (`label`, `options`, and `value` or
    `selected`), and `toggle` (`label`, `on`);
  - embeds: `terminal` (`pane`, a number or `"%3"`; also
    `ui.terminal(pane, fields)`), `image` (`src`; also
    `ui.image(src, fields)`), and `web` (`src`; also `ui.web(src,
    fields)`). See
    [terminal and image components](#terminal-and-image-components) and
    [web pages](#web-pages-and-web-apps).
- **Timers** run on the app's thread. An interval is at least 10 ms, and
  an app has at most 1000 timers pending.
- **`console`** lines are kept as a process app's standard error is.

### `fetch`

`fetch` sends HTTP and HTTPS requests, only to the hosts the manifest
lists, and only when the app's capabilities include `net.fetch`:

```toml
capabilities = ["net.fetch"]

[script]
source = "app.ts"

[net]
hosts = ["api.example.com", "localhost:8080", "127.0.0.1:3000", "[::1]:8080"]
```

```ts
const r = await fetch("https://api.example.com/v1/items?limit=10", {
  headers: { Authorization: "Bearer " + token },
});
if (!r.ok) throw new Error("items: " + r.status);
const items = await r.json();
```

- **Hosts.** Each entry is a name or an address, with an optional port.
  `api.example.com` allows that host on its scheme's own port (443 for
  `https`, 80 for `http`); `localhost:8080` allows that port only. There
  are no wildcards: `api.example.com` does not allow `example.com` or
  `x.api.example.com`. A name is matched as written (case aside), before
  it is resolved.
- **The capability.** `fetch` needs `net.fetch` in the grant the app
  actually holds: the one the daemon gave its launch, as it answers the
  app host's registration. Declaring it in the manifest puts it in the
  grant `dream app run` and the desktop ask for, so it is shown with the
  app's other capabilities. A launch that grants less makes `fetch`
  reject, saying the app's grant does not include `net.fetch`, whatever
  the manifest says. Until the host has connected and knows the grant,
  `fetch` is refused too. The app host checks the grant and the hosts
  itself: the requests never pass through the daemon. A package with
  `[net]` needs `net.fetch`, and `net.fetch` needs `[net]`; only a script
  app has either.
- **Refused.** A URL that is not `http` or `https`, a host or port the
  manifest does not list, a user name or password in the URL, a method
  other than `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`, or
  `OPTIONS`, a body on a `GET` or `HEAD`, and setting a header that
  belongs to the connection (`Host`, `Content-Length`, `Connection`,
  `Transfer-Encoding`, `Proxy-*`, ...) reject with a `TypeError` whose
  message says why, before any connection is made.
- **Redirects** are followed (at most 20), and each one is checked as the
  first request was: a redirect to a host the manifest does not list is
  refused, and the request fails. A 303, or a 301 or 302 answering a
  `POST`, goes on as a `GET` without its body; `Authorization` and
  `Cookie` are not sent to another origin. `redirect: "error"` fails on a
  redirect instead, and `redirect: "manual"` answers with the redirect.
- **In a preview** (`--preview`, `dream app preview`, and a test's
  replay) there is no launch, so the package's declared capabilities
  stand for the grant, and requests are sent to the same hosts: there
  are no fixtures for `fetch`.
- **What it takes:** a URL (or an object with `url`), and `method`,
  `headers` (an object, `[name, value]` pairs, or `Headers`), `body` (a
  string; `JSON.stringify` an object), and `redirect`. There is no
  `signal`, streaming, `FormData`, or binary body.
- **What it answers:** a `Response` with `status`, `statusText`, `ok`,
  `url` (after redirects), `redirected`, `headers` (names in lowercase;
  `get`, `has`, `forEach`, `entries`), and the body, read whole, through
  `text()` or `json()`, once. The body is read as UTF-8. An HTTP error
  status is a response, not a rejection; a network failure, a refused
  request, or a limit rejects with a `TypeError`.
- **Limits.** A request, with its redirects and its body, may take 30 s;
  past that it fails with `no complete answer within 30000 ms`. A body
  may be 8 MiB, sent or received; a larger response fails with `the
  response is larger than ... bytes`. An app may have 16 requests in
  flight. Requests run on threads of their own, never the app's, so a
  slow server never counts against the 250 ms an entry into the app may
  run; the answer then enters the app like any other, under that limit.
- **The client.** HTTP/1.1 over TLS (rustls), checking certificates
  against the Mozilla root store Dreamer is built with, not the
  system's: a certificate from a private CA is refused. No proxy from the
  environment is used, no cookies are kept, nothing is cached, and no
  compressed encoding is asked for.

### Types and `dream app check`

`dreamer.d.ts` declares everything above for editors: each `dreamer.*`
method with its parameters and result as interfaces (`Dreamer.Pane`,
`Dreamer.TopologyParams`, ...), enums as unions of their names, the `ui`
builders and each one's properties, `app()`, the timers, `console`,
`fetch` (with `Headers` and `Response`), and the module `"dreamer"`. It is generated from the API schema
([api.md](api.md)) and the builders' list, so it matches this version of
Dreamer. Parameters are declared with every field optional, as the API
takes them; results with the fields the API always sends as required.

`dream app types DIR` writes it to `DIR/types/dreamer.d.ts`, and, when
`DIR` has no `tsconfig.json`, one that includes `*.ts` and that file with
only the language's own library (no DOM, as in the app host). Run it
again after updating Dreamer. An editor's TypeScript service then
completes and checks `app.ts`. The `types` directory and `tsconfig.json`
are not used at run time.

`dream app check` (and `dream app install`, which refuses the package)
also checks `app.ts` against the same declarations, in Rust, with no
TypeScript compiler or Node. It reports, each with
`file:line:column`:

- a name after `dreamer.` that is not an API method or group, and a
  `dreamer.call("...")` whose literal method name is not one, with the
  closest name when there is one;
- a property a method's parameters do not have, at any depth (a nested
  record, or a variant of a one-of such as `actions.submit`'s `kind`);
- a `ui.*` builder that does not exist, and a property a builder or
  `ui.screen`'s options do not have;
- a property `app()` does not take;
- a method called by name (`dreamer.jobs.list()`, or `dreamer.call`
  with a literal name) that the manifest's `capabilities` do not cover,
  which the app host would refuse when it runs; a method named at run
  time is still only refused then;
- a value written in place whose kind does not fit: a string, number,
  boolean, array, object, or function where the declaration takes
  another; a string that is not one of an enum's names; a fraction or a
  negative number where the API takes a whole or unsigned number. Items
  of an array literal are checked the same way.

This is a targeted check of these uses, not a type checker. It follows
`app`, `ui`, and `dreamer` as the globals or as imported from
`"dreamer"` (renamed or not), and not once they are shadowed or assigned
to another name. It does not follow a value through a variable, a
function's result, or a cast (`as`), and it does not check the app's
own types, results, or anything else TypeScript would. A `.js` source is
checked the same way.

With `--json`, the check's object has a `problems` array of `file`,
`line`, `column`, and `message`; any problem sets `ok` to false and
makes the first one the `error` when the package has no other. Without
it, each problem is printed as `app.ts:4:34: message`, and the command
fails.

### Keys and focus

The host moves the focus and keeps each control's value, so a control
works whether or not the app listens to it:

- Tab moves the focus through the tables, lists, and controls shown.
- A focused `input` takes letters, digits, and Space (so `q` is a
  letter there), Backspace, and Enter (submit); text from a desktop or a
  paste goes into it as one edit.
- A focused `button` is pressed, and a `toggle` flipped, with Enter or
  Space; a `select` moves with Up and Down, and to its next option with
  Enter or Space.
- A focused `table` or `list` moves its selection with Up and Down (or
  k and j), PageUp, PageDown, Home, and End; Enter is `onEnter`.
- A click from a desktop focuses the node and selects the row or
  option, presses the button, or flips the toggle.
- `q` outside an input quits, and `Ctrl-c` always does. Other keys go to
  `onKey`.

A value the user changed stays until the app gives a different one than
it gave the last time: an input whose `value` the app sets to `""` after
a submit is cleared, and one whose `value` the app never changes keeps
what was typed.

### Limits and failure

The app runs on a thread of its own, never on the pane's drawing and
input, so a slow app cannot hold up the pane or the desktop.

- **Time:** every entry into the app (starting it, a handler, a call's
  answer, a fetch's answer, a timer, an event, drawing the view, with
  the promise reactions each queues) may run 250 ms. Code still running
  then is interrupted. A `fetch` has its own limits ([`fetch`](#fetch)).
- **Memory:** the app's heap may hold 64 MiB, and its stack 1 MiB.
- **Failing.** When code is interrupted, memory runs out, the code does
  not load, `app()` is not called, or `view` throws or returns something
  that is not a node, the app stops. Its last view stays, its status
  line replaced by `stale:` and why; `r` starts it again with a fresh
  engine (its state and memory dropped), and `q` quits.
- **Preview.** `--preview` (and `dream app preview`) runs the app with
  no daemon. With a `fixtures/load.json`, that is its `data` and `load`
  is not called; without one, the pane says it is waiting for the
  daemon. Its calls are answered from `fixtures/calls.json`
  ([testing an app](#testing-an-app)), or with an error.

## Modules

A package may name a Luau module beside its manifest (`module =
"app.luau"`). It runs in the same sandbox as daemon scripts
([scripts.md](scripts.md)): no files, processes, or network, with budgets,
and its `dreamer.*` calls go over the app's own connection with the app's
grant. It may define:

- `transform(rows)`: called with the fetched rows, returns the rows to
  show; use it for computed columns or to filter rows.
- Command handlers: a command with `handler = "name"` calls the module's
  function `name` with `{ selected = <row>, form = <values>, values =
  <input components' values by node key>, event = <what ran it> }`; a
  string it returns is shown in the status line.

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

The runtime looks at the manifest and the module about twice a second. When
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
model and its [view tree](#the-tree-on-the-wire) to the daemon whenever
they change (only the newest model is kept, and a model or tree
superseded while one is being sent is skipped), and the daemon marks
its pane in the topology. The desktop then draws that pane natively
from the published tree instead of its terminal, and sends it the input
made there; see [desktop.md](desktop.md#input-in-app-views). Keys from
the desktop and keys typed in the pane go through the same input
handling, so `q` from either quits the app. A plain tmux client keeps
seeing the text interface.

Besides keys, the desktop sends (each as one `apps.input` event):

| Field | Means |
| --- | --- |
| `text` | Text for the focused input as one edit: text committed by the desktop's text input (IME) or pasted. Line breaks become spaces (a composer keeps them), other control characters are dropped, and a number field keeps only digits, `.`, and `-`. A view with no focused input (a table, a toggle field) ignores it, so pasted text never runs a command |
| `select` | A click: select this row of a table or list, or focus this form field, by index |
| `scroll` | Wheel steps: move a table's or list's selection, positive down |
| `node` | With `select` or `scroll`, in a [view tree](#view-trees): the key of the node clicked or under the pointer. A table, list, or form shown takes the focus first, so the click or wheel steps reach that node; without it they go to the focused node |

Text pasted into the pane's terminal (a bracketed paste, from tmux or a
terminal) is handled the same way as `text`.

An app started without a launch ticket (`dreamer-termapp app.toml` in a
shell) is not registered: the desktop draws its pane as a terminal.
After a daemon restart the pane is drawn as a terminal too, until the
app is relaunched.

## Agentic panes

An agentic pane is an app package bound to an agent provider and a
grant. It is a pane app like any other, so it keeps running without the
desktop and works through a plain `tmux attach`.

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
  and Ctrl-c quits. `q` is a letter here.
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
