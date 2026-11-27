# dream

`dream` is Dreamer's command-line interface. Use it to list and read
panes, act on them, manage the daemon, connect remote hosts, and build
pane apps, from a shell or a script. It works without the desktop app:
commands that touch tmux go through the daemon, `dreamerd`
([daemon.md](daemon.md)), which is started on demand.

```sh
dream ls                           # sessions, windows, and panes
dream read %3 --history 200        # a pane's text
dream act split %3 -h -- htop      # a new pane beside %3, running htop
dream act send %3 'make test' --enter
dream status                       # is the daemon running, and what does it drive
dream doctor                       # when something is wrong
```

`dream` with no arguments prints every command. `dream --version` prints
the version. Most commands that print take `--json`.

**Which tmux server.** A command that starts `dreamerd` takes the tmux
options the new daemon should use: `-L name` (or `--tmux-socket name`) or
`--tmux-socket-path PATH` for the server, and `--tmux PATH` for the tmux
program. Without them, the daemon drives your default tmux server, with
`tmux` from `PATH` or `DREAMER_TMUX`. If a running daemon drives a
different tmux server, the command refuses and suggests `dream shutdown`.

## Panes and actions

These commands start `dreamerd` if it is not running, except
`dream actions`, which needs a running daemon.

| Command | Effect |
| --- | --- |
| `dream ls [--json]` | Sessions (`$N`), windows (`@N`), and panes (`%N`), with sizes, programs, process IDs, and pane generations. `*` marks the active window or pane |
| `dream read %PANE [--history N] [--json]` | A pane's text from the daemon's [mirror](mirrors.md): up to N lines of scrollback, then the screen |
| `dream mirrors [--json]` | Every pane's mirror: state, tier, size, and journal |
| `dream tier %PANE hot\|warm\|cold` | Set a pane's [tier](mirrors.md#tiers) |
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
| `dream status --perf [--json]` | The same, then performance diagnostics: stage timings, traffic counters, and queue depths |
| `dream diagnostics on\|off\|reset [--json]` | Turn performance diagnostics on or off, or zero them; see [daemon.md](daemon.md#diagnostics) |
| `dream version` | Client and daemon versions and build family |
| `dream shutdown` | Stop `dreamerd`. tmux keeps running |
| `dream doctor` | Diagnostics: directories and their permissions, socket, startup lock, ledger summary, daemon, and tmux link. Never starts the daemon |
| `dream service install [--no-start] [tmux options]` | Install an optional per-user service (systemd on Linux, a launch agent on macOS) that keeps `dreamerd` running from login; `--no-start` only writes its definition |
| `dream service status` | Whether the service is installed and running |
| `dream service uninstall [--no-stop]` | Unload the service (unless `--no-stop`) and remove its definition |

`status`, `diagnostics`, and `shutdown` need a running daemon and never
start one. See [daemon.md](daemon.md) for files, directories, lifecycle,
and [running as a service](daemon.md#running-as-a-service).

## Remote hosts

| Command | Effect |
| --- | --- |
| `dream hosts connect NAME DESTINATION [--dream PATH] [--remote-tmux PATH] [--remote-socket NAME \| --remote-socket-path PATH]` | The local daemon connects to the daemon on `DESTINATION` over SSH and keeps the connection |
| `dream hosts connect PROFILE` | Connect to a profile from `hosts.yaml` ([remote.md](remote.md#profiles)). `--auto-reconnect` or `--manual` overrides its policy (also for a destination given directly) |
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
| `dream app new NAME [--template table\|form\|chat] [--dir DIR]` | Write a working package to start from: `app.toml`, `app.ts`, fixtures, the editor declarations, and its test's expected screens ([termapp.md](termapp.md#starting-from-a-template)) |
| `dream app check PATH [--json]` | Validate a package (its directory or `app.toml`) and say what grant it needs; for a script app, also report type-level mistakes in its `app.ts` ([termapp.md](termapp.md#types-and-dream-app-check)) |
| `dream app types DIR` | Write `types/dreamer.d.ts` into a package, and a `tsconfig.json` that uses it when there is none, for editors |
| `dream app preview PATH [--text] [--size WxH]` | Draw it once from `fixtures/`, with no daemon, as text (80x24 by default) |
| `dream app preview PATH --open [--session $N]` | Open a live preview in a new tmux window, drawn natively by the desktop; it reloads when the package changes ([termapp.md](termapp.md#live-preview)) |
| `dream app preview PATH --png FILE` | Write the desktop's drawing of the app's preview pane as a PNG (for a web app, its page as Chromium draws it), opening a preview first when none is open; fails with `no Dreamer window is running; --png needs one` when no desktop is running. With `--level observe\|own\|elsewhere` it takes the image as an agent at that level: its connection narrows itself to the agent's grant, opens a preview pane of its own, and closes it afterwards |
| `dream app test PATH [--update] [--size WxH]` | Replay `fixtures/events.jsonl` with no daemon and compare the screen after each event with `fixtures/expect/`; `--update` records them ([termapp.md](termapp.md#testing-an-app)) |
| `dream app install PATH [--name NAME]` | Check, preview, and copy it into `<config dir>/apps/`; the version it replaces is kept |
| `dream app rollback NAME` | Put the kept version back (and keep the one it replaces) |
| `dream app list` | Installed packages, and which have a kept version |
| `dream app launch NAME [--session $N \| --beside %N] [--request]` | Run an installed package in a new tmux window (or in a pane split from `%N`, side by side) with the grant it declares; `--request` prints the `apps.launch` parameters as JSON instead, for a client that launches on its own connection |

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
[--all-panes] [--web-inspect]` serves Dreamer's tools to an MCP client on
standard input and output. See [mcp.md](mcp.md).

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

## Web panes

`dream web open URL [--session $N|NAME | --pane %N] [--json]` opens a web
page in a new tmux pane: a placeholder pane that Dreamer's desktop fills
with the page ([desktop.md](desktop.md#web-panes)). With `--pane` the
pane is split side by side; with `--session` the page gets a new window
there; with neither, the pane the command runs in (`TMUX_PANE`) is
split. It prints the new pane (`%N`). `http`, `https`, `file`, and `data`
URLs and `about:blank` are accepted, and so is an address as typed into a
browser's address bar (below); anything else is refused. Plain tmux
clients see the placeholder's text: the address. While Dreamer's desktop
is adding [Dreamer Web](desktop.md#dreamer-web), or offers it in the new
pane, the command waits until it is there; it fails, and the pane is
closed, if Dreamer Web is turned off, its install fails, or the pane is
closed first. `dream browser` does the same.

`dream browser ADDRESS [--pane %N | --session $N|NAME] [--json]` opens
the address in Dreamer's [browser](desktop.md#the-browser): a new web pane
beside the pane the command runs in (`TMUX_PANE`), or a new window in
`--session`. With `--pane %N` it loads the address in that web pane
instead, keeping its back and forward history. The address is a URL, or
as typed into the address bar: `example.com` is `https://example.com`,
`localhost:3000` (or anything with a port, or a loopback address) is
`http:`, and an absolute path is a `file:` URL. A search is not an
address: there is no search engine. It prints the pane (`%N`).

`dream browser clear` empties the browser's profile on this machine:
its cookies, logins, storage, and cache. While Dreamer's desktop has it
open (after its first web pane), the profile is emptied when the desktop
next starts its web panes, and the command says so.

### Dreamer Web

The desktop draws web pages with Chromium, which is not part of the
Dreamer package: it is a separate component, Dreamer Web, about 150 MB to
download and 330 MB installed on macOS, 160 MB and 380 MB on Linux
([desktop.md](desktop.md#dreamer-web)). macOS on Apple Silicon and Linux
on x86-64 only so far. On macOS it is a zip, on Linux a gzipped tarball.

| Command | Does |
| --- | --- |
| `dream web install` | Uses a copy already on this machine in place, if there is one ([desktop.md](desktop.md#dreamer-web)); otherwise downloads the Dreamer Web archive of this Dreamer's version, checks it against the `.sha256` published beside it, and installs it. It downloads from `DREAMER_WEB_URL` if set, else `web.url` in `dreamer.yaml`, else the GitHub release |
| `dream web install URL` | Downloads from `URL` (`https://`, `http://`, or `file://`): the archive's address, or a directory's that holds it; its `.sha256` must be beside it. Does not look for a copy on this machine |
| `dream web install ARCHIVE` | Installs from an archive you already have (a release asset, or one `cargo xtask package` built). Checked against `ARCHIVE.sha256` if that file exists |
| `dream web install DIR` | Installs from an unpacked component directory (`cargo xtask web bundle` builds one) |
| `dream web status` | Says which Dreamer Web is installed and where, and, for a copy used in place, where that copy is; fails, saying why and how to add it, when it is missing or is another version's |
| `dream web remove` | Removes it (for a copy used in place, only the link to it). Web panes then offer to add it again |

The desktop adds the component with the same installer, from a web
pane, the first-start offer, or the palette. Before installing, it checks
that the component is the one this Dreamer needs (its `component.json`
names the same Dreamer version and CEF release), on macOS that the
framework and each helper app pass `codesign --verify --strict`, and that
the helper loads the framework (or `libcef.so`) from where it sits; on
Linux that also finds every system library Chromium needs, or names the
missing one. It unpacks next to the installed copy and swaps it
in only when all of that passed; on any failure the installed copy stays
as it was. One install runs at a time per data directory; a second one
says so and stops. It goes in `web/` of Dreamer's data directory
(`DREAMER_DATA_DIR`, then `$XDG_DATA_HOME/dreamer`, then
`~/.local/share/dreamer`). Web panes waiting for it draw their pages once
it is installed; there is no need to restart Dreamer.

On a terminal, the download's progress counts up on one line. A
download that fails says from where, why (curl's message), and how to
point Dreamer at another place: `DREAMER_WEB_URL`, `web.url`, or a zip
you have.

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
that Dreamer installs in its data directory. Each takes `--harness pi`,
the only harness Dreamer installs. See
[agents.md](agents.md#pi-installed-by-dreamer).

`dream agents pi [ARGS...]` starts pi the way Dreamer runs it: with
Dreamer's pi profile, checked first and restored to its last loading
version if it no longer loads. `dream agents profile
init|status|log|diff|revert|commit|check` show and manage that profile
and its history. See [agents.md](agents.md#dreamers-pi-profile).

`dream agents providers [--json]` shows whether Claude Code and Codex are
ready to run on your own login. `dream agents ask [--provider P]
[--method M]... [--pane %N]... [--all-panes] [--no-tools] [--max-turns N]
[--timeout SECONDS] [--allow-metered] -- PROMPT` runs a prompt on one of
them with Dreamer's tools. See [providers.md](providers.md).

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

A job is a program that runs in a tmux window of its own and keeps
running when the daemon stops. `dream jobs [--json]` lists jobs;
`dream jobs start [--cwd DIR] [--name NAME] -- PROGRAM [ARGS...]`,
`cancel ID`, and `artifacts ID` manage them. See [jobs.md](jobs.md).

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

## Memory

`dream memory recall [QUERY...] [--scope SCOPE] [--limit N]` prints the
memories that match, best first, with their text. `dream memory remember
--kind KIND --scope SCOPE [--title T] [--description D] [--link L]...
TEXT...` keeps a new one and prints its id and file (`-` as the text reads
standard input). `list [--scope SCOPE] [--kind KIND]`, `get ID`, `update
ID [options]`, and `forget ID` do the rest. Every command takes `--json`,
`--cwd DIR` (default: the current directory, which picks the project), and
`--session ID` (default: `$DREAMER_SESSION_ID`). SCOPE is `user`, `host`,
`project`, or `session`; KIND is `user`, `feedback`, `project`, or
`reference`. `dream memory import [--scope SCOPE]` copies the memory files
into the provider in force (`sqlite` or `kosmonaut`), and `dream memory export [--scope
SCOPE]` copies the other way; both print what they copied. See
[memory.md](memory.md).

## Scripts

`dream scripts [--json]` lists loaded scripts; `dream scripts load NAME FILE
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

| Command | What it does |
| --- | --- |
| `dream config-dir` | Print the configuration directory |
| `dream workspace list` | Workspace files, and whether each is valid |
| `dream workspace check NAME\|PATH` | Validate a workspace and show its windows and panes |
| `dream workspace up NAME\|PATH [-L socket] [--tmux PATH]` | Create the workspace's tmux session, and print the `tmux attach` command for it |

These work on workspace files and tmux directly, without the daemon. See
[workspaces.md](workspaces.md).

## Configuration and agent skills

| Command | What it does |
| --- | --- |
| `dream config check` | Validate every configuration file. Exits 1 on a problem, naming the file, key or line, and what is wrong. Also reports each agent harness: whether its program is there, and its login state |
| `dream config schema [--json]` | Every key of `dreamer.yaml`: kind and range, default, meaning, example, and whether it applies live |
| `dream themes` | Every theme, built in and yours, with a swatch and the one in force |
| `dream skills` (or `dream skills list`) | The agent skills that ship with Dreamer |
| `dream skills show NAME` | Print a skill |
| `dream skills install [--claude] [--codex] [--opencode]` | Write the skills to `~/.claude/skills/`, `~/.codex/skills/`, and opencode's `$XDG_CONFIG_HOME/opencode/skills/` (`~/.config/opencode/skills/` without it); all three by default: `dreamer-config` (configuring Dreamer), `dreamer-manage` (installing, updating, and operating it), and `dreamer-apps` (building pane apps). Dreamer's pi profile has them already |

See [settings.md](settings.md#live-and-checked). The same skills, with
the MCP server, come as a plugin for Claude Code and Codex: see
[MCP](mcp.md#the-dreamer-plugin).

## Updating

| Command | What it does |
| --- | --- |
| `dream update` | Install the latest release over this one, in the same place |
| `dream update --check` | Say whether a newer release exists |
| `dream update --version V` | Install release `V`, an older one too |

`dream update` runs the release's own installer, so it applies to
installations made by the install script. It leaves one made by `apt`,
`dnf`, or `pacman` alone and says which package to get. See
[Installing Dreamer](install.md#update).
