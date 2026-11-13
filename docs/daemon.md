# dreamerd

`dreamerd` is Dreamer's per-user headless daemon. It drives one tmux server
through control mode, runs typed actions, and keeps the action ledger.
Clients reach it over a Unix socket using the [wire protocol](wire.md).
Stopping it never stops tmux: sessions, windows, panes, and their programs
keep running.

## Commands

| Command | Effect |
| --- | --- |
| `dream ensure-running [-L name \| --tmux-socket-path path] [--tmux PATH]` | Connect to the running daemon, or start one in the background with these tmux options. Concurrent calls start exactly one daemon |
| `dream status [--json]` | Process, uptime, socket, log, connected clients, version, and the tmux endpoint's state. Does not start a daemon |
| `dream version` | Client version and build family, plus the daemon's version if one is running |
| `dream shutdown` | Stop the daemon and report what keeps running. A daemon from another build refuses this client's protocol, so `dream shutdown` sends that daemon SIGTERM instead, using the pid recorded in its startup lock |

`dreamerd` itself takes:

```text
dreamerd [--background] [--tmux PATH] [--tmux-socket NAME | --tmux-socket-path PATH]
dreamerd --version | --self-check
```

Without a socket option, the daemon drives your default tmux server. It
only ever attaches to a server that is already running; it never starts
one. It connects on the first request that needs tmux and reconnects
after the link drops.

## Files

| File | Location | Notes |
| --- | --- | --- |
| Socket `dreamerd.sock` | runtime directory | Mode 0600 |
| Startup lock `dreamerd.lock` | runtime directory | Holds the daemon's pid; locked for its lifetime |
| Log `dreamerd.log` | state directory | Written in background mode; mode 0600 |
| Action ledger `actions.jsonl` | state directory | See [actions.md](actions.md) |

The **runtime directory** is the first of these that is set:

1. `DREAMER_RUNTIME_DIR`
2. `$XDG_RUNTIME_DIR/dreamer`
3. `$TMPDIR/dreamer-<uid>`

Otherwise it is `/tmp/dreamer-<uid>`.

The **state directory** is `DREAMER_STATE_DIR`, then
`$XDG_STATE_HOME/dreamer`, then `~/.local/state/dreamer`.

Both directories are created with mode 0700. The daemon refuses to start if
an existing directory belongs to another user or is open to other users. It
never loosens or takes over such a directory.

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
  Sessions and panes keep running, and the next start reattaches.

## Settings

| Variable | Effect |
| --- | --- |
| `DREAMER_JOURNAL_PANE_BYTES`, `DREAMER_JOURNAL_HOST_BYTES` | Journal retention per pane and per host (defaults 8 MiB and 256 MiB); see [mirrors.md](mirrors.md) |

## Environment for tests

| Variable | Effect |
| --- | --- |
| `DREAMER_RUNTIME_DIR`, `DREAMER_STATE_DIR` | Private directories per test |
| `DREAMER_FORBID_DEFAULT_TMUX` | The daemon refuses to attach to the default tmux server. Test harnesses set it so no test can reach the user's own tmux, even if it forgets to name an isolated socket |
| `DREAMER_TEST_ABORT_AFTER_DISPATCH` | Abort the process right after an action reaches tmux, to test crash recovery |
| `DREAMER_TEST_SLOW_PANE` | `PANE:MICROS`: sleep that long before applying each output chunk of that pane, so it outruns its mirror |
| `DREAMER_MIRROR_WORKERS` | Number of mirror owner threads (default: cores, 1 to 4). The pressure test sets 1 so two panes share an owner |
| `DREAMER_SSH` | The `ssh` program the coordinator runs (default `ssh` from `PATH`). Tests point it at a stand-in so no real SSH configuration is used |
| `DREAMER_TEST_BACKOFF_MS` | The first reconnect delay in milliseconds (default 1000), so reconnect tests run quickly |
| `DREAMER_TEST_SMALL_QUEUES` | Shrink the subscriber and connection queues, so back-pressure tests behave the same on every platform regardless of socket buffer sizes |

## Under pressure

One busy pane must not slow down another (spec 16.3, 16.5):
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
  8 MiB per pane, and offered again every 5 ms. This absorbs bursts without
  a re-capture. Once a pane's held output passes 8 MiB, it has outrun its
  mirror, and the router rebuilds that mirror:
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
