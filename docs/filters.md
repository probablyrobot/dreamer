# Filters

A filter watches panes for lines that match and acts on them: it
publishes them, records them as events, sends them to an app, or types a
fixed command into a pane. It only reads. It never changes what a program
wrote, the daemon's mirror, or what any tmux client sees.

A filter is a [text tap](taps.md) plus the Rust match stage (the same
patterns as `terminal.query`: linear-time regular expressions or plain
text, size-limited) plus outputs.

## Installing

`filters.install` (the user only) takes:

| Field | Meaning |
| --- | --- |
| `name` | Unique; the filter's sender identity is `filter:<name>` |
| `panes` or `every_pane` | What it watches |
| `pattern`, `literal`, `literals`, `ignore_case` | What it matches, as in [taps.md](taps.md#queries) |
| `topic` | Publish each matched line on this topic: the derived stream ([messages.md](messages.md)) |
| `recipient` | Send each matched line to this instance |
| `event` | Record each match as a `filters.matched` event; [scripts](scripts.md) receive these with `on_event` |
| `input` | Type this fixed text into this pane on each match. The matched text is never typed |
| `grant` | What the filter itself may do: `taps.subscribe` for its panes, `messages.publish` for `topic`/`recipient`, `terminal.input` and the pane for `input` |
| `per_second` | Matches acted on per second (default 20); more are counted as `dropped` |
| `breaker` | Triggers allowed in any 10 s (default 50); see below |

The grant is checked when the filter is installed and again on every call
it makes, like any connection's. `filters.list` shows each filter's state
and counts, and `filters.pause`, `filters.resume`, and `filters.remove` (the
user only) control it.

## Loops

A filter whose output produces its own match would run forever. Three
things stop that:

- **Its own input is not matched.** Lines that contain the text the filter
  typed, within 5 s, are skipped (`skipped_own`): the shell echoing its
  command does not count.
- **Rate.** At most `per_second` matches are acted on.
- **Circuit breaker.** Triggers (publish, send, type) beyond `breaker` in any
  10 s trip the filter. It stops, its state becomes `tripped` with the
  reason, a `filters.tripped` event is recorded, and it stays stopped until
  `filters.resume`.

Messages a filter sends also carry the broker's causal depth limit
([messages.md](messages.md#loops-and-limits)).
