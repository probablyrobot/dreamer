# Taps

A tap is a bounded view of panes for monitors, filters, scripts, and
agents. It reads the daemon's canonical mirror after output was ingested,
so it never changes what a program wrote or what any tmux client sees,
and it can never slow a pane down.

## Levels

| Level | Delivers | Typical use |
| --- | --- | --- |
| `raw` | The bytes programs wrote, in order (`raw`) | Recorders, protocol tools |
| `text` | Committed lines without escapes (`lines`, with each line's `rows` from the top of history and the pane's `cols`), and changed rows of an alternate screen (`screen`) | Filters, monitors, agents |
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

## Queries

`terminal.query` is the one-shot counterpart: which panes' newest lines
match a pattern right now, here and on connected hosts.

| Field | Meaning |
| --- | --- |
| `pattern` | A regular expression; with `literal`, plain text. `literals` adds plain-text alternatives; `ignore_case` applies to all |
| `panes` | The panes to search; empty means every mirrored pane |
| `lines` | How many of each pane's newest lines, screen included (default 200, at most 2000) |
| `max_matches` | Default 100, at most 1000 |
| `max_bytes` | Bytes of matching line text in the answer; default 65536, at most 1048576 |
| `hosts` | Connected hosts whose panes are searched too; `*` means every live host |
| `timeout_ms` | Default 2000, at most 10000 |
| `title` | Only panes whose title matches this regular expression |

The answer lists each match (host, pane, line index in the window, the
line, and the match's byte offsets) and how many panes were searched.
When a bound cut it short, `truncated` is set and `notes` says which
(`max_matches`, `max_bytes`, `timeout`, a host that did not answer). A host that cannot
be reached is a note, not a failure.

Patterns run in Rust, in time linear in the input (no backtracking). A
pattern that would compile too large (`(a{1000}){1000}`), is longer than
4 KiB, or is empty is refused. Panes are read a few at a time through the
same cheap reads taps use, so a query never holds up input or output.

Searching listed panes needs them in the grant; every pane, or any host,
needs `all_panes`.
