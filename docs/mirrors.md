# Pane mirrors

`dreamerd` keeps a canonical terminal mirror of every pane on the tmux
server it drives: a Ghostty engine that holds the pane's screen, history,
cursor, and modes. Clients and agents read panes from these mirrors. They
never parse tmux output themselves. The mirrors keep running while no GUI
is open. You need this page if you are writing a client that follows
panes, or want to know what `dream read`, `dream mirrors`, and
`dream tier` report ([Reading mirrors](#reading-mirrors)).

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

## Every session streams

tmux sends a control client output only for panes in its own session, so
the daemon keeps one control client per tmux session (see
[daemon.md](daemon.md#control-clients)): its main client, and one more for
every other session, started and stopped as sessions come and go. Panes in
your own second session, in sessions made by `dream workspace up`, and in
hidden harness drawers stream like any other.

- **One source per pane.** Each pane's output is taken from exactly one
  client: that of the lowest session id whose session holds the pane's
  window. Every other client turns the pane's output off
  (`refresh-client -A '%N:off'`). A window linked into several sessions
  therefore arrives once, in order.
- **tmux's flow control still holds.** tmux stops reading a pane only
  when every control client attached has it off or is behind on it; a
  client never told about the pane counts as wanting more. So every
  client but the source has every pane off, and a pane that outruns the
  daemon is held back by tmux as with a single client.
- **A new source means a fresh attach.** When a pane's source changes (its
  window is moved, linked, or unlinked, a drawer is hidden or shown, or the
  source's session is killed), the mirror is attached again from a capture
  through the new client, as for a respawned pane. Output written
  meanwhile is in that capture; the two streams are never spliced.
- A pane whose session's client has not attached yet reports `attaching`.
  If a client cannot attach, the pane reports `failed` with the reason,
  and the next change tmux reports tries again.

The `not_streaming` mirror state is reserved. Daemons that kept a single
control client reported it for panes outside that client's session;
nothing reports it now.

## Ownership and ordering

- One router task reads every control client's events, each client's in
  order, and takes a pane's events only from that pane's source. It never
  waits on tmux itself; capture commands run as separate tasks, on the
  pane's source, and report back.
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
up from a checkpoint.

| Limit | Default | Enforced |
| --- | --- | --- |
| Per pane | 8 MiB | On every append, oldest records first |
| Per host | 256 MiB | On every append; over the limit, the pane that is writing gives up its own oldest records |
| Age | 10 minutes | On every append, and for idle panes whenever status is read |

The size limits count memory, not payload. Each record costs its bytes'
allocation (capacity, rounded up to 16 bytes) plus its slot in the
journal, 40 bytes on macOS and 48 on Linux. A stream of small records,
such as the output of `yes`, therefore retains fewer payload bytes than
the limit, but the journal's memory stays near it. `dream mirrors`
reports journal sizes counted the same way.

Trimming never touches the engine; it only limits how far back a client
can replay. The retained records are always exactly
`journal_first_sequence..=sequence`. A client that needs an older record
has fallen behind and needs a fresh checkpoint instead.

## Subscriptions

A client follows a pane with `terminal.subscribe`. The pane's owning worker
decides how the stream starts and registers the subscriber before it
applies the next record. Nothing can fall between the starting point and
the stream.

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
| `connection_lost` | The daemon lost tmux, for example because the tmux server exited; a new epoch follows. A tmux server started again on the same socket numbers its sessions, windows, and panes from zero, and its panes get new mirrors |
| `unsubscribed` | `terminal.unsubscribe` |
| `reattached` | The pane's output outran its mirror, which was rebuilt from a fresh capture (see [daemon.md](daemon.md#under-pressure)). Subscribe again; a checkpoint follows |

A slow client never stalls the pane's owner or other subscribers. The owner
never waits on a subscriber; it ends the one that is behind instead.

## Screen frames

A subscription has a **feed**. `stream`, the default, is the checkpoint or
replay and every record described above. On `frames`, the pane's owner
sends no records; the subscription instead gets the visible screen, read
from the mirror's engine, as `terminal.frame` bulk records (`ScreenFrame`).
The mirror, its journal, taps, filters, and collectors are the same on
either feed: frames are only another way to send a view what the mirror
already holds.

- **Asking.** `frames` is only for a connection whose client listed the
  `terminal.frames` feature in `Hello` ([wire.md](wire.md#features)).
  `terminal.subscribe` takes `feed: frames`; without the feature it is
  refused, and such a client never receives a frame. On `frames` the
  reply's mode is `frames`: no checkpoint, and a full frame at `sequence`,
  numbered `frame`, follows.
- **Switching.** `terminal.set_feed` moves a live subscription of the
  same connection either way, between two of the pane's records:
  - to `stream`: the reply names sequence N; a checkpoint at N follows and
    records from N + 1 (records may arrive before the checkpoint, as at a
    subscribe);
  - to `frames`: every record up to N arrives before the reply, which names
    N and the number of the full frame at N that follows; no record follows.

  Nothing of the previous feed arrives after the reply: the subscription's
  last checkpoint or frame is written first. A switch to the feed a
  subscription is already on starts it over the same way; from `stream` to
  `stream`, records up to N may still follow the reply, and are in the
  checkpoint.
- **Switching on its own.** A subscription made with `automatic: true` on
  a connection with the feature is switched by the daemon, by the pane's
  output rate. It starts on the feed it asked for. It goes to `frames` once
  the pane wrote more than `render.frames_above` a second (512 KiB by
  default) over the last `render.frames_dwell` (1 s), and back to `stream`
  once it wrote less than `render.frames_below` (128 KiB). It stays at
  least `render.frames_dwell` on a feed before either switch. The gap
  between the two rates and that least time keep a pane that writes about
  as fast as a threshold from flipping
  ([settings.md](settings.md#screen-frames)). The client learns of a switch
  from a `terminal.feed` event (`FeedSwitch`, `automatic: true`), placed
  exactly where a reply to `terminal.set_feed` would be, and the same rules
  follow it. A `terminal.set_feed` ends the automatic switching for that
  subscription.
- **Holding.** `terminal.hold_feed` with `hold: true` keeps a subscription
  on `stream` while the client works with the pane's history (a
  selection, a view scrolled back, a search, copy mode). A subscription on
  `frames` switches to `stream` at once, as above, with a `terminal.feed`
  event. None is switched to `frames` while held, nor for
  `render.frames_hold` (2 s) after `hold: false`. Then an automatic
  subscription follows the rate again, and any other one goes back to the
  feed it asked for.
- **What a frame holds.** The mirror sequence it was taken at (every record
  up to it is in the frame); its number, counting the subscription's frames
  from 1 without a gap across switches; the grid size; the rows that differ
  from the previous frame sent on the subscription, with their cells and
  styles; the cursor; the input modes; the title; the default colors and
  palette; the image placements, with the pixels of every image that was
  not placed in the last placements sent; whether the program holds presentation (synchronized
  output). A **full** frame carries every row and every part. The first
  frame after a subscribe or a switch is full, and so is a frame after the
  grid size changed. Any other frame applies only on top of the one
  numbered one less.
- **Row numbers.** `top` is the first screen row's number counted from the
  top of the pane's history, the way decorations number lines, so a mark
  lands on row `mark.row - top`. `total_rows` is history plus the screen.
- **Input modes.** Application cursor keys and keypad, every mouse
  reporting mode (9, 1000, 1002, 1003) and report format (1005, 1006, 1015,
  1016), alternate scroll, focus reports, bracketed paste, kitty paste
  events, the Num Lock and Alt modes (1035, 1036, 1039), LNM, KAM, the
  alternate screen, and the kitty keyboard protocol's flags.
- **Rate.** At most one frame per subscription every 8 ms. A frame is built
  only after the previous one on the subscription has been written to the
  socket, from the screen as it is then: screen states that changed in
  between are never sent, and a slow client gets fewer frames instead of a
  queue. Only the screen states nobody saw are skipped; no record is.
- **Effects.** Bells, title and directory changes, and clipboard writes
  (to the pane's focused client only) are not screen states, so they are
  never skipped. On `frames` they arrive as `terminal.effects` events
  (`EffectBatch`), in order, each before the first frame taken at or after
  its record, and before the reply or event that switches the
  subscription back to `stream`. Each arrives once: the effects of a
  record go with the records on `stream` and in these batches on
  `frames`, never both. A client that lets 4,096 of them pile up is ended
  as `lagged`, as a full record queue is.
- **Remote hosts.** Frames, switches, holds, effects, and the host's own
  switches pass through `hosts.subscribe`; the host's daemon applies its
  settings to the subscription. The coordinator numbers frames itself, so a
  relay that comes back after a lost link continues on frames with a full
  frame numbered after the last, and holds the new subscription if the
  client held the old one. A relay that loses its link after a switch to
  `stream` but before that switch's checkpoint arrived ends as `lagged`.
- **Diagnostics.** `daemon.diagnostics` lists every view subscription of
  the daemon's own panes (`views`): its pane, the feed it is on, whether
  the daemon switches it or a client holds it, how many times it switched
  (and how many of those were to `frames`), and the pane's output rate as
  the daemon measured it. `dream status --perf` and `dream diagnostics`
  print them. A relayed pane's subscription is in its host's diagnostics.

The daemon encodes frames with `client::frames::encode`. The `replica`
crate's `screen::ScreenView` keeps a screen from frames by these rules,
refuses a frame that does not follow the last one, and gives the renderer
the same frame an engine would for that screen. The desktop's view on
frames is built on it
([desktop-internals.md](desktop-internals.md#screen-frames)).

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
  So a busy Cold pane still costs the daemon the reading and decoding of
  its output. `cargo xtask bench tiers` measures that against a Warm and
  a Hot pane.
- **Memory:** shortly after panes go Cold, the daemon hands the freed
  memory back to the system. A Cold pane then holds about 3 KiB, while a
  Warm pane holds about 1 KiB for each line of history it keeps.

`dream mirrors` shows a Cold pane with state `cold` and tier `cold`, and
no sequence or journal.

**Going Cold on its own:**
- A pane that nothing follows goes Cold after it has been quiet for
  `tiers.cold_after` (30 minutes by default).
- It also goes Cold when there are more than `tiers.warm_limit` such panes
  (128 by default), quietest first.
- And when all the host's mirrors together hold more than
  `tiers.warm_memory` (1 GiB by default), quietest first, until they are
  back under it. A mirror's memory is estimated from its screen and
  scrollback (about 1 KiB per 120-column line) plus its journal. Followed
  panes count toward the ceiling but are never demoted, so they alone can
  go over it.
- One check every 10 s covers the whole host; there is no timer per pane.
- The daemon logs what it demoted.

See [settings.md](settings.md#pane-tiers).

## Reading mirrors

| Command | API | Shows |
| --- | --- | --- |
| `dream read %PANE [--history N] [--json]` | `terminal.read` | Up to N scrollback lines (at most 10,000), then the screen, as text. With `--json`, also the size, cursor, generation, epoch, and the number of records applied |
| `dream mirrors [--json]` | `terminal.mirrors` | Each pane's state, tier, size, records, bytes ingested, retained journal range and size, and worker; host journal totals and limits |
| `dream tier %PANE hot\|warm\|cold` | `terminal.set_tier` | Sets a pane's tier |

## Not yet

- **Scrollback depth:** a mirror keeps as much scrollback as tmux's
  `history-limit`; Dreamer has no lower limit of its own. The Warm memory
  ceiling bounds the total by demoting panes, not by trimming history.
- **Pixel size:** the canonical cell pixel size is fixed (10×20) until
  clients negotiate geometry.
