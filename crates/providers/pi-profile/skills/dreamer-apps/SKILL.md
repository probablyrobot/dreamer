---
name: dreamer-apps
description: "Build small apps for the Dreamer terminal: monitors, dashboards, lists, forms, and tools that run in a real tmux pane and are drawn natively by the desktop. Use when the user asks for a pane that shows or watches something (jobs, panes, agent runs, collector rows) or a small tool inside Dreamer. Starts from a TypeScript template, checks it, previews it as text and as an image, tests it from sample data, installs it, and opens it beside the chat."
---

# Building Dreamer apps

A Dreamer app is a package directory: `app.toml` (id, title, and the
capabilities it needs), `app.ts` (its logic, TypeScript), and sample data
in `fixtures/`. The app host runs `app.ts` sandboxed (QuickJS) in a real
tmux pane; the desktop draws its view natively, and a plain `tmux attach`
shows it as a terminal interface. There is no Node, no npm, and no build
tool: `dream app check` strips the types.

Build the app for the user instead of explaining how. The check names
every mistake with `file:line:column`, so let it guide you. For anything
this page does not cover, read "Pane apps" in Dreamer's documentation
(`docs/termapp.md`).

## The loop

Each step is a tool (`app_*`, in `dream mcp` and in Dreamer's pi
extension) and a command. Package directories must be inside the apps
directory: the agent's working directory, or `DREAMER_APPS_ROOT`.

| Step | Tool | Command |
| --- | --- | --- |
| New | `app_new` (`name`, `template`) | `dream app new NAME --template table\|form\|chat [--dir DIR]` |
| Edit | your editor tools | `app.ts`, `fixtures/`, `capabilities` in `app.toml` |
| Check | `app_check` | `dream app check DIR [--json]` |
| Preview as text | `app_preview` (`mode: "text"`) | `dream app preview DIR --text [--size 100x30]` |
| Preview as an image | `app_preview` (`mode: "image"`) | `dream app preview DIR --png FILE` |
| Test | `app_test` (`update`) | `dream app test DIR [--update]` |
| Install | `app_install` | `dream app install DIR` |
| Open | `app_launch` (`name`, `beside`) | `dream app launch NAME [--beside %N]` |

1. **New.** Pick the template closest to the request: `table` (rows from
   the daemon, Enter acts on one), `form` (fields and a button), `chat`
   (a log and an input). It writes a working package, its fixtures,
   `types/dreamer.d.ts`, and the test's expected screens. There is no
   `web` template; for a page, see "Web pages" below.
2. **Edit** `app.ts`. Keep `fixtures/load.json` the same shape as what
   `load()` returns, and add every method you call to `capabilities`.
3. **Check** until `ok` is true (see "Reading the check" below).
4. **Preview as text first**, from the fixtures with no daemon, and read
   it: is everything there, does it fit, are the columns wide enough?
   Then, if a Dreamer window is running, **as an image** to see the
   native drawing: it opens the preview in a window of its own, returns
   the PNG, and closes the window again. Without a window the image
   preview fails with `no Dreamer window is running`; the text preview
   is enough then. Fix what looks wrong and check again.
5. **Test.** `fixtures/events.jsonl` holds one event per line
   (`{"kind":"key","key":"Down"}`, `{"kind":"text","text":"hi"}`); the
   test compares the screen after each with `fixtures/expect/NNN.txt`.
   After an intended change, test with `update` and read the new screens
   before you keep them.
6. **Install.** It checks and previews again, then copies the package to
   the user's apps directory, keeping the version it replaces
   (`dream app rollback NAME`). The installed name is the app's id
   (`local.NAME`) unless you give one.
7. **Open** it with `app_launch`: beside the pane you run in (the chat),
   or `beside` another pane, or `new_window`. Tell the user the pane it
   is in (`%N` from the result). A later install of the same app reloads
   the open pane; you need not launch it again.

### Reading the check

`app_check` (and `dream app check DIR --json`) answers with `ok`,
`problems`, and `needs` (the grant the manifest asks for). Each problem
has `file`, `line`, `column`, and `message`, and the first one is also
in `error`. Fix them in order, then check again: one fix can reveal the
next problem. What the messages mean:

| Message | Fix |
| --- | --- |
| `` `ui.tabel` is not a ui builder; did you mean `table`? `` | Use the builder named; the list is under "Components" |
| `` `colums` is not a property of ui.table; did you mean `columns`? `` | Rename or remove the property |
| `` `selected` of ui.table takes a string or a number, not a boolean `` | Give a value of the kind named |
| `` `dreamer.tmux.topolgy` is not an API method; did you mean `topology`? `` | Use the method named; `types/dreamer.d.ts` lists them all |
| `` `endpont` is not a field of the parameters of dreamer.tmux.topology (TopologyParams); did you mean `endpoint`? `` | Use the record's field, as declared in `types/dreamer.d.ts` |
| `ok: false`, `problems` empty, and `error` such as ``app.ts:23:9: Expected ',', got 'ident'`` | A syntax error, or a manifest problem: `error` says where |

The check does not compare the methods `app.ts` calls with
`capabilities`. A call outside them is refused when the app runs (the
status line shows `dreamer.DreamerError` and why), so keep the two in
step yourself. The check passing also does not mean the screen looks
right: preview it.

Installing and opening need the `own` or `elsewhere` level; at `observe`
they are refused. Opening an app beside a pane you did not create asks
the user first at `elsewhere`, and is refused at `own` (use a pane you
created, or `new_window`). The app's grant is its capabilities, and must
be within yours.

