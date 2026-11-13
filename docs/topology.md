# Topology

`tmux.topology` returns an endpoint's sessions, windows, and panes as the
daemon last read them from tmux:
- windows have their name, panes, active pane, zoom state, size, and
  whether Dreamer can draw their layout (`layout_supported`, false for
  floating panes);
- panes have their cell rectangle in the window, process, title, and
  process generation.

tmux is authoritative for all of it.

## Following changes

`tmux.watch` returns the same topology with a `revision`. Afterwards the
connection receives a `tmux.topology` event (a `TopologyUpdate`) whenever
the topology changes:
- splits, kills, renames, zoom, window and pane switches, respawns, and new
  sessions;
- changes made by any tmux client, not only through Dreamer.

The daemon re-reads tmux once per burst of structural notifications
(after 30 ms of quiet). It publishes only if the result differs, so one
change gives one event at most.

**Revisions only increase.** A watcher never receives an older topology
after a newer one.

**Coalescing.** Updates go through a latest-value channel. A client that
reads slowly gets the newest topology when it catches up, not every step
in between (spec 16.3: status coalesces to the latest revision). The
daemon never waits on a watcher.

There is one watch per connection; a second `tmux.watch` only returns the
current topology. The watch ends with the connection.

## Window size

tmux is authoritative for cell geometry. Each tmux window takes its size
from at most one Dreamer client, its **sizing owner** (spec 11.8):
- **Asking.** `tmux.size_window {window, cols, rows, client}` asks for a
  window's size. The first client to ask owns it. The daemon applies the
  owner's requests with `refresh-client -C @WINDOW:COLSxROWS` on its control
  client. A request from any other client is answered as a follower
  (`owner: false`) and changes nothing.
- **When clients ask.** Clients ask only when their own view changes size
  or shows another window, never in reaction to a topology change. The
  daemon never re-applies a size by itself. That is why two desktops of
  different sizes cannot make a window oscillate.
- **Handover.** When the owner's connection closes (or it asks with
  `release`), the most recent other requester becomes the owner, and its
  last request is applied once.
- **Leaving.** When the last owner leaves, the window keeps the last
  applied size for Dreamer's control client. The daemon does not reset it.
- **Your tmux options are never changed.** `window-size` decides how
  Dreamer's control-client size combines with other clients'. With the
  default `latest`, a normal tmux client you are typing in wins, and
  Dreamer does not fight it. With `largest` or `smallest`, Dreamer's size
  is one of the sizes compared. With `manual`, nothing changes size
  automatically.

The workspace measures its pane area in cells when it paints. It asks for
that size for the window it shows, once the area has been still for
150 ms.

## Evidence

`crates/server/tests/topology.rs`:
- `watchers_follow_changes_made_by_another_tmux_client` makes a split, a
  rename, a zoom, a new window, a window switch, a respawn, and a kill
  with the plain `tmux` command. Each arrives as an event with a higher
  revision, and the last event equals `tmux.topology`.
- `a_burst_of_changes_coalesces_to_the_final_state` makes 30 quick renames.
  Fewer than 30 events arrive, the last carries the final name, and
  nothing older follows.

- `one_client_owns_a_window_s_size_and_ownership_passes_on_disconnect`
  sets `window-size latest` on its isolated tmux. It checks that:
  - the first client owns the window and gets its size;
  - a second client with another size is a follower;
  - the size holds over time;
  - another tmux client attached at 70x20 makes the window settle (at
    70x20) and stay put;
  - when the owner disconnects, the follower's last size is applied.
- The workspace smoke requires tmux to size the shown window to the
  workspace's area when the display refreshes.

Writing that test found that windows always reported `zoomed: false`. The
inventory now reads tmux's `window_zoomed_flag`.
