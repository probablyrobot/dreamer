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
| `dream status [--json]` | Process, socket, log, clients, version, and tmux endpoint state |
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

`dream app run APP_ID [--method METHOD]... [--all-panes] [--session $N]
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

## Jobs

`dream jobs` lists jobs; `dream jobs start [--cwd DIR] [--name NAME] --
PROGRAM [ARGS...]`, `cancel ID`, and `artifacts ID` manage them. See
[jobs.md](jobs.md).

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
