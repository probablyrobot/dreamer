# MCP

`dream mcp` is an MCP server on standard input and output, so an agent
with an MCP client (Claude Code, Codex, and others) can use Dreamer's
tools. It uses the official Rust SDK (`rmcp`, pinned in the dependency
baseline), which speaks MCP protocol versions up to 2026-07-28.

```sh
dream mcp                                        # the user's full authority
dream mcp --method terminal.read --pane %3       # only reading pane 3
```

Register it with an MCP client as a stdio server whose command is
`dream mcp` with the options you choose.

## Tools

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

`--method` (repeatable), `--pane %N` (repeatable), and `--all-panes`
narrow the server's connection with `grants.narrow` before any tool runs:
the connection then holds exactly that grant and can never widen it
again. Without them, the server holds the authority of the user who
started it.

The daemon works the same with no MCP client at all.
