# dream

`dream` is Dreamer's command-line interface. It works without a GUI: every
command that touches tmux goes through `dreamerd`'s typed API, and
`dreamerd` is started on demand.

## tmux through the daemon

These commands start `dreamerd` if it is not running. They take the tmux
options a newly started daemon should use: `-L name` or
`--tmux-socket-path PATH` for the server, and `--tmux PATH` for the
binary. If a running daemon drives a different tmux server, they refuse
and suggest `dream shutdown`.

| Command | Effect |
| --- | --- |
| `dream ls [--json]` | Sessions (`$N`), windows (`@N`), and panes (`%N`), with sizes, programs, process IDs, and pane generations. `*` marks the active window or pane |
| `dream act <action> … [--key K] [--json]` | Run one typed action and print its observed outcome |
| `dream actions [ID] [--limit N] [--json]` | The action ledger, newest first, or one action |

Actions:

```text
dream act split  %PANE [-h|-v] [--percent N | --cells N] [--cwd DIR] [-- PROGRAM ARGS...]
dream act resize %PANE [--width N] [--height N]
dream act select %PANE | @WINDOW
dream act zoom   %PANE
dream act rename @WINDOW | $SESSION NAME
dream act send   %PANE TEXT [--enter] [--generation G]
dream act keys   %PANE KEY... [--generation G]
dream act kill   %PANE | @WINDOW
```

- `split -h` places the new pane beside the target; `-v`, the default,
  places it below.
- A program after `--` runs as an exact argument vector, not through a
  shell.
- `send` delivers the text byte for byte. `--enter` appends a carriage
  return.
- `keys` takes tmux key names (`Enter`, `C-c`, `Up`).
- `--generation G` refuses the input if the pane's process has been replaced
  since `dream ls` showed generation `G`.
- `--key K` makes the action idempotent: repeating it with the same key
  returns the first result instead of running again.

The exit status is 0 when the action completed, 1 when it failed or was
refused (the output says why), and 3 when the outcome is unknown. See
[actions.md](actions.md).

## Daemon

| Command | Effect |
| --- | --- |
| `dream ensure-running [tmux options]` | Start `dreamerd` unless it is running |
| `dream status [--json]` | Process, socket, log, clients, heap in use, version, and tmux endpoint state |
| `dream version` | Client and daemon versions and build family |
| `dream shutdown` | Stop `dreamerd`. tmux keeps running |
| `dream doctor` | Diagnostics: directories and their permissions, socket, startup lock, ledger summary, daemon, and tmux link. Never starts the daemon |

See [daemon.md](daemon.md) for files, directories, and lifecycle.

## Remote hosts