## The TypeScript API

```ts
import { app, ui, dreamer } from "dreamer";

interface Data { panes: Dreamer.Pane[] }

app({
  state: { status: "Enter selects the pane" },
  async load(): Promise<Data> {            // fixtures/load.json in previews and tests
    const t = await dreamer.tmux.topology();
    return { panes: t.panes };
  },
  view(s, data: Data) {
    return ui.screen(
      ui.table({
        key: "panes",
        title: "Panes",
        columns: ["id", "current_command"],
        rows: data.panes,
        onEnter: async (p: Dreamer.Pane) => {
          await dreamer.actions.submit({ action: { kind: { select_pane: { pane: p.id } } } });
          return { status: `selected %${p.id}` };   // merged into the state
        },
      }),
      { status: s.status },
    );
  },
});
```

- **`app({...})`**: `state` (the start), `load(ctx)` (the data; again on
  `ctx.reload()` and every `refresh` ms), `view(state, data, ctx)`,
  `onKey(key, ctx)`, `onEvent(topic, payload, ctx)`. A handler returns a
  part of the state to merge, or a promise of one.
- **`dreamer.*`**: one async function per API method the capabilities
  allow, named as in the API (`dreamer.tmux.topology()`,
  `dreamer.jobs.list({})`), or `dreamer.call(method, params)`. A refused
  call rejects with `dreamer.DreamerError` (`code`, `message`).
- **Types**: `types/dreamer.d.ts` (`dream app types DIR` writes it again
  after an update) declares every method's parameters and results
  (`Dreamer.Pane`, `Dreamer.JobRecord`, ...) and every builder.
- **Globals**: `setTimeout`, `setInterval`, `clearTimeout`,
  `clearInterval`, `console`, `fetch`, and the language's built-ins. No
  files or processes, and no way to load other code.
- **`fetch`** reaches only the hosts the manifest lists, with the
  `net.fetch` capability: `capabilities = ["net.fetch"]` and
  `[net] hosts = ["api.example.com", "localhost:8080"]` (a host alone
  means its scheme's port). Method, headers, a string body; the response
  has `status`, `ok`, `headers`, `text()`, `json()`. Anything else,
  a redirect to an unlisted host included, rejects with a `TypeError`.
  Each request may take 30 s and its body 8 MiB. For anything else, get
  data through `dreamer.*` (jobs, collectors, terminals).
- **Limits**: each entry into the app may run 250 ms; the heap is
  64 MiB. Errors show on the status line; the app keeps running.

## Components (`ui.*`)

Each takes an object with an optional `key` and `title`. Give every
node whose place can change a `key`, so the selection and scroll
position stay with it.

| Kind | Builders and their fields |
| --- | --- |
| Layout | `column`, `row`, `scroll`, `tabs` (`active`), `split` (`sizes`, `direction`): `children`, or `(children, fields)` |
| Data | `table` (`columns`, `rows`, `rowKey`, `selected`, `onSelect`, `onEnter`), `list` (`rows`, `field`, `rowKey`, `selected`, `onSelect`, `onEnter`), `log` (`lines`), `gauge` (`ratio`, `label`), `chart` (`values`) |
| Text | `text`, `markdown`, `code` (`language`), `diff`, `badge` (`tone`): `text`, or `(text, fields)` |
| Input | `button` (`label`, `onPress`), `input` (`label`, `value`, `placeholder`, `onChange`, `onEnter`, `onSubmit`), `select` (`label`, `options`, `value` or `selected`, `onChange`), `toggle` (`label`, `on`, `onChange`) |
| Status | `progress` (`ratio`, `label`), `status` (`items`: `{ text, tone }`), and `ui.screen(node, { status, focus, title })` for the status line |
| Embeds | `terminal` (`pane`: a number or `"%3"`), `image` (`src`), `web` (`src`): also `(pane or src, fields)` |

Tones are `neutral`, `accent`, `success`, `warning`, and `danger`. Tab
moves the focus between tables, lists, and controls; `q` (outside an
input) or Ctrl-c ends the app.

- **`terminal`** shows another pane's screen, live and read only. It
  needs `terminal.read` in `capabilities` and the pane in the grant
  (`all_panes = true` for a pane chosen at run time); otherwise the node
  shows `refused:` and why.
- **`image`** shows a PNG or JPEG from the package, by its path there
  (`img/logo.png`); not an absolute path and not `..`.
- **Web pages.** `web` shows a page drawn by the desktop: a file in the
  package's `web/` directory (`index.html` when `src` is empty), or an
  address on the app's origin or one of its `[web] allow` origins. A
  page from the package gets `window.dreamer.call(method, params)`,
  under the app's grant. A plain terminal shows only the page's
  address, and the text preview shows the node, not the page: preview
  as an image to see it. A whole app can be a page (a web app: `[web]`
  in `app.toml` and no `app.ts`); see "Web pages and web apps" in
  `docs/termapp.md`.

## Rules

- **Least grant.** Ask for the fewest capabilities that do the job. Add
  `all_panes = true` only to read other panes or hear their events.
- **Real methods only.** The check names any that are not.
- **Never** put secrets, tokens, or shell snippets in a package, and
  never edit the user's tmux or SSH configuration.
- **Uninstall** by removing its directory under `dream config-dir`
  `apps/`. A running installed app reloads when its files change; a
  version that does not check leaves the running one in place.
