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

Each tool is one call to the daemon over an ordinary connection. The
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
`actions.get`, `actions.list`, and `agents.runs`; `own` and `elsewhere`
add `terminal.input`, `terminal.run`, `actions.submit`, `jobs.start`, and
`jobs.cancel`.

The daemon works the same with no MCP client at all.
