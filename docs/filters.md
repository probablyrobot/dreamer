# Filters

A filter watches panes for lines that match and acts on them: it marks
them, publishes them, records them as events, sends them to an app, or
types a fixed command into a pane. It only reads. It never changes what a
program wrote, the daemon's mirror, or what any tmux client sees.

## A first filter

This filter highlights every line that contains `error`, in any case, on
every pane. `dream rpc` sends a JSON-RPC request, one object per line, to
the daemon ([cli.md](cli.md#json-rpc)):

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"filters.install","params":{"name":"errors","every_pane":true,"pattern":"error","literal":true,"ignore_case":true,"decorate":"highlight","grant":{"methods":["taps.subscribe"],"all_panes":true}}}' | dream rpc
```

List it, and remove it, the same way:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"filters.list"}' | dream rpc
echo '{"jsonrpc":"2.0","id":1,"method":"filters.remove","params":{"name":"errors"}}' | dream rpc
```

There is no `dream` subcommand for filters. Agents install them with the
`watch_lines`, `list_filters`, and `stop_filter` tools ([mcp.md](mcp.md)),
and a [filter pane](termapp.md#filter-panes) turns a description into
filters for you.

A filter is a [text tap](taps.md), a match stage (the same patterns as
`terminal.query`: linear-time regular expressions or plain text,
size-limited), and outputs.

## Installing

`filters.install` (the user only) takes:

| Field | Meaning |
| --- | --- |
| `name` | Unique: letters, digits, `-`, `_`, or `.`. The filter's sender identity is `filter:<name>` |
| `panes` or `every_pane` | What it watches |
| `pattern`, `literal`, `literals`, `ignore_case` | What it matches, as in [taps.md](taps.md#queries) |
| `topic` | Publish each matched line on this topic: the derived stream ([messages.md](messages.md)) |
| `recipient` | Send each matched line to this instance |
| `event` | Record each match as a `filters.matched` event; [scripts](scripts.md) receive these with `on_event` |
| `input` | `{ pane, text }`: type this fixed text into this pane on each match. The matched text is never typed |
| `decorate`, `badge` | Mark each matched line (`highlight`, `dim`, `fold`, or `badge` with its text); see [Decorations](#decorations) |
| `grant` | What the filter itself may do: `taps.subscribe` for its panes, `messages.publish` for `topic`/`recipient`, `terminal.input` and the pane for `input` |
| `per_second` | Matches acted on per second (default 20); more are counted as `dropped` |
| `breaker` | Triggers allowed in any 10 s (default 50); see [Loops](#loops) |

The grant is checked when the filter is installed and again on every call
it makes, like any connection's. `filters.list` shows each filter's state
and counts, and `filters.pause`, `filters.resume`, and `filters.remove` (the
user only) control it.

## Filters an app installs

A [filter pane](termapp.md#filter-panes) installs filters itself. An app
may call the `filters.*` methods only with `filters.manage` in its grant
(a grant entry, not a method) and content read (`terminal.query`,
`terminal.read`, or `taps.subscribe`). Its filters only read:

- they watch panes within the app's own (`every_pane` needs the app to
  have `all_panes`), with a grant of at most `taps.subscribe` on them;
- they may record events (`event`) and decorate, but not publish, send,
  or type (`topic`, `recipient`, and `input` are refused);
- they end with the app's grant, if it expires.

An app pauses, resumes, and removes only filters an app installed within
its own panes, never one the user installed, and `filters.list` shows it
only those. A filter keeps running after the app quits; the user removes
it with `filters.remove`.

## Loops

A filter whose output produces its own match would run forever. Three
things stop that, whether the filter types the input itself or a
[script](scripts.md) does in answer to its `filters.matched` events:

- **Its own input is not matched.** Lines that contain the text the filter
  typed, within 5 s, are skipped (`skipped_own`): the shell echoing its
  command does not count.
- **Rate.** At most `per_second` matches are acted on.
- **Circuit breaker.** Triggers (publish, send, type) beyond `breaker` in any
  10 s trip the filter. It stops, its state becomes `tripped` with the
  reason, a `filters.tripped` event is recorded, and it stays stopped until
  `filters.resume`.

Text or a paste a script types while its `on_event` handles one of the
filter's `filters.matched` events counts as the filter's own: its echo is
skipped, and each one is a trigger for the breaker. Recording events on
their own is not a trigger, so a filter pane watching a busy log is not
tripped by it.

Messages a filter sends also carry the broker's causal depth limit
([messages.md](messages.md#loops-and-limits)).

## Decorations

A filter with `decorate` marks the lines it matches. A decoration is data
for clients to draw over the terminal, never part of the terminal stream:
what the program wrote and what tmux shows stay unchanged, and hiding or
folding a line is a local presentation choice.

`decorations.list` returns a pane's marks. Each names its line by `row`,
counted from the top of the pane's history, with the line's `text`, the
pane `generation`, and its width (`cols`), plus the style, badge, and
filter. A mark is valid only while all of these still hold:

- a respawn (new generation) or a resize (the text reflows) drops all of a
  pane's marks;
- a line that left the history, or moved because the history is at its
  limit, is dropped when the marks are next listed.

A mark can be made just before the mirror applies its line. Such a mark
is not listed yet, and is not dropped: `decorations.list` waits up to 3 s
for the mirror to reach it.

Each pane keeps at most 500 marks; the oldest go first. A
`decorations.changed` event (with the `pane`) says when to list again.
Listing a pane needs it in the grant. The desktop draws them over the
grid and can switch them off per pane; see
[desktop.md](desktop.md#decorations).
