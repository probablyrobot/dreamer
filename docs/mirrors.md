# Pane mirrors

`dreamerd` keeps a canonical terminal mirror of every pane on the tmux
server it drives: a Ghostty engine that holds the pane's screen, history,
cursor, and modes. Clients and agents read panes from these mirrors. They
never parse tmux output themselves. The mirrors keep running while no GUI
is open.

## How a pane is mirrored

1. **Cold attach.** The daemon pauses the pane's output for its control
   client and captures the pane's state, history, screen, saved screen, and
   pending parser bytes in one command line that tmux runs without
   yielding. It then resumes the pane (see [tmux-bootstrap.md](tmux-bootstrap.md)).
   Output that arrives after the pause is the exact continuation of the
   capture, and it is kept in order.
2. **Live.** The pane's output, and size changes taken from
   `%layout-change`, go to the pane's engine in the order tmux sent them.
3. **Following tmux.** The daemon re-reads the inventory whenever tmux
   reports a structural change, including pane process changes through a
   `pane_pid` subscription:
   - new panes are attached;
   - closed panes are dropped;
   - a respawned pane gets a new mirror with the next generation.

## Ownership and ordering

- One router task reads the control connection's events in order. It never
  waits on tmux itself; capture commands run as separate tasks and report
  back.
- Engines live on a fixed pool of VT worker threads (up to 4). Each pane has
  exactly one owning worker and one ordered channel to it, so its records
  are applied in stream order. A full channel makes the router wait, which
  slows reading from tmux instead of dropping bytes.
- Every result names the pane (server, incarnation, pane id), its process
  generation, and the connection epoch. A record for an older generation is
  ignored.
- Mirrors never answer the application. The pane's replies come from tmux
  (see [terminal-compatibility.md](terminal-compatibility.md)).

## Journal and retention

Each mirror journals every record it applies, numbered by the mirror's
committed sequence: output bytes, size changes, and the continuation that
followed the cold attach. The journal is what a client replays to catch
up from a checkpoint (M2-02).

| Limit | Default | Enforced |
| --- | --- | --- |
| Per pane | 8 MiB | On every append, oldest records first |
| Per host | 256 MiB | On every append; over the limit, the pane that is writing gives up its own oldest records |
| Age | 10 minutes | On every append, and for idle panes whenever status is read |

Trimming never touches the engine; it only limits how far back a client
can replay. The retained records are always exactly
`journal_first_sequence..=sequence`. A client that needs an older record
has fallen behind and needs a fresh checkpoint instead.

## Subscriptions

A client follows a pane with `terminal.subscribe`. The pane's owning worker
decides how the stream starts and registers the subscriber before it
applies the next record. Nothing can fall between the starting point and
the stream (spec 11.5).

- **Checkpoint:** the worker snapshots the engine at sequence N, and records
  N+1 onward follow. The snapshot travels as chunked bulk data (topic
  `terminal.checkpoint`). Because priority events go first, records after N
  can arrive before the checkpoint; the client holds them until the
  checkpoint is restored.
- **Replay:** a client that resumes with the epoch, generation, and sequence
  it last applied gets the retained records right after that point, and no
  checkpoint. Resuming from another epoch or generation, or from a point
  that has been trimmed, falls back to a checkpoint.
- **Scrollback:** the reply includes the mirror's scrollback limit. A
  snapshot does not carry it, and a replica that kept more or less history
  would drift from the mirror.

Records arrive in order as `terminal.records` batches. A stream always ends
with a `terminal.end` event that names the reason:

| Reason | Meaning |
| --- | --- |
| `lagged` | The subscriber's queue (4096 records) filled: it is not keeping up. Resubscribe, resuming if the records are still retained |
| `pane_closed` | The pane is gone |
| `respawned` | The pane runs a new process; subscribe to the new generation |
| `connection_lost` | The daemon lost tmux; a new epoch follows |
| `unsubscribed` | `terminal.unsubscribe` |
| `reattached` | The pane's output outran its mirror, which was rebuilt from a fresh capture (see [daemon.md](daemon.md#under-pressure)). Subscribe again; a checkpoint follows |

