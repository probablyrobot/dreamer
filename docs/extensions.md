# Extensions

An extension is a program `dreamerd` starts, supervises, and restarts. It
talks to the daemon in JSON-RPC 2.0 on its standard input and output, and
reaches everything through the same typed API as every other client,
under the grant you give it.

**An extension is not sandboxed.** It runs as you, with your files,
network, and processes. A separate process keeps its crashes away from
the daemon, and the grant limits what it can ask the daemon. Neither
limits what the program does on its own. Only configure programs you
trust.

## Configuring

`extensions.yaml` in the configuration directory (`dream config-dir`):

```yaml
extensions:
  - name: build-watch                 # letters, digits, - _ . ; also the log's name
    argv: [/usr/local/bin/build-watch, --quiet]   # exact program and arguments, no shell
    cwd: /home/me/src                 # optional
    enabled: true                     # default
    grant:
      methods: [events.subscribe, terminal.read, messages.*]
      panes: [3]                      # panes it may read, follow, type into, or act on
      all_panes: false                # every pane, windows, sessions, hosts
```

The daemon reads the file when it starts, so restart the daemon after
editing it. An invalid file starts nothing and `dream extensions` shows
why. The grant works like an app's (see
[grants.md](grants.md)); an extension never holds the owner grant, and
cannot launch apps or restart extensions.

## Protocol

The extension's **standard output** carries JSON-RPC to the daemon, one
JSON object per line: its own requests (any API method its grant allows,
see [api.md](api.md#json-rpc-endpoint)) and its replies to the daemon's
requests. Its **standard input** carries the daemon's replies, the
daemon's requests, and notifications for its subscriptions. **Standard
error** is its log, appended to `<state dir>/extensions/<name>.log`.

The daemon's requests to an extension (string ids starting with
`dreamer-`):

| Method | When | The extension answers |
| --- | --- | --- |
| `extension.hello` | Right after it starts. Params: `name`, `protocol` (the API version), `daemon_version` | Within 10 s, with `{"version": "..."}` (shown by `dream extensions`) |
| `$/ping` | Every 10 s | Within 10 s, with any result |
| `extension.shutdown` | Before the daemon stops it | Promptly, then exits |

The extension also gets the environment variable `DREAMER_EXTENSION` (its
name). As a sender of messages it is `extension:<name>`, stamped by the
daemon (see [messages.md](messages.md)).

## Writing one in Rust

The `dreamer-sdk` crate speaks the protocol for you. It answers
`extension.hello`, `$/ping`, and `extension.shutdown` on its own task, so
heartbeats are answered while your code is busy. Your code makes typed
calls and reads typed notifications.

The crate is on crates.io. It re-exports `api`, `app_protocol`, and
`view_tree`, so it is the only Dreamer crate your program adds. It runs
on [Tokio](https://tokio.rs):

```toml
[dependencies]
dreamer-sdk = "0.15"
tokio = { version = "1", features = ["macros", "rt"] }
```

The calls and notifications look like this:

```rust
use dreamer_sdk::api;

let mut session = dreamer_sdk::start("my-extension 0.1").await?;
let mut notifications = session.notifications().expect("taken once");
session
    .call(api::Request::EventsSubscribe(api::EventSubscribe::default()))
    .await?;
loop {
    tokio::select! {
        Some(api::Notification::EventsBatch(b)) = notifications.recv() => {
            session.publish("my.topic", format!("{} events", b.events.len()).into_bytes()).await?;
        }
        () = session.stopped() => return Ok(()),
    }
}
```

- `session.hello` holds the extension's name and the daemon's API
  version.
- `session.call` takes any `api::Request` and returns the matching
  `api::Response`, or `Error::Rpc` with the daemon's code and message
  (for example `-32005` outside the grant).
- `session.notifications()` yields `api::Notification` values: events,
  messages, terminal records, topology. If the extension does not read
  them, newer ones are dropped and counted by `session.dropped()`; the
  heartbeat never waits on them.
- `session.stopped()` resolves when the daemon asks the extension to stop
  or its input ends.
- Write logs to standard error. Standard output belongs to the protocol.

A complete example, `crates/sdk/examples/event-summary.rs` in the source
tree, follows the event log and publishes a summary on `summary.events`
after each batch. Build it with
`cargo build -p dreamer-sdk --example event-summary`. Its grant needs
`events.subscribe` and `messages.publish`, and `all_panes` to see events
about no pane.

In other languages, implement the three daemon requests above and speak
JSON-RPC by the conventions in [api.md](api.md#json-rpc-endpoint).

## Supervision

- **Starting.** Each enabled extension starts with the daemon, in its own
  process group.
- **Failing.** Exiting, not answering `extension.hello`, missing a
  heartbeat (a stall), or closing its standard output is a failure. The
  daemon kills the process group and starts the extension again after a
  delay: 0.5 s, doubling per failure up to a minute, with ±20% jitter.
  A minute of running resets the count.
- **Parking.** After 8 failures in a row the extension is parked as
  `failed` with its last error, until `dream extensions restart NAME`.
- **Stopping.** When the daemon stops (or on `extensions.restart`) it asks
  `extension.shutdown` and gives it a second to exit by itself, then sends
  SIGTERM to the process group, and kills it after two more seconds.
- **Never in the way.** An extension's input and output run on their own
  tasks with bounded queues. Terminal input, output, and every other client
  keep flowing while an extension is slow, stalled, or restarting.

## Inspecting

`dream extensions` lists each extension's state (`starting`, `running`,
`restarting`, `failed`, `disabled`, `stopped`), pid, version, restarts,
consecutive failures, last error, and log path; `--json` prints the
`extensions.list` record. `dream extensions restart NAME` (the user only)
stops it if it runs and starts it now, including one parked as failed.
