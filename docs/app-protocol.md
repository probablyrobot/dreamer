# The app process protocol

A process app is a pane app whose own program draws its view. The app
host (`dreamer-termapp`) starts the program, and the two talk over the
program's standard input and output. This page defines that conversation
precisely enough to write an app in any language. For the package and how
it runs in a pane, see [termapp.md](termapp.md#process-apps); for writing
one in Rust, see [the Rust SDK](#the-rust-sdk).

## Framing

- Each message is one JSON object on one line, ended by `\n`, in UTF-8.
  Empty lines are ignored.
- A line is at most 16 MiB. The host stops a program that writes a longer
  one.
- Every message has a `type`. Fields marked optional may be left out.
- The program's standard output carries only messages. Its standard
  error is its log: the host keeps the last 20 lines and shows the last
  one if the program fails.

The protocol version is **1**. The host sets `DREAMER_APP_PROTOCOL=1` in
the program's environment.

## The handshake

1. The host sends `hello` first.
2. The program answers with its own `hello`, naming the version it
   speaks, before anything else.
3. A program that names another version, or sends anything else first,
   is stopped, and the pane says why.

Host to app:

```json
{"type":"hello","protocol":1,"app":{"id":"local.counter","version":"0.1.0","title":"Counter"},"methods":["tmux.topology"],"live":true}
```

| Field | Means |
| --- | --- |
| `protocol` | The host's protocol version |
| `app` | `id`, `version`, and `title` from the manifest |
| `methods` | The methods the app may call: its manifest's capabilities, without `process` (`service.*` stands for a whole service) |
| `live` | Whether the host is connected to the daemon now |

App to host:

```json
{"type":"hello","protocol":1,"name":"counter 0.1"}
```

`name` is optional, for logs.

## The view: app to host

The view is a tree of components, as described in
[termapp.md](termapp.md#the-tree-on-the-wire): a frame with `title`,
`status`, an optional `focus` (the key of the node that takes keys), and
`root`. The program sends it whole, then sends changes:

```json
{"type":"view","tree":{"title":"Counter","status":"+ add  q quit","focus":"main","root":{"key":"main","type":"column","children":[{"key":"count","type":"text","text":"0"}]}}}
{"type":"patch","ops":[{"op":"props","path":["main","count"],"title":null,"kind":{"type":"text","text":"1"}}]}
```

- `view` replaces the tree the host holds.
- `patch` applies `ops`, in order, to the tree the host holds. The ops
  are those of [the tree on the wire](termapp.md#the-tree-on-the-wire):
  `frame`, `root`, `props`, and `children`, each naming its node by the
  keys on the path from the root.
- Keys are present and unique among siblings. A `view` that breaks this
  stops the program.
- A `patch` that does not apply (a path that is not there, or a result
  that breaks the key rule) is not applied. The host answers with
  `resync`, and the program sends the whole tree next:

  ```json
  {"type":"resync","reason":"no node `main/count`"}
  ```

Send a new tree, or a patch, whenever the state changes. The host draws
the newest in the pane and publishes it to desktops. A tree can be sent
any time after the handshake. Until the first one arrives, the pane says
it is starting.

## Input: host to app

Input from the pane's terminal and from desktops arrives as `event`,
tagged by `kind`:

```json
{"type":"event","event":{"kind":"key","key":"Down"}}
{"type":"event","event":{"kind":"text","text":"pasted text"}}
{"type":"event","event":{"kind":"select","node":"panes","index":3}}
{"type":"event","event":{"kind":"scroll","node":"panes","steps":-2}}
```

| `kind` | Fields | Means |
| --- | --- | --- |
| `key` | `key` | A key by name: a printable character (`a`, `+`), `Enter`, `Esc`, `Tab`, `Backspace`, `Delete`, `Left`, `Right`, `Up`, `Down`, `Home`, `End`, `PageUp`, `PageDown`, `F1` to `F12`, or `Ctrl-a` to `Ctrl-z` |
| `text` | `text` | Text for the focused input as one edit: committed by a desktop's text input, or pasted |
| `select` | `index`, optional `node` | A click on row or field `index` of node `node` (of the focused node when left out) |
| `scroll` | `steps`, optional `node` | Wheel steps over node `node`; positive is down |
| `component` | `node`, `action`, optional `value` | A component's own event, such as a button pressed. Reserved: the current host sends none yet |

The app owns its focus and selection: the host does not move them. Every
key goes to the app except `Ctrl-c`, which the host keeps to quit. Ignore
kinds you do not know.

## API calls

The app calls the daemon's API through the host. Methods and their
parameters and results are those of the JSON API ([api.md](api.md)).

App to host:

```json
{"type":"call","id":1,"method":"tmux.topology","params":{}}
```

Host to app:

```json
{"type":"result","id":1,"result":{"sessions":[],"windows":[],"panes":[]}}
{"type":"result","id":2,"error":{"code":-32005,"message":"`daemon.status` is not in this app's capabilities"}}
```

- `id` is the app's own number; the answer carries it back. `params` may
  be left out for `{}`.
- Calls run concurrently. Answers may come in any order, between other
  messages.
- A call is answered with `error` when:
  - its method is not among the manifest's capabilities, or is one of
    the host's own (`apps.register`, `apps.publish`,
    `apps.publish_tree`, `grants.narrow`): code `-32005`, without
    reaching the daemon;
  - the host is not connected to the daemon: code `-32004`;
  - the daemon refuses it: the daemon's code and message. The host's
    connection holds the grant the app was launched with
    ([grants.md](grants.md)), so a method or pane outside that grant is
    refused there;
  - there is no answer within 70 seconds: code `-32004`.

The error codes are the API's ([api.md](api.md)).

## Other messages from the host

**Subscriptions.** After a successful `events.subscribe` call (it must be
among the capabilities), each notification on the host's connection
arrives as:

```json
{"type":"notify","topic":"events.batch","payload":{"events":[]}}
```

**The connection.** When the host's connection to the daemon goes away
or comes back:

```json
{"type":"link","live":false,"reason":"the daemon closed the connection"}
{"type":"link","live":true}
```

**Quitting.** When the user quits (`Ctrl-c`), the host sends
`{"type":"quit"}` and closes the program's input. A program that has not
exited a second later is killed.

## Ending, and failing

- **Quitting by itself.** A program that exits with status 0 ends the
  app: the host exits, and the pane closes.
- **Failing.** A program that exits with another status, is killed by a
  signal, closes its output, or breaks the protocol leaves its last view
  in place. The status line is replaced by `stale:` with the reason and
  the last line of its standard error. `r` starts it again, and `q`,
  `Esc`, or `Ctrl-c` quits.
- **Not reading.** The host queues up to 1024 messages for a program that
  does not read its input. When the queue is full, new input is not sent
  and the status line says so.

None of this holds up the pane: the program's input, output, and
standard error are read and written apart from drawing and input.

## The Rust SDK

The `dreamer-sdk` crate (`crates/sdk`) speaks the protocol for Rust apps.
`dreamer_sdk::ui` builds trees, and `dreamer_sdk::app` runs the loop:

```rust
use dreamer_sdk::app::{self, App, Context, Event, Msg};
use dreamer_sdk::ui;

struct Counter(i64);

impl App for Counter {
    fn view(&self) -> ui::Screen {
        ui::screen("Counter", ui::column("main", [ui::text("count", self.0.to_string())]))
            .status("+ add  q quit")
    }

    fn update(&mut self, msg: Msg, cx: &mut Context) {
        if let Msg::Event(Event::Key { key }) = msg {
            match key.as_str() {
                "+" => self.0 += 1,
                "q" => cx.quit(),
                _ => {}
            }
        }
    }
}

fn main() -> Result<(), app::Error> {
    app::run_blocking(Counter(0))
}
```

- **The loop.** `view` is called after every message. The SDK sends the
  whole tree first and then only the changes (`patch`), and the whole
  tree again after `resync`.
- **Calls.** `cx.call(tag, method, params)` (or `cx.request(tag,
  &api::Request)`) sends a call and returns at once. The answer comes back
  later as `Msg::Reply { tag, method, result }`;
  `Msg::response(method, &value)` turns a result into the typed
  `api::Response`.
- **Other messages.** `Msg::Start` (the hello), `Msg::Event`,
  `Msg::Notify`, and `Msg::Link`.
- **Quitting.** `cx.quit()` ends the loop, and returning from `main`
  exits with status 0.
- **Builders.** Layout: `column`, `row`, `split` (with `.sizes` and
  `.vertical()`), `tabs`, and `scroll`. Data: `table` (with `.row`),
  `list` and `log` (with `.item`), `text`, `gauge`, `chart`, and `form`
  (with `field` and `toggle`). Any node: `.title`; tables and lists:
  `.selected`. `screen(title, root)` makes the frame, with `.status` and
  `.focus`.

A complete example is `crates/sdk/examples/pane-app.rs`: the tmux panes
in a table beside a counter, with what happened below. Build it with
`cargo build -p dreamer-sdk --example pane-app`, then point a package's
`[process] command` at the binary.
