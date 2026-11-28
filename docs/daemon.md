# dreamerd

`dreamerd` is Dreamer's per-user headless daemon. It drives one tmux server
through control mode, runs typed actions, and keeps the action ledger.
Clients reach it over a Unix socket using the [wire protocol](wire.md).
Stopping it never stops tmux: sessions, windows, panes, and their programs
keep running.

You rarely start it yourself: the desktop app and `dream` start it when
they need it. To see what it is doing:

```sh
dream status     # process, socket, log, clients, tmux link
dream doctor     # directories, permissions, lock, ledger; never starts the daemon
dream shutdown   # stop it; tmux keeps running
```

## Commands

| Command | Effect |
| --- | --- |
| `dream ensure-running [-L name \| --tmux-socket-path path] [--tmux PATH]` | Connect to the running daemon, or start one in the background with these tmux options (`--tmux-socket name` is the same as `-L name`). Concurrent calls start exactly one daemon |
| `dream status [--json]` | Process, uptime, socket, log, connected clients, heap in use, version, and the tmux endpoint's state. Does not start a daemon |
| `dream status --perf [--json]` | The same, then performance diagnostics: stage timings, traffic counters, and queue depths (see [Diagnostics](#diagnostics)) |
| `dream diagnostics on\|off\|reset [--json]` | Turn performance diagnostics on or off, or zero them. They are off by default |
| `dream version` | Client version and build family, plus the daemon's version if one is running |
| `dream shutdown` | Stop the daemon and report what keeps running. A daemon from another build refuses this client's protocol, so `dream shutdown` sends that daemon SIGTERM instead, using the pid recorded in its startup lock |
| `dream doctor` | The runtime and state directories and their permissions, the socket, the startup lock, a summary of the action ledger, and, if a daemon runs, its tmux link. Never starts the daemon |

`dreamerd` itself takes:

```text
dreamerd [--background] [--tmux PATH] [--tmux-socket NAME | --tmux-socket-path PATH]
dreamerd --version | --self-check
```

Without a socket option, the daemon drives **Dreamer's own tmux server**,
`tmux -L dreamer`, so the tmux you run in a terminal stays out of it.
When that server is not running, or has no sessions, the daemon starts it
with one session in your home folder; it reads your tmux configuration as
tmux does, or what `tmux.config` names. `tmux -L dreamer attach` (or
`ls`) reaches Dreamer's sessions from any terminal.

To drive another server, give `-L NAME` (`-L default` is your default tmux
server, the one plain `tmux` uses) or `--tmux-socket-path PATH`, or set
`tmux.server` / `tmux.server_path` in `dreamer.yaml`
([settings.md](settings.md#tmux-server)); an option comes first. The
daemon starts only its own server: any other it only ever attaches to
once it is running. `dream status` names the server in use. It connects on
the first request that needs tmux and reconnects after the link drops.

### Control clients

The daemon drives tmux through a main control client, attached to one
session. Every command goes through it. It chooses that session each time
it connects:

1. never a session of Dreamer's own (names starting with `_dreamer_`, such
   as the drawer holding session) while any other session exists;
2. then a session a client is attached to;
3. then the most recently active session;
4. then the lowest session id.

When that session is killed, tmux ends the main client, and the daemon
connects again on the next request and chooses again.

tmux sends a control client output only for panes in its own session, so
the daemon also attaches one control client to every other session, and
stops it when the session goes. These clients attach with `ignore-size`,
so they never size a window. They are not `read-only`: tmux 3.8 sends a
read-only control client no pane output. How the mirrors take each pane's
output from exactly one client is in
[mirrors.md](mirrors.md#every-session-streams).

What you see in tmux: every session counts as attached while the daemon
runs (`tmux ls` says `(attached)`, `#{session_attached}` counts the
daemon's clients), so `destroy-unattached` never fires and hooks on
`client-attached` run for them. `tmux list-clients` shows one
`control-mode` client per session.

The extra clients cost little with a few sessions. tmux does some work
for every client on each pane's output, so the cost grows with the number
of sessions; `cargo xtask bench busy --sessions S` measures it.

### PATH

tmux, harness CLIs (`claude`, `codex`), jobs, and extensions given by
name are looked up on the daemon's `PATH`. A daemon started from Finder
or the Dock inherits launchd's `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`),
which has none of them. So when the inherited `PATH` holds nothing beyond
those four directories, the daemon asks your login shell once at startup
(`$SHELL -l -c`, with empty input and a 3-second limit) and takes only
its `PATH`. If the shell fails or takes too long, it appends whichever of
`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`, and `~/.cargo/bin`
exist. The daemon then restarts itself in place (same pid) with that
`PATH`, so everything it starts inherits it. Any other inherited `PATH`
is used as it is. `--tmux` and `DREAMER_TMUX` still name tmux exactly.
The log records the `PATH` in force at startup.

## Files

| File | Location | Notes |
| --- | --- | --- |
| Socket `dreamerd.sock` | runtime directory | Mode 0600; the native protocol ([wire.md](wire.md)) |
| Socket `dreamerd.rpc.sock` | runtime directory | Mode 0600; JSON-RPC 2.0 ([api.md](api.md#json-rpc-endpoint)) |
| Startup lock `dreamerd.lock` | runtime directory | Holds the daemon's pid; locked for its lifetime |
| Log `dreamerd.log` | state directory | Written in background mode; mode 0600 |
| Action ledger `actions.jsonl` | state directory | See [actions.md](actions.md) |
| Extension logs `extensions/<name>.log` | state directory | Each extension's standard error, appended; mode 0600. See [extensions.md](extensions.md) |
| Memory database `memory.sqlite` | data directory | With `memory.provider: sqlite`, the default (or where `memory.sqlite.path` says). See [memory.md](memory.md#sqlite) |
| Host and session memory `memory/host/`, `memory/sessions/<id>/` | state directory | Markdown files and a `MEMORY.md` index, with `memory.provider: files`; read once into the database when `sqlite` takes over. See [memory.md](memory.md#moving-from-files) |

The **runtime directory** is the first of these that is set:

1. `DREAMER_RUNTIME_DIR`
2. `$XDG_RUNTIME_DIR/dreamer`
3. `$TMPDIR/dreamer-<uid>`

Otherwise it is `/tmp/dreamer-<uid>`.

The **state directory** is `DREAMER_STATE_DIR`, then
`$XDG_STATE_HOME/dreamer`, then `~/.local/state/dreamer`.

The **data directory**, where Dreamer installs pi
([agents.md](agents.md#pi-installed-by-dreamer)) and Dreamer Web
([desktop.md](desktop.md#dreamer-web)), is `DREAMER_DATA_DIR`,
then `$XDG_DATA_HOME/dreamer`, then `~/.local/share/dreamer`.

The runtime and state directories are created with mode 0700. The daemon
refuses to start if an existing one belongs to another user or is open to
other users. It never loosens or takes over such a directory.

Socket paths are limited to 103 bytes on macOS and 107 on Linux. A runtime
directory whose `dreamerd.rpc.sock` would be longer is refused, with that
reason, before anything binds.

**When the daemon cannot start,** whoever started it hears why:

- `dream` and the desktop start it in the background and read its
  standard error during startup.
- If it stops before serving, its reason becomes the error: an open or
  foreign directory, a socket path that is too long, or a missing
  `dreamerd` beside the app.
- `dream` prints the reason. The desktop's tab strip shows it and tries
  again every 5 s, so fixing the cause is enough.

## Lifecycle

- **One instance:** the daemon takes the startup lock before it touches the
  socket. A start that finds the lock held exits with status 3, so racing
  starts leave one daemon. Holding the lock proves that any socket file left
  behind is stale.
- **Detaching:** `--background` puts the daemon in a new session with no
  controlling terminal. Closing the terminal or SSH session that started it
  sends it no hangup, and SIGHUP is ignored in any case.
  - A shell job leads its own process group and cannot start a session, so
    in that case the daemon hands over to a copy of itself.
- **Logging:** background mode logs to the file and the foreground logs to
  standard error. The daemon writes nothing to standard output after it
  starts, and nothing but protocol frames to the socket.
- **Stopping:** SIGTERM, SIGINT, or `dream shutdown` stop the daemon, which
  then removes its socket and releases the lock. Actions left unfinished by
  a crash are settled at the next start (see [actions.md](actions.md)).

## Running as a service

The daemon needs no service: the desktop, `dream`, and the SSH bridge
start it on demand, detached from whatever started it. An optional
per-user service keeps it running from login, and through logout where
the system ends a user's processes:

| Command | Effect |
| --- | --- |
| `dream service install [tmux options]` | Write the definition, stop a daemon started on demand (tmux keeps running), and load it. The commands are printed before they run |
| `dream service install --no-start` | Write the definition only |
| `dream service status` | Whether it is installed, and whether the service manager has it running |
| `dream service uninstall [--no-stop]` | Unload it (unless `--no-stop`) and remove exactly the file `install` wrote |

| Platform | Definition | Loaded with |
| --- | --- | --- |
| Linux | `~/.config/systemd/user/dreamerd.service` (or under `$XDG_CONFIG_HOME`) | `systemctl --user daemon-reload`, `systemctl --user enable --now dreamerd.service` |
| macOS | `~/Library/LaunchAgents/dev.dreamer.dreamerd.plist`, logging to `~/Library/Logs/dreamer/dreamerd.log` | `launchctl bootstrap gui/<uid> <plist>` |

Both run `dreamerd` in the foreground, as the `dreamerd` next to `dream`,
with the tmux options given, and restart it if it fails. Nothing else is
written. Without systemd, `install` says so; the daemon still starts on
demand.

### Survival policy

- **SSH disconnect:** a daemon started by `dream bridge --stdio` is
  detached into its own session. Ending the SSH connection sends it no
  hangup, and its tmux and programs keep running.
- **Logout on Linux:** with systemd-logind's `KillUserProcesses=yes`, which
  is the default on some distributions, logging out ends every process of
  the user, including a detached daemon and the tmux server. To keep work
  running after the last session ends, enable lingering
  (`loginctl enable-linger`) and install the service. With
  `KillUserProcesses=no` nothing is needed.
- **macOS:** a launch agent runs in the user's GUI session, from login to
  logout. A daemon started over SSH lives until it is stopped or the machine
  restarts.
- **Daemon stop:** stopping the daemon, by any means, never stops tmux.
  Sessions and panes keep running, and the next start reattaches. That
  includes [jobs](jobs.md), which still record how they end. Supervised
  [extensions](extensions.md) and loaded [scripts](scripts.md) and
  [filters](filters.md) belong to the daemon: a graceful stop asks
  extensions to stop and ends scripts and filters; a crash ends them with
  it.

## Settings

| Variable | Effect |
| --- | --- |
| `DREAMER_JOURNAL_PANE_BYTES`, `DREAMER_JOURNAL_HOST_BYTES` | Journal retention per pane and per host, in bytes of memory (defaults 8 MiB and 256 MiB); see [mirrors.md](mirrors.md) |
| `DREAMER_DIAGNOSTICS` | `1`: performance diagnostics on from the start, as after `dream diagnostics on` |
| `DREAMER_DAEMON` | The `dreamerd` that `dream` and the desktop start (default: the one beside the running program) |

## Environment for tests

| Variable | Effect |
| --- | --- |
| `DREAMER_RUNTIME_DIR`, `DREAMER_STATE_DIR`, `DREAMER_DATA_DIR` | Private directories per test |
| `DREAMER_PI_LATEST_URL`, `DREAMER_PI_DOWNLOAD_URL` | Where pi releases are found; tests point them at `file://` stand-ins so nothing is downloaded |
| `DREAMER_FORBID_DEFAULT_TMUX` | The daemon refuses the default tmux server, and Dreamer's own (`-L dreamer`) unless `TMUX_TMPDIR` is set. Test harnesses set it so no test can reach the user's own tmux, or their Dreamer's, even if it forgets to name an isolated socket |
| `DREAMER_TEST_ABORT_AFTER_DISPATCH` | Abort the process right after an action reaches tmux, to test crash recovery |
| `DREAMER_TEST_PASTE_TOTAL` | What all unfinished pastes may hold together, in bytes (default 128 MiB), to test the limit |
| `DREAMER_TEST_SLOW_PANE` | `PANE:MICROS`: sleep that long before applying each output chunk of that pane, so it outruns its mirror |
| `DREAMER_MIRROR_WORKERS` | Number of mirror owner threads (default: cores, 1 to 4). The pressure test sets 1 so two panes share an owner |
| `DREAMER_SSH` | The `ssh` program the coordinator runs (default `ssh` from `PATH`). Tests point it at a stand-in so no real SSH configuration is used |
| `DREAMER_SERVER_ARTIFACTS` | A directory of server archives for `hosts.install` to use, with no download (default: `share/dreamer/server` beside the daemon, then the downloaded ones, then this version's release; see [remote.md](remote.md#where-the-archive-comes-from)) |
| `DREAMER_SERVER_URL` | Where `hosts.install` downloads server archives from instead of this version's release: the address of a directory holding them and their `.sha256` files |
| `DREAMER_TEST_BUILD_FAMILY` | Report this build family instead of the real one, to test incompatible peers |
| `DREAMER_TEST_BACKOFF_MS` | The first reconnect delay in milliseconds (default 1000), so reconnect tests run quickly |
| `DREAMER_TEST_SMALL_QUEUES` | Shrink the subscriber and connection queues, so back-pressure tests behave the same on every platform regardless of socket buffer sizes |

## Under pressure

One busy pane must not slow down another:

- **Input lanes.** Input is delivered one event at a time per pane, so a
  paste is never interleaved with other input to the same pane. Each pane
  has its own lane: a large paste into one pane never holds up typing in
  another.
- **Stalled subscribers.** A subscriber that stops reading is ended as
  `lagged` when its queue fills. The pane's owner never waits for it (see
  [mirrors.md](mirrors.md)).
- **A pane that outruns its mirror.** The router reads every pane's
  output from the one tmux connection, and it never waits on a single
  pane. Output the pane's worker has no room for is held in order, up to
  8 MiB of memory per pane (counted as the journal counts it; see
  [mirrors.md](mirrors.md)). Held output goes to the worker as soon as its
  lane has room: whenever more of that pane's output arrives, and every
  5 ms when none does. This absorbs bursts without a re-capture. Once a
  pane's held output passes 8 MiB, it has outrun its mirror, and the router
  rebuilds that mirror:
  1. It pauses the pane in tmux and captures it fresh. Output between the
     overflow and the pause is part of that capture, so nothing is lost
     silently.
  2. Output still queued from before is dropped as stale. Every message
     carries the pane's attach number.
  3. The new mirror's journal starts past a gap, so no old resume point
     can be spliced onto it.
  4. Subscribers end as `reattached` and start again from a checkpoint.
  5. `terminal.mirrors` counts it in `reattaches`.
- **Urgent lane.** Each worker has two lanes. Output goes in order on a
  bounded lane. A new attach, input encoding, reads, subscriptions, and
  statistics go on an urgent lane that the worker drains first, so they
  never wait behind a pane's backlog. Input is therefore encoded against
  the modes the mirror has applied so far. Under a backlog those can trail
  what the program has already written.

`crates/server/tests/pressure.rs` measures this with real tmux and two
panes on one mirror worker. Pane %0 runs `yes`, a subscriber to %0 never
reads, and an 8 MiB paste goes into %0 at the same time. Meanwhile
`echo` round trips are timed into pane %1, from input until the daemon's
mirror shows the output.

| Build | Machine | %1 round trip p95, quiet | %1 round trip p95, under pressure | %0 ingest |
| --- | --- | --- | --- | --- |
| debug, before per-pane lanes | Apple M3 Max (Mac15,11), 36 GiB, macOS 27.2 | 8.5 ms | 19.7 s (waited for the whole paste) | 2.3 MB/s |
| debug, per-pane lanes | same | 8.7 ms | 38.7 ms | 5.1 MB/s |

A second test, `a_pane_that_outruns_its_mirror_is_reattached_without_stalling_others`,
puts %0 and %1 on separate workers. It makes %0's output slow to apply
(`DREAMER_TEST_SLOW_PANE=0:2000`, 2 ms per chunk) and floods %0. Only a
router that waits on %0 could then delay %1. The test requires:
- %0 is re-attached;
- %1's round trips stay within bounds;
- %0's mirror catches up;
- a follower of %0 continues from a fresh checkpoint.

| Build | Machine | %1 round trip p95, quiet | %1 round trip p95 while %0 is re-attached | Re-attaches |
| --- | --- | --- | --- | --- |
| debug, re-attach on a full lane (first version) | Apple M3 Max (Mac15,11), 36 GiB, macOS 27.2 | 8.7 ms | 163 ms (255.8 ms on Linux CI) | 6 |
| debug, 8 MiB held per pane before re-attaching | same | 8.2 to 8.8 ms | 11.0 ms (three runs) | 1 |

Each re-attach's fresh capture shares the tmux command channel with %1's
input. Holding a backlog first makes re-attaches rarer, and that is
where most of the improvement comes from.

Both tests fail if the p95 under pressure exceeds 250 ms or five times the
quiet p95, whichever is larger. These numbers come from a debug-build
test, not the benchmark harness. They show the property, not
performance. Delivering a paste costs about 0.4 MiB/s through tmux's
literal-byte path (`send-keys -H`); that is the next thing to measure.

## Diagnostics

The daemon and the replica can time each stage of the terminal path and
count the traffic through it. This is off by default and costs nothing
then: each hook reads one flag and takes no clock reading. While on, a
stage costs two clock readings and a few atomic adds into a fixed
histogram, with no lock, no allocation, and nothing per byte. Only
durations and sizes are kept, never terminal content.

```sh
dream diagnostics on          # or start dreamerd with DREAMER_DIAGNOSTICS=1
dream status --perf [--json]  # timings, counters, and queue depths
dream diagnostics reset       # zero them; on or off stays as it was
dream diagnostics off
```

The API is `daemon.diagnostics` and `daemon.set_diagnostics`.

**Stages,** each timed per output chunk (or per input request) in path
order:

| Stage | Where | From | To |
| --- | --- | --- | --- |
| `input` | daemon | an input request arrives | its bytes are sent to tmux: the pane's input lane and the encoding against the canonical engine |
| `tmux` | daemon | input sent to a pane | the next read of that pane's output. tmux, the pty, and the program are all in it |
| `decode` | daemon | one read of tmux's control output returns | it is decoded into notifications |
| `route` | daemon | output decoded | the pane's VT owner starts on it: the router, any held backlog, and the owner's queue |
| `parse` | daemon | the canonical VT parse of one chunk starts | it ends |
| `publish` | daemon | a parsed record is handed to subscribers | it is in the journal |
| `transfer` | daemon | a record is handed to a subscriber | its batch reaches the client connection's writer |
| `replica_parse` | client | a received batch is applied to the replica | it is applied |
| `frame` | client | a frame is extracted from the replica | it is extracted |

Between `transfer` and `replica_parse`, the socket write, the kernel, and
the client's decoding are not timed. Glyph shaping, GPU submission, and
presentation are the desktop's and are not timed here. Percentiles come
from histogram buckets and are within 6 %.

**Counters** cover the time since diagnostics were turned on or reset:
- bytes read from tmux;
- bytes parsed, the records applied, and the parser's throughput while it
  works;
- bytes and batches sent to clients;
- subscriptions started from a checkpoint (and the checkpoints' bytes) or
  by replay;
- subscriptions ended as lagged: dropped subscriber events, each reported
  to its subscriber;
- mirrors rebuilt because a pane's output outran them;
- bytes of Cold panes' output dropped.

**Queue depths** are read when asked, on or off: tmux events waiting for
the router, output held for a full owner queue, attaches running and
waiting, each VT owner's queue, and the fullest subscriber queue.

**Views** are listed when asked, on or off: each view subscription, the
feed it is on (`stream` or `frames`), whether the daemon switches it by
rate or a client holds it, its switches, and its pane's output rate
([mirrors.md](mirrors.md#screen-frames)).