A slow client never stalls the pane's owner or other subscribers. The owner
never waits on a subscriber; it ends the one that is behind instead.

## Client replicas

The `replica` crate keeps a client's copy of a pane in step with the
daemon's mirror. It never guesses:

| Situation | What the replica does |
| --- | --- |
| A record at or below the last applied sequence | Ignores it (a duplicate, or already in the checkpoint) |
| A record beyond the next expected sequence, including out-of-order delivery | Stops applying and asks for a resnapshot. It never applies on top of a hole |
| A record or checkpoint from another subscription, epoch, or generation | Discards it |
| Records that arrive before their checkpoint | Holds them, then applies those after the checkpoint's sequence |
| A checkpoint that fails to restore (corrupt or interrupted transfer) | Keeps showing the previous view, marks it stale, and asks again |
| Reconnect | Resumes from its (epoch, generation, sequence): a replay if the daemon still retains the records, otherwise a checkpoint |
| Daemon restart | Epochs never repeat across restarts, so the old resume point is refused and a checkpoint follows |
| Pane respawned | Follows the new generation from its own checkpoint |
| Ended as lagged | Resubscribes with a resume point |
| Ended as reattached | Subscribes again from a checkpoint. The rebuilt mirror's journal starts past a gap, so an old resume point could never be spliced on |

Resnapshots are bounded: five in a row without a successful checkpoint,
with backoff from 50 ms up to 2 s, and the replica reports "unable to catch
up" instead of looping.

## Tiers

Every pane is in one tier:

| Tier | In the daemon | Set by |
| --- | --- | --- |
| **Hot** | Live mirror and journal | Clients, for the panes they show |
| **Warm** | Live mirror and journal | The default for a mirrored pane |
| **Cold** | Pane metadata only: no mirror and no journal. The pane's output still arrives from tmux and is dropped before any terminal parsing | `dream tier %N cold` (`terminal.set_tier`) |

The pane keeps running in tmux while it is Cold, and what it writes stays
in tmux's own grid.

- **Going Cold** is refused while anything follows the pane: a view, a
  tap, or a filter. The error names how many subscribers there are.
- **Leaving Cold:** any request for the pane's content brings it back
  before answering. That includes a subscription, a read, a query, input,
  or a tier change. The pane is attached from a fresh capture, as a new
  pane is: a cold attach (see [tmux-bootstrap.md](tmux-bootstrap.md)). The
  request waits up to 5 s for it. Output alone does not bring a pane back.
- **Why tmux keeps sending the output:** tmux 3.8 can switch a pane's
  output off for one control client (`refresh-client -A '%N:off'`). But
  when every client has done so, tmux stops reading the pane. With the
  daemon as the only client, a program writing to a Cold pane would stall.
- **Memory:** shortly after panes go Cold, the daemon hands the freed
  memory back to the system. A Cold pane then holds about 3 KiB, while a
  Warm pane holds about 1 KiB for each line of history it keeps (see
  [performance.md](performance.md)).

`dream mirrors` shows a Cold pane with state `cold` and tier `cold`, and
no sequence or journal.

**Going Cold on its own:**
- A pane that nothing follows goes Cold after it has been quiet for
  `tiers.cold_after` (30 minutes by default).
- It also goes Cold when there are more than `tiers.warm_limit` such panes
  (128 by default), quietest first.
- One check every 10 s covers the whole host; there is no timer per pane.
- The daemon logs what it demoted.

See [settings.md](settings.md#pane-tiers).

## Reading mirrors

| Command | API | Shows |
| --- | --- | --- |
| `dream read %PANE [--history N] [--json]` | `terminal.read` | Up to N scrollback lines, then the screen; size, cursor, generation, epoch, and the number of records applied |
| `dream mirrors [--json]` | `terminal.mirrors` | Each pane's state, tier, size, records, bytes ingested, retained journal range and size, and worker; host journal totals and limits |
| `dream tier %PANE hot\|warm\|cold` | `terminal.set_tier` | Sets a pane's tier |

## Not yet

- **Scrollback depth:** a mirror keeps as much scrollback as tmux's
  `history-limit`.
- **Pixel size:** the canonical cell pixel size is fixed (10×20) until
  clients negotiate geometry.
