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

## Tiers

Every mirrored pane is **Warm** (mirrored and journaled, not shown) or
**Hot** (visible in a client). In the daemon both tiers keep a live mirror
and a journal; clients set Hot for the panes they show. **Cold** (metadata
only, with tmux output switched off for the pane) is not implemented yet.

## Reading mirrors

| Command | API | Shows |
| --- | --- | --- |
| `dream read %PANE [--history N] [--json]` | `terminal.read` | Up to N scrollback lines, then the screen; size, cursor, generation, epoch, and the number of records applied |
| `dream mirrors [--json]` | `terminal.mirrors` | Each pane's state, tier, size, records, bytes ingested, retained journal range and size, and worker; host journal totals and limits |
| `dream tier %PANE hot\|warm` | `terminal.set_tier` | Sets a pane's tier |

## Not yet

- **Cold tier** and demotion through tmux's per-pane output control.
- **Scrollback depth:** a mirror keeps as much scrollback as tmux's
  `history-limit`.
- **Pixel size:** the canonical cell pixel size is fixed (10×20) until
  clients negotiate geometry.
