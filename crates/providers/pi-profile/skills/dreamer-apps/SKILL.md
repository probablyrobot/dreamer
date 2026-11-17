---
name: dreamer-apps
description: "Build small terminal apps for the Dreamer terminal: monitors, dashboards, lists, forms, and agentic panes that run in a real tmux pane. Use when the user asks for a pane that shows or watches something (jobs, panes, agent runs, filter matches), or a small tool inside Dreamer. Starts from a template (TypeScript) or writes a declarative package, checks it, previews it as text and tests it from sample data, installs it, and launches it."
---

# Building Dreamer apps

A Dreamer app is a package directory: an `app.toml`, sample data in
`fixtures/`, and either an `app.ts` (TypeScript using `ui.*` and
`dreamer.*`, run sandboxed by the app host) or a declarative view with an
optional Luau module. It runs as `dreamer-termapp` in a real tmux pane,
so it keeps running with no window open and works through a plain
`tmux attach`. There is no Node and no build tool.

Build apps for the user instead of explaining how. For a component, key,
or setting this page does not cover, read "Pane apps" in Dreamer's
documentation (`docs/termapp.md`). `dream app check` says exactly what is
wrong with a package, so let it guide you.

## The loop

1. **New:** `dream app new NAME --template table|form|chat [--dir DIR]`
   writes a working package: `app.toml`, `app.ts`, `fixtures/load.json`
   (what `load()` returns), `fixtures/events.jsonl`, `fixtures/calls.json`
   (answers to `dreamer.*` calls in tests), `types/dreamer.d.ts`, and the
   test's expected screens. There is no web template yet.
2. **Edit** `app.ts`, the fixtures, and the capabilities in `app.toml`.
3. **Check:** `dream app check DIR`. It reports every problem with
   `file:line:column`, and what the app's grant will be (`needs …`). Fix
   and repeat until it says `ok`.
4. **Preview:** `dream app preview DIR --text [--size 100x30]` draws the
   app once from its fixtures, with no daemon. Read the drawing and fix
   what looks wrong. When a Dreamer window is running:
   `dream app preview DIR --open` shows the app beside the user, drawn
   natively from its fixtures, and reloads it on every save (its status
   line says `not built: …` when a save does not build); and
   `dream app preview DIR --png FILE` writes the desktop's drawing as an
   image you can look at. Without a desktop, `--png` fails with
   `no Dreamer window is running`; use `--text` then.
5. **Test:** `dream app test DIR` replays `fixtures/events.jsonl` (one
   event per line: `{"kind":"key","key":"Down"}`, `{"kind":"text","text":"hi"}`)
   and compares the screen after each with `fixtures/expect/NNN.txt`. On
   a difference it prints the rows that differ. When the change is
   intended, `dream app test DIR --update` records the screens again;
   read them before you keep them.
6. **Install:** `dream app install DIR`. This checks and previews again,
   then copies the package to the user's apps directory. The version it
   replaces is kept: `dream app rollback NAME` brings it back. A package
   that fails changes nothing.
7. **Launch:** `dream app launch NAME` runs it in a new tmux window with
   exactly the grant it declares. Tell the user the pane id it prints.

An installed app that is running reloads itself when its files change. A
version that does not validate leaves the running one in place.

## A monitor, complete

```toml
schema_version = 1
id = "local.jobs"                 # dotted lowercase, unique
version = "0.1.0"
title = "Jobs"
capabilities = ["jobs.list"]      # every method it calls
all_panes = true                  # to hear events about other panes (here: jobs')

[data.jobs]
method = "jobs.list"              # a query, never a mutation
path = "jobs"                     # where the rows are in the result
events = ["jobs.state"]           # fetch again on these events
# refresh_ms = 2000               # or poll (at least 200)

[view]
type = "table"                    # table, list, log, text, gauge, chart, form, agent
source = "jobs"
row_key = "id"                    # keeps the selection on its row
columns = ["argv", "state"]
```

With `fixtures/jobs.json`:

```json
{"jobs": [{"id": "a", "argv": ["make", "test"], "state": "running"}]}
```

## Rules

- **Methods.** Use only real API methods; `dream app check` names any
  that are not. Data sources may only query. Changes come from
  `[commands.*]` bound to keys, and each method there must be in
  `capabilities` too.
- **Least grant.** Ask for the fewest capabilities that do the job. Add
  `all_panes = true` only to read other panes or hear events about them.
- **Agentic panes.** `view.type = "agent"` with an `[agent]` table makes
  an inspector, monitor, filter, or operator pane. These run a model on
  the user's own Claude Code or Codex login. Inspector, monitor, and
  filter panes only read. Never add `terminal.input` or actions to them.
- **Never** put secrets, tokens, or shell snippets in a package. Never
  edit the user's tmux or SSH configuration.
- **Ending.** An app pane ends with `q` or Ctrl-c, or when its tmux pane
  is closed. Uninstall it by removing its directory under `dream config-dir`
  `apps/`.
