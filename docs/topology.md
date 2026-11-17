# Topology

`tmux.topology` returns an endpoint's sessions, windows, and panes as the
daemon last read them from tmux:
- windows have their name, panes, active pane, zoom state, size, and
  whether Dreamer can draw their layout (`layout_supported`, false for
  floating panes);
- panes have their cell rectangle in the window, process, title, and
  process generation, and `drawer_of` names the session whose
  [harness drawer](desktop.md#the-harness-drawer) a pane is;
- the session `_dreamer_drawers`, where hidden drawers wait, has
  `holding` set. Clients leave it out of tabs and session lists;
- a pane whose program is a registered app host (a package started with
  `dream app launch` or `dream app run`) has `app`: the app's `instance`
  and `app_id`, and `live`, false once the host's connection closed.

tmux is authoritative for all of it except `app`, which comes from the
daemon's own registrations; it goes when the pane closes.

## App panes

The host of an app pane publishes its current model to the daemon
(`apps.publish`), which keeps only the newest per pane. The user's own
clients read it with `apps.view` (`pane`, and `after`: wait up to
`wait_ms`, default 20 s, for a revision newer than that one) and send
the host input with `apps.input`: `pane` and exactly one of `key` (by
the app model's key names), `text` (typed or pasted into the focused
input), `select` (a click on a row or field, by index), or `scroll`
(wheel steps); with `select` or `scroll`, `node` names the tree node
they are for (see below). Both are owner only. The input arrives at the host as an
`apps.input` event on its connection and acts there exactly as if it was
made in the pane, so every client, including a plain tmux client, sees
the same app in the same state. When the host stops, `apps.view` keeps
returning its last view with `live: false` until the pane closes.

The host also publishes its view as a tree of components
([termapp.md](termapp.md#view-trees)), as versions numbered from 1
(`apps.publish_tree`, `seq`): the whole tree as JSON (`tree`), or the
changes from the version before (`patch`). The daemon refuses a patch
that does not follow the version it keeps, with a precondition error;
the host then sends the whole tree. The daemon keeps the newest tree
and the changes of the last 64 versions.

A client reads the tree with `apps.tree` (`pane`, `after` as for
`apps.view`, and `seq`, the version it holds, 0 for none). The answer
brings it to the newest version `seq`: the whole `tree` when it held
none or one older than the changes kept, else `patches`, one list of
changes per version, in order. A client whose patches do not lead to
`seq`, or do not apply, asks again with `seq` 0. Owner only, like
`apps.view`.

App panes exist on the local daemon only: a remote host's daemon marks
its own panes, but the coordinator does not forward their views.

### Images of app panes

The daemon draws nothing, so an image of an app pane as the desktop
draws it comes from a desktop (`dream app preview --png`):

- A desktop process waits for work with `apps.capture_wait` (`wait_ms`,
  default 20 s, at most 60 s) on a connection of its own; the answer is
  a request (`id`, `pane`, `path`, `seq`), or `id` 0 when none came.
  While it waits, and for 5 s after, it counts in `daemon.status` as
  one of `desktops`.
- `apps.capture` (`pane`; `path`, absolute, of the PNG file to write;
  `seq`, the tree version the image must show at least, 0 for whatever
  is drawn; `wait_ms`, default 10 s) hands the request to every desktop
  counted now. With none, it is refused at once with `no Dreamer window
  is running; --png needs one`.
- Each desktop answers with `apps.capture_done` (`id`, and `width` and
  `height` of the image it wrote, or `error`: why not, for example that
  none of its windows shows the pane). The first image written is the
  answer; when every desktop said why not, the caller gets their
  reasons.

All three are owner only.

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
- **Leaving.** When the last owner of a window leaves (for example, the
  desktop quits while the daemon keeps running), the daemon clears its size
  for that window (`refresh-client -C @WINDOW:`). Once Dreamer sizes no
  window at all, it also marks its control client `ignore-size`. tmux then
  sizes every window from your other clients again: your terminal's window
  goes back to the terminal's size. A control client that ever sized a
  window would otherwise keep counting with tmux's default 80x24. The flag
  is removed before Dreamer next applies a size.
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
- `when_the_last_dreamer_window_goes_the_tmux_window_returns_to_the_terminal_s_size`:
  - a tmux client at 70x20 stands in for your terminal;
  - Dreamer sizes the window to 110x33;
  - after its only client disconnects, the window is 70x20 again.

  Before the fix it stayed at 110x33, and with only the size cleared, at
  80x24.
- The workspace smoke requires tmux to size the shown window to the
  workspace's area when the display refreshes.

Writing that test found that windows always reported `zoomed: false`. The
inventory now reads tmux's `window_zoomed_flag`.
