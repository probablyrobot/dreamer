# Scripts

Scripts are small programs in [Luau](https://luau.org) that run inside
Dreamer: filters, triggers, commands, and the logic of small pane apps.
They are the extension tier between declarative files and supervised
processes ([extensions.md](extensions.md)). See
[ADR 0001](adr/0001-luau-script-runtime.md) for why Luau.

## The sandbox

Each script instance has its own Luau VM:

- Only Luau's safe standard libraries: no `io`, no `os.execute` or
  `os.getenv`, no `require` of files, no native modules.
- Built-in libraries and globals are read-only. A script's own globals are
  private to it.
- The only way out is the `dreamer` module.

## The `dreamer` module

`dreamer` is generated from the API schema
(`crates/api/generated/dreamer.luau`), one function per method:

```lua
local text = dreamer.terminal.read({ pane = 3, history = 0 })
local ok, err = pcall(dreamer.daemon.shutdown)   -- refused outside the grant
```

- Parameters and results are tables following the JSON conventions in
  [api.md](api.md#json-conventions): records have string keys, lists are
  sequences, enums are names, and bytes are base64 strings.
- Numbers are doubles. Integers must be within ±2^53, and larger ones are
  refused rather than rounded. NaN and infinities are refused.
- Every call is performed under the script's grant
  ([grants.md](grants.md)). A refusal, or any other error from the
  daemon, is a Luau error whose message carries the code (for example
  `-32005`); catch it with `pcall`.
- A call is asynchronous: the script yields while it is in flight, and
  other work goes on.

## Budgets

| Budget | Default | On overrun |
| --- | --- | --- |
| Memory per VM | 16 MiB | The call fails with a memory budget error |
| Running time per call into the script | 50 ms | The interrupt hook stops it with a time budget error |
| Each host call | 5 s | The call fails with `-32004` (no answer within the deadline) |

The running-time budget counts only time the script itself runs. Time
spent waiting for host calls does not count, and each host call has its
own deadline instead. After an overrun the VM stays usable. What happens
next (terminate, report, restart) is up to whoever hosts the script.

## Running scripts in the daemon

`dreamerd` runs scripts loaded through the `scripts` service:

| Method | Does |
| --- | --- |
| `scripts.validate` | Compile a script without running it |
| `scripts.load` | Start a script with a `name`, its `source`, a `grant`, the event kinds it receives, and optional `limits` (the user only) |
| `scripts.reload` | Replace a script's source atomically (the user only) |
| `scripts.stop` | Stop and unload it (the user only) |
| `scripts.list` | Each script's state, revision, handled events, errors, restarts, and last error |

On the command line:

```sh
dream scripts load watcher watcher.luau --method events.subscribe \
    --method messages.publish --all-panes --event tmux
dream scripts                       # list
dream scripts reload watcher watcher.luau
dream scripts stop watcher
dream scripts check watcher.luau    # compile only
```

- **Isolation.** Each script runs in its own VM on its own worker thread,
  never on a thread that handles terminals. Its `dreamer.*` calls go
  through its own connection to the daemon, which holds the script's grant
  and the sender identity `script:<name>`
  ([messages.md](messages.md)). A script reaches only what that grant
  allows.
- **Loading.** The source's top level runs once. If it fails (a syntax
  error, an error, a refused call, a budget), the script is not loaded and
  the error is returned. Loaded scripts last until they are stopped or the
  daemon stops.
- **Events.** With `events` (kind prefixes, as in [events.md](events.md)),
  the script's global `on_event(event)` is called for each event, with the
  event record. A gap arrives as an event of kind `events.gap` with `first`
  and `last`. Receiving events needs `events.subscribe` in the grant.
- **Errors.** An error in `on_event` is counted and kept as the last
  error; the script keeps running.
- **Budgets.** A budget overrun terminates the instance (its VM and
  connection end), records a `scripts.failed` event, and starts the script
  again from its source after 0.5 s, doubling per failure up to a minute.
  After 8 in a row it is parked as `failed` until a reload.
- **Reloading.** The new source is checked and started first. Only once it
  has loaded does it replace the running version. A version that does not
  compile or does not load leaves the last-good one running, and the
  reload returns the reason.
