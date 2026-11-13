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
| `DREAMER_MIRROR_WORKERS` | Number of mirror owner threads (default: cores, 1 to 4). The pressure test sets 1 so two panes share an owner |
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

`crates/server/tests/pressure.rs` measures this with real tmux and two
panes on one mirror worker. Pane %0 runs `yes`, a subscriber to %0 never
reads, and an 8 MiB paste goes into %0 at the same time. Meanwhile
`echo` round trips are timed into pane %1, from input until the daemon's
mirror shows the output.

| Build | Machine | %1 round trip p95, quiet | %1 round trip p95, under pressure | %0 ingest |
| --- | --- | --- | --- | --- |
| debug, before per-pane lanes | Apple M3 Max (Mac15,11), 36 GiB, macOS 27.2 | 8.5 ms | 19.7 s (waited for the whole paste) | 2.3 MB/s |
| debug, per-pane lanes | same | 8.7 ms | 38.7 ms | 5.1 MB/s |

The test fails if the p95 under pressure exceeds 250 ms or five times the
quiet p95, whichever is larger. These numbers come from a debug-build
test, not the benchmark harness. They show the property, not
performance. Delivering a paste costs about 0.4 MiB/s through tmux's
literal-byte path (`send-keys -H`); that is the next thing to measure.
