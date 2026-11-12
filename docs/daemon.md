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

## Environment for tests

| Variable | Effect |
| --- | --- |
| `DREAMER_RUNTIME_DIR`, `DREAMER_STATE_DIR` | Private directories per test |
| `DREAMER_FORBID_DEFAULT_TMUX` | The daemon refuses to attach to the default tmux server. Test harnesses set it so no test can reach the user's own tmux, even if it forgets to name an isolated socket |
| `DREAMER_TEST_ABORT_AFTER_DISPATCH` | Abort the process right after an action reaches tmux, to test crash recovery |
| `DREAMER_TEST_SMALL_QUEUES` | Shrink the subscriber and connection queues, so back-pressure tests behave the same on every platform regardless of socket buffer sizes |
