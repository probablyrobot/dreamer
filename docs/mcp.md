# MCP

`dream mcp` is an MCP server on standard input and output, so an agent
with an MCP client (Claude Code, Codex, and others) can use Dreamer's
tools. It uses the official Rust SDK (`rmcp`, pinned in the dependency
baseline), which speaks MCP protocol versions up to 2026-07-28.

```sh
dream mcp                                        # the user's full authority
dream mcp --level elsewhere                      # an agent: asks before acting on your panes
dream mcp --method terminal.read --pane %3       # only reading pane 3
```

Register it with an MCP client as a stdio server whose command is
`dream mcp` with the options you choose.

## Terminal tools

These are the tools an agent uses to see and work with terminals. Each
acting tool goes through the act level of the server's grant
([grants.md](grants.md#act-levels-and-approvals)).

| Tool | Does | API |
| --- | --- | --- |
| `terminals_list` | Every pane: session, window, command, directory, title, size, tier, time since its last output, and whether this session created it | `terminal.list` |
| `terminal_read` | A pane's screen as text, or with `lines` the last N lines of history and screen | `terminal.read` |
| `terminal_search` | A regular expression (or plain text) over one pane's or every pane's newest lines, bounded in lines, matches, bytes, and time | `terminal.query` |
| `terminal_watch` | Wait until a pattern appears in a pane's new output, the pane goes quiet for `quiet_ms`, or (`exit`) its program ends, or the timeout (default 30 s, at most 10 min). Returns `matched`, `quiet`, `exited`, `closed`, or `timeout`, the matching line, and a job's exit status | `terminal.watch` |
| `terminal_run` | Run a command (`argv`, or `command` through `/bin/sh -c`) as a job in a new window, or a pane split from `split`, which this session then owns; wait for it (default 60 s) and return its exit status and output. A run that outlasts the wait keeps going | `terminal.run` |
| `terminal_send` | Type `text`, paste `paste`, and press `enter` in a pane, under this session's interaction lease on it. Once a person types into that pane, the session refuses to type there again | `leases.acquire`, `terminal.input` |
| `layout_split` | Split a pane, optionally running an exact program; this session owns the new pane | `actions.submit` |
| `layout_select` | Select a pane or a window | `actions.submit` |
| `layout_rename` | Rename a window or a session | `actions.submit` |
| `layout_move` | Move a pane into another window or a new window of its own | `actions.submit` |
| `layout_zoom` | Toggle a pane's zoom | `actions.submit` |
| `layout_close` | Close a pane or a window; outside what this session created, this always asks | `actions.submit` |

`terminal.watch` and `terminal.run` read the pane's mirror the way taps
do, every 50 ms and only when something changed, so they never hold up
the pane. A pattern in `terminal_watch` matches lines written after the
watch starts (and the line being written, such as a prompt); quiet
counts from the pane's last output, which may be before the watch.
`exit` means the pane's program ended: a job's or run's program, or a
program whose pane tmux reports dead.

At the `elsewhere` level, an action on a pane this session did not create
waits for the user's answer (up to 60 s) before it runs or fails; see
[Act levels and approvals](grants.md#act-levels-and-approvals). Answer
with `dream approvals`.

## Collector tools

Collectors are standing rules in the daemon that turn terminal output into
rows ([collectors.md](collectors.md)). An agent sets one up once and asks
for what it gathered later, instead of re-reading scrollback.

| Tool | Does | API |
| --- | --- | --- |
| `collector_create` | Start a collector: a `preset` (`errors`, `test-failures`, `commands`, `urls`, `paths`) and/or `from`, `when`, `capture`, `keep`, `per_second`, as in a definition. Parts given replace the preset's; the name defaults to the preset's | `collect.create` |
| `collector_query` | A collector's rows, oldest first, filtered by `panes`, exact `fields` values (an object, such as `{"test": "a"}`), `since_s` (the last N seconds), and `after` (a row id); or with `group_by` the count of rows per value, most first | `collect.query` |
| `collector_list` | Each collector's definition, state, counts, followed panes, and last error | `collect.list` |
| `collector_stop` | Stop a collector (its rows stay); with `delete`, remove it and its rows | `collect.stop` |

Collectors only read, so act levels do not govern them: at every level,
`observe` included, the agent may create, query, and stop collectors.
They need `all_panes` unless `from` lists only panes in scope
([collectors.md](collectors.md#grants)). MCP has no way to wake an agent
when a row arrives; an agent asks with `collector_query` and `after` (the
last row id it saw). Dreamer's pi extension does wake its agent
([agents.md](agents.md#dreamers-tools-inside-pi)).

## App tools

An agent builds a pane app ([termapp.md](termapp.md#building-an-app))
with the same steps as `dream app`, then opens it for the user. The
`dreamer-apps` skill describes the loop, the TypeScript API, and the
components.

| Tool | Does | Through |
| --- | --- | --- |
| `app_new` | A working package from a template (`name`; `template`: `table`, `form`, or `chat`; `dir`, default the name) | `dream app new` |
| `app_check` | Build `app.ts` and check the package: `ok`, the grant it needs, and `problems`, each with `file`, `line`, `column`, and `message` | `dream app check --json` |
| `app_preview` | The app drawn from its fixtures: `mode` `text` (default; `size` WxH) returns the screen as text; `mode` `image` returns a PNG as the desktop draws it | `dream app preview --text`; for an image `apps.launch`, `apps.tree`, `apps.capture`, `actions.submit` |
| `app_test` | Replay `fixtures/events.jsonl` and compare each screen: `ok` and what differs; `update` records the screens drawn now | `dream app test` |
| `app_install` | Check, preview, and install the package; the version it replaces is kept. Returns the installed `name` | `dream app install` |
| `app_launch` | Open an installed app (`name`) with the grant it declares: beside the pane the server runs in (`TMUX_PANE`, the chat's pane), beside `beside`, or in a `new_window`. Returns the new `pane`, which this session owns | `apps.launch` |

- **Directories.** `dir` is a package directory inside the apps
  directory: `DREAMER_APPS_ROOT` when set, else the working directory the
  server was started in (the harness's). A relative `dir` is taken from
  there. A path with `..`, or one that leaves the directory through a
  symbolic link, is refused; the tools read and write nothing outside it
  except the installed copy in Dreamer's apps directory.
- **Check problems.** `app_check` returns `ok`, `needs` (the grant the
  manifest asks for), and `problems`, each with `file`, `line`, `column`,
  and a `message` that names the fix where it can (``did you mean
  `table`?``). A syntax error has no entry in `problems`; it is in
  `error`, with its place. The check does not hold the methods `app.ts`
  calls to the manifest's `capabilities`: a call outside them is refused
  when the app runs.
- **Text, then image.** The text preview needs no daemon and works
  everywhere. The image preview needs a running Dreamer window: without
  one it fails at once with `no Dreamer window is running; --png needs
  one`, before opening anything. With one, it opens the preview in a new
  window of its own, waits until the desktop has drawn it, takes the
  image, and closes the window again.
- **Grants.** Checking, previewing as text, and testing need no level.
  Installing needs a grant that may launch apps (`apps.launch`, given
  from `own` up); at `observe` it is refused. Launching is an
  `apps.launch` call the daemon checks: the app's grant must be within
  the server's, beside a pane the server did not create it asks the user
  at `elsewhere` and is refused at `own`, and in a new window it only
  creates ([grants.md](grants.md#agents-and-apps)). An image preview
  launches and closes its own pane, so it needs `own` too.

## Browser tool

| Tool | Does | Through |
| --- | --- | --- |
| `browser_open` | Open an address (`url`: a URL, or as typed into an address bar, such as `example.com` or `localhost:3000`) in Dreamer's [browser](desktop.md#the-browser): a new web pane beside the pane the server runs in, beside `beside`, or in a `new_window`, which this session owns; or, with `pane`, load it in that web pane. Returns the `pane` and the `url`. A new pane waits while the desktop adds [Dreamer Web](desktop.md#dreamer-web) (or offers it), and fails with why, its pane closed, when it is not there and will not be | `web.open`, `web.navigate` |

The page runs in the user's desktop, with no Dreamer API; the agent
reads it with the page tools below, when its grant allows. Not at the
`observe` level; splitting or loading into a pane the session did not
create waits for the user's approval at `elsewhere` and is refused at
`own` ([grants.md](grants.md#act-levels-and-approvals)).

## Page tools

An agent reads and drives the page of a web pane, or of a web app's
pane, as the user's Dreamer window shows it
([desktop.md](desktop.md#agents-and-pages)). Each needs `web.inspect` in
the server's grant, which an agent's grant holds only when given:
`dream mcp --level LEVEL --web-inspect`, or an agent session's grant that
lists it ([grants.md](grants.md#reading-and-driving-pages)).

| Tool | Does | API |
| --- | --- | --- |
| `web_read` | The page's `url`, `title`, and `text`; with `tree`, its accessibility tree as `nodes` (`node` id, `depth`, `role`, `name`, `value`) | `web.read` |
| `web_click` | Click an element named by `node` (from the tree) or `selector` (CSS, its first match) | `web.click` |
| `web_type` | Type `text` into the element named by `node` or `selector` (focused first), else where the page's focus is; `submit` then presses Enter | `web.type` |
| `web_screenshot` | The page as Chromium draws it, as image content | `web.screenshot` |

Clicking and typing act on the pane: refused at `observe`, at `own`
allowed in the web panes the session opened, and at `elsewhere` asked
for elsewhere. The clipboard is never used. With no Dreamer window
running, or one that does not show the pane, the tool fails and says
so.

## Memory tools

Durable memory ([memory.md](memory.md)) lets an agent keep facts across
sessions and harnesses. The context of each call is the server's working
directory (or `cwd`, if given), which picks the project, and the agent
session in `DREAMER_SESSION_ID`, if set.

| Tool | Does | API |
| --- | --- | --- |
| `memory_recall` | Memories matching `query` (none: the newest), best first; optional `scope` and `limit` | `memory.recall` |
| `memory_remember` | Keep `text` with its `kind` (`user`, `feedback`, `project`, `reference`) and `scope` (`user`, `host`, `project`, `session`); optional `title`, `description`, `links` | `memory.remember` |
| `memory_forget` | Delete the memory `id` | `memory.forget` |

Recall and remember are in an agent's grant at every level; forget from
`own` up ([memory.md](memory.md#grants)).

## Other tools

| Tool | Does | API |
| --- | --- | --- |
| `list_panes` | Sessions, windows, and panes | `tmux.topology` |
| `list_runs` | Agent runs here and on connected hosts, with evidence | `agents.runs` |
| `read_pane` | A pane's screen and up to 500 lines of history | `terminal.read` |
| `list_hosts` | The remote hosts Dreamer knows, and whether each is connected | `hosts.list` |
| `check_job` | A job's record: state, exit status, window | `jobs.get` |
| `create_pane` | Split a pane, optionally running an exact program | `actions.submit` |
| `start_job` | Run an exact program as a job in its own window | `jobs.start` |
| `send_input` | Type text into a pane, exactly as given, under the session's interaction lease on it. Once a person types into that pane, the session refuses to type there again | `leases.acquire`, `terminal.input` |
| `move_panes` | Gather panes into one window with a given name, existing or new; one action record per move | `tmux.topology`, `actions.submit` |
| `find_lines` | Which panes' recent lines contain some text, with the lines, here and on connected hosts (`hosts`) | `terminal.query` |
| `check_action` | An action's record and outcome | `actions.get` |
| `cancel_job` | Stop a managed job | `jobs.cancel` |
| `watch_lines` | Install a filter that reports (and optionally marks) lines matching some text in panes, from now on | `filters.manage`, content read |
| `list_filters` | The filters this session may manage, with their counts | `filters.manage` |
| `stop_filter` | Remove a filter an app installed | `filters.manage` |

Each of these tools, and each terminal and collector tool, is one call
to the daemon over an ordinary connection. The
daemon checks the connection's grant and the target exactly as for any
other client ([grants.md](grants.md)), so MCP bypasses nothing. A refused
or failed call comes back as a tool error whose text is the daemon's
message. Results are JSON, as structured content and as text.

## Connected hosts

`list_panes`, `read_pane`, `start_job`, and `check_job` take an optional
`host`, the name of a connected host from `list_hosts`. The call is then
forwarded to that host's daemon as a `hosts.call`, and the result says
which `host` answered. `find_lines` takes `hosts` (`"*"` for every live
one). Each host's result is its own. Running a check on two hosts is two
`start_job` calls and two `check_job` calls, and one can fail while the
other succeeds; nothing spans hosts atomically. A narrowed server needs
`hosts.call` (and `hosts.list`) among its methods to reach other hosts.

## Narrowing

`--method` (repeatable), `--pane %N` (repeatable), `--all-panes`, and
`--level` narrow the server's connection with `grants.narrow` before any
tool runs: the connection then holds exactly that grant and can never
widen it again. Without them, the server holds the authority of the user
who started it.

`--level observe|own|elsewhere` gives the connection an agent's grant:
every pane (unless `--pane` names some), the act level, and, unless
`--method` names others, the methods of the terminal tools. At `observe`
those are `tmux.topology`, `terminal.list`, `terminal.read`,
`terminal.query`, `terminal.watch`, `jobs.get`, `jobs.list`,
`actions.get`, `actions.list`, `agents.runs`, `collect.create`,
`collect.list`, `collect.query`, `memory.recall`, `memory.list`,
`memory.get`, and `memory.remember`; `own` and `elsewhere` add
`terminal.input`, `terminal.run`, `actions.submit`, `jobs.start`,
`jobs.cancel`, `collect.stop`, `memory.update`, `memory.forget`,
`apps.launch`, `apps.tree`, `apps.capture`, `daemon.status`, and
`daemon.version`. `--web-inspect` adds `web.inspect`, for the
[page tools](#page-tools), to whichever of these the grant holds.

The daemon works the same with no MCP client at all.