| Command | Effect |
| --- | --- |
| `dream hosts connect NAME DESTINATION [--dream PATH] [--remote-tmux PATH] [--remote-socket NAME \| --remote-socket-path PATH]` | The local daemon connects to the daemon on `DESTINATION` over SSH and keeps the connection |
| `dream hosts connect PROFILE` | Connect to a profile from `hosts.yaml`. `--auto-reconnect` or `--manual` overrides its policy (also for a destination given directly) |
| `dream hosts verify NAME [--wait]` | Run your own `ssh` to the host in this terminal so you can answer its questions (host key, passphrase), then connect the coordinator. See [remote.md](remote.md#first-connection) |
| `dream hosts install NAME [--artifacts DIR] [--yes] [--wait]` | Install this version's server on the host after showing the plan; see [remote.md](remote.md#installing-a-server) |
| `dream hosts upgrade NAME [--artifacts DIR] [--yes] [--allow-older] [--wait]` | Install, restart the host's daemon on the new version (tmux keeps running), and reconnect; see [remote.md](remote.md#upgrading-a-server) |
| `dream hosts reconnect NAME` | Connect again now, also after a failure that is not retried automatically |
| `dream hosts list [--json]` | Every host, its state, the remote daemon, and why a host is not live |
| `dream hosts call NAME METHOD [JSON]` | Forward one typed call, for example `dream hosts call devbox tmux.topology`, and print the remote result as JSON |
| `dream hosts disconnect NAME` | Close the SSH connection. The remote daemon and its tmux keep running |

See [remote.md](remote.md) for what runs where.

## Apps

Building an app package ([termapp.md](termapp.md#building-an-app)):

| Command | Does |
| --- | --- |
| `dream app check PATH [--json]` | Validate a package (its directory or `app.toml`) and say what grant it needs |
| `dream app preview PATH [--size WxH]` | Draw it once from `fixtures/`, with no daemon, as text |
| `dream app install PATH [--name NAME]` | Check, preview, and copy it into `<config dir>/apps/`; the version it replaces is kept |
| `dream app rollback NAME` | Put the kept version back (and keep the one it replaces) |
| `dream app list` | Installed packages, and which have a kept version |
| `dream app launch NAME [--session $N]` | Run an installed package in a new tmux window with the grant it declares |

`dream app run APP_ID [--method METHOD]... [--all-panes | --pane %N...] [--session $N]
[--ttl-ms N] [--name NAME] -- PROGRAM [ARGS...]` starts a program in a new
tmux window as a registered app. The program gets a single-use ticket, and
once it registers it may call only the listed methods, on its own pane
unless `--all-panes` is given. See [grants.md](grants.md). An app that
should exchange messages needs `--method 'messages.*'` (see
[messages.md](messages.md)).

## Bridge

`dream bridge --stdio [tmux options]` carries the daemon's protocol over
standard input and output. It is the fixed command a coordinator runs on a
remote host through `ssh -T`; you do not normally run it yourself.

- It connects to the user's daemon socket, starting `dreamerd` with the
  given tmux options if nothing answers.
- It copies bytes both ways and never parses or rewrites them, so the
  handshake (protocol version, build family) happens between the two real
  peers ([wire.md](wire.md)).
- Standard output carries protocol bytes only. Every diagnostic goes to
  standard error, including "could not start dreamerd".
- It ends when its standard input ends (after giving the daemon two seconds
  to finish) or when the daemon closes the connection. A daemon it started
  is detached from it: closing the bridge or the SSH connection leaves the
  daemon, tmux, and every pane running.

## Extensions

`dream extensions [--json]` lists the supervised extensions from
`extensions.yaml` and their state; `dream extensions restart NAME` starts
one again now. See [extensions.md](extensions.md).

## MCP

`dream mcp [--level observe|own|elsewhere] [--method M]... [--pane %N]...
[--all-panes]` serves Dreamer's tools to an MCP client on standard input
and output. See [mcp.md](mcp.md).

## Harness drawer

`dream drawer toggle|show|hide|status|new` works the session's
[harness drawer](desktop.md#the-harness-drawer) from a shell, so a tmux
key of your own can run it (for example `bind j run-shell "dream drawer
toggle --pane '#{pane_id}'"` in your own tmux configuration).

| Command | Does |
| --- | --- |
| `toggle` | Hidden or in another window: show it here and focus it. Shown but not focused: focus it. Focused: hide it |
| `show` | Show it here and focus it, starting the harness if there is none |
| `hide` | Move it to the holding session; focus goes back where it was |
| `status` | Whether the session has one, its pane, where it is, and the pane you came from |
| `new --yes` | End the running harness and start a fresh one, shown. Without `--yes` it refuses while a drawer exists |

`--session $N` (or a session name), `--window @N`, or `--pane %N` names
the session and the window to show it in. Without them, the pane the
command runs in (`TMUX_PANE`) does; run in the drawer itself, it means
the drawer's session. `--json` prints the daemon's answer as data.

## Approvals

`dream approvals [--json]` lists the actions agents are waiting to have
approved. `dream approvals allow ID` allows one, `dream approvals allow
ID --pane` allows it and later actions in the same panes (closing still
asks), and `dream approvals deny ID` refuses it. See
[grants.md](grants.md#act-levels-and-approvals).

## Agent runs

`dream agents runs [--host NAME]... [--json]` lists Claude Code and Codex
runs here and on connected hosts. `dream agents hooks [--write FILE]`
prints, or adds to a settings file, the Claude Code hooks that report run
state. `dream hook claude` is the hook itself. See
[processes.md](processes.md).

`dream agents status [--json]`, `dream agents update [--version X.Y.Z]`,
and `dream agents rollback` show, install or update, and roll back the pi
that Dreamer installs in its data directory. See
[agents.md](agents.md#pi-installed-by-dreamer).

`dream agents pi [ARGS...]` starts pi the way Dreamer runs it: with
Dreamer's pi profile, checked first and restored to its last loading
version if it no longer loads. `dream agents profile
init|status|log|diff|revert|commit|check` show and manage that profile
and its history. See [agents.md](agents.md#dreamers-pi-profile).

`dream agents providers` shows whether Claude Code and Codex are ready
to run on your own login, and `dream agents ask [options] -- PROMPT` runs
a prompt on one of them with Dreamer's tools. See
[providers.md](providers.md).

`dream agents sessions` works with agent sessions in the daemon (see
[agents.md](agents.md#sessions)):

| Command | What it does |
| --- | --- |
| `dream agents sessions [--json]` | List sessions, open and closed |
| `dream agents sessions create [--harness H] [--cwd DIR] [--model M] [--name N] [--json]` | Start a session; the directory defaults to the current one |
| `dream agents sessions send ID TEXT [--wait]` | Send a prompt; with `--wait`, print the turn's events until it ends |
| `dream agents sessions events ID [--after N] [--follow] [--json]` | Print the transcript; with `--follow`, keep printing new events |
| `dream agents sessions approve ID APPROVAL [--option O]`, `deny ID APPROVAL` | Answer an approval request |
| `dream agents sessions cancel ID`, `resume ID`, `close ID` | Stop the running turn, start a detached session's harness again, or close the session |

`dream agents delegate --to claude|codex [--cwd DIR] [--timeout SECONDS]
[--json] [--result FILE] -- PROMPT` hands one task to Claude Code or Codex,
headless on its own login, and prints its answer (with `--json`, the
result as JSON). It is what pi's `claude_code` and `codex` tools run. See
[agents.md](agents.md#delegation-to-claude-code-and-codex).

## Jobs

`dream jobs` lists jobs; `dream jobs start [--cwd DIR] [--name NAME] --
PROGRAM [ARGS...]`, `cancel ID`, and `artifacts ID` manage them. See
[jobs.md](jobs.md).

## Collectors

`dream collect create -f FILE` starts a collector from a YAML or JSON
definition; `dream collect create NAME` takes the same from options:

| Part | Options |
| --- | --- |
| Panes (one) | `--all`, `--session NAME`, `--window @N`, `--pane %N` (repeat for more), `--command REGEX` |
| Trigger (one) | `--line REGEX` [`--ignore-case`], `--command-end`, `--idle MS`, `--bell`, `--exit`, `--every SECONDS` |
| Capture | `--field NAME=TEMPLATE` (repeat), `--json-line`, `--screen` or `--lines N`, `--before N`, `--after N` |
| Bounds | `--keep-rows N`, `--keep-for DURATION` (`30s`, `10m`, `2h`, `7d`), `--per-second N` |

`dream collect create --preset PRESET [NAME] [options]` starts one of the
shipped presets (`errors`, `test-failures`, `commands`, `urls`, `paths`),
named after the preset unless NAME is given. Each part given in options
(panes, trigger, capture, bounds) replaces the preset's part; see
[Presets](collectors.md#presets).

`dream collect list [--json]` shows each collector, its state, and its
counts. `dream collect query NAME [--pane %N]... [--field NAME=VALUE]...
[--since DURATION] [--after ID] [--limit N] [--group-by FIELD] [--json]`
prints rows (or counts with `--group-by`). `dream collect stop NAME
[--delete]` stops one, and with `--delete` removes its rows. See
[collectors.md](collectors.md).

## Scripts

`dream scripts` lists loaded scripts; `dream scripts load NAME FILE
[--method M]... [--pane %N]... [--all-panes] [--event KIND]... [--call-ms N]`,
`reload NAME FILE`, `stop NAME`, and `check FILE` manage them. See
[scripts.md](scripts.md).

## JSON-RPC

`dream rpc [tmux options]` speaks JSON-RPC 2.0 on standard input and
output, one JSON object per line, through the daemon's JSON-RPC socket
(see [api.md](api.md#json-rpc-endpoint)). It starts `dreamerd` if nothing
answers. Scripts in any language can drive the daemon with it:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"tmux.topology"}' | dream rpc
```

It ends like the bridge: when its standard input ends (after two seconds
for the last replies) or when the daemon closes the connection. Keep its
input open to receive notifications from subscriptions.

## Workspaces

`dream config-dir`, `dream workspace list | check | up` work on workspace
files directly; see [workspaces.md](workspaces.md).

## Configuration and agent skills

| Command | What it does |
| --- | --- |
| `dream config check` | Validate every configuration file. Exits 1 on a problem, naming the file, key or line, and what is wrong. Also reports each agent harness: whether its program is there, and its login state |
| `dream config schema [--json]` | Every key of `dreamer.yaml`: kind and range, default, meaning, example, and whether it applies live |
| `dream themes` | Every theme, built in and yours, with a swatch and the one in force |
| `dream skills` | The agent skills that ship with Dreamer |
| `dream skills show NAME` | Print a skill |
| `dream skills install [--claude] [--codex]` | Write the skills to `~/.claude/skills/` and `~/.codex/skills/` (both by default): `dreamer-config` (configuring Dreamer) and `dreamer-apps` (building pane apps). Dreamer's pi profile has them already |

See [settings.md](settings.md#live-and-checked).
