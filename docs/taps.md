# Taps

A tap is a bounded view of panes for monitors, filters, scripts, and
agents. It reads the daemon's canonical mirror after output was ingested,
so it never changes what a program wrote or what any tmux client sees,
and it can never slow a pane down.

## Levels

| Level | Delivers | Typical use |
| --- | --- | --- |
| `raw` | The bytes programs wrote, in order (`raw`) | Recorders, protocol tools |
| `text` | Committed lines without escapes (`lines`), and changed rows of an alternate screen (`screen`) | Filters, monitors, agents |
| `meta` | Size, title, cwd, sequence, and whether an alternate screen is active (`meta`), when they change | Dashboards that need no content |

A **committed** line is one the cursor has moved past: output lines, and
a command line once Enter was pressed. The line the cursor is on (a prompt
being typed) is not committed yet. While a full-screen program (an
alternate screen) runs, `text` reports the rows that changed since the
last look instead, coalesced; that program's screen never becomes
committed lines.

## Subscribing

`taps.subscribe` takes a `level`, the `panes` to follow (or `every_pane`,
which also follows panes created later), and for `text` and `meta` an
`interval_ms` (default 250, at least 50). Data arrives as `taps.data`
events, one pane each, with the pane's generation. `ended` says the pane
is gone. End a tap with `taps.unsubscribe`.

`text` and `meta` look at a pane once per interval and read only when it
produced output, so an idle pane costs nothing and a busy one a bounded
amount. `raw` follows the record stream.

A `text` tap finds which lines are new by comparing the newest history
lines with those of its last look, by content: the history is limited in
bytes, so its length says nothing once it is full. A run of identical
lines can make the overlap ambiguous; the tap then reports the fewest new
lines that fit.

## Gaps

Output a tap did not deliver is reported as a `gap`. The pane itself lost
nothing.

| Reason | Meaning |
| --- | --- |
| `lagging` | A `raw` tap fell behind (its reader was slow) and the journal no longer had what it missed; it continues from now |
| `window` | More than 200 lines were committed between two looks of a `text` tap; `lines` says how many were skipped when that is known |
| `restarted` | The pane's program was replaced (respawned) |

A tap that falls behind is dropped by the mirror rather than waited for.
If the journal still holds what it missed, it continues from there
without a gap.

## Grants

Following listed panes needs those panes in the connection's grant;
`every_pane` needs `all_panes` (see [grants.md](grants.md)).
