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

## Evidence

`crates/server/tests/topology.rs`:
- `watchers_follow_changes_made_by_another_tmux_client` makes a split, a
  rename, a zoom, a new window, a window switch, a respawn, and a kill
  with the plain `tmux` command. Each arrives as an event with a higher
  revision, and the last event equals `tmux.topology`.
- `a_burst_of_changes_coalesces_to_the_final_state` makes 30 quick renames.
  Fewer than 30 events arrive, the last carries the final name, and
  nothing older follows.

Writing that test found that windows always reported `zoomed: false`. The
inventory now reads tmux's `window_zoomed_flag`.
