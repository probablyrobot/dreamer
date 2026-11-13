# Desktop app

`dreamer` is the GPUI desktop client.

```text
dreamer [--workspace] [-L socket | --tmux-socket-path PATH] [--tmux PATH]
dreamer --attach %PANE [-L socket | --tmux-socket-path PATH] [--tmux PATH]
dreamer --fixture FILE
```

With no arguments (as when launched from Finder or a desktop launcher),
or with `--workspace`, the app opens a **workspace** for the first session
of the default tmux server. See below. The app starts `dreamerd` if it is
not running, passing it the tmux options. See
[packaging.md](packaging.md) for the macOS app and the Arch package.

`--attach` shows a single tmux pane as a client replica (see
[mirrors.md](mirrors.md)). `--fixture` paints a VT fixture, and `--smoke`
alone paints the built-in renderer demo.

## Workspace

A workspace window shows one tmux session:
- **Tab strip.** A narrow strip lists the session's windows by index. The
  active one is highlighted. Clicking a tab runs `select-window`.
- **Panes.** The active window's panes are drawn at their tmux cell
  rectangles. tmux is authoritative for geometry (spec 15.1). The one-cell
  gaps tmux leaves between panes show as dividers. A zoomed window shows
  its active pane over the whole area. A window with a layout Dreamer
  cannot draw yet (floating panes) shows a notice instead of a partial
  layout.
- **One feed per pane.** Each visible pane has its own feed (replica and
  subscription), with its own scroll, selection, and links. Panes in other
  windows are not subscribed. A pane's feed stops when the pane leaves
  the view.
- **Focus.** Keyboard input goes to one pane. Clicking a pane focuses it
  here and runs `select-pane`, and the click does not type anything. The
  focused pane gets focus-in and the previous one focus-out, so focus
  reports and clipboard addressing follow it. The window title follows the
  focused pane.
- **Following tmux.** The view follows `tmux.watch` (see
  [topology.md](topology.md)). Splits, kills, renames, zoom, and window
  switches made by any tmux client appear without a restart. An action's
  result is seen only when the new topology arrives; it is never assumed.

- **Window size.** The workspace asks tmux to size the window it shows to
  its pane area. That takes effect only while this window owns the tmux
  window's size; see [topology.md](topology.md#window-size).
- **Dragging splits.** The one-cell gaps between panes are drag handles.
  Dragging shows a preview line at once and resizes the pane on the left
  of (or above) the gap. It sends `resize-pane` at most every 60 ms with
  the latest size, plus once on release, and never shrinks a pane below
  2 cells. The panes themselves always show tmux's actual layout. A drag
  ends at whatever tmux decided.
- **Zoom.** Cmd-Shift-Enter on macOS, Ctrl-Shift-Enter on Linux, or the
  `zoom` control on the focused pane, toggles zoom through the typed zoom
  action. The shortcut is never sent to the pane.

`dreamer --smoke-workspace` checks this against an isolated tmux whose
session `main` has two windows, the first split into three panes, each
pane printing `READY-%<id>`. The macOS gate runs it as the `workspace`
step, and `gate linux-desktop` runs it on each backend. It checks that:
- the workspace has two tabs and three panes, each showing its own
  output;
- a split made with the plain `tmux` command appears;
- clicking a pane focuses it, and typed text reaches only that pane;
- zoom shows the focused pane alone, and unzoom brings all four back;
- tmux sizes the shown window to the workspace's area;
- dragging the vertical divider 10 cells in 10 moves resizes the pane to
  exactly that width, with at most 11 resize commands;
- a tab click switches the tmux window and shows only its pane.

Sizing, clicking, typing, and dragging need a refreshing display. With a locked screen they
are reported as not run and the step as `not_run`.

## Frame scheduling

- **Owner thread.** A background thread per pane owns the replica's engine
  and waits on the daemon connection with no timeout. It builds a frame only
  when records were applied or a checkpoint was restored.
- **Coalescing.** The newest frame replaces any unpainted one, and the
  window is woken through a channel that holds at most one signal. A burst
  of output becomes as few repaints as the window can take.
- **Idle costs nothing.** No timers and no polling: an idle pane produces no
  thread wakeups, no window wakeups, and no paints.
- **Synchronized output** (`CSI ? 2026 h`): a held frame is not shown until
  the application ends the hold, or until 1 second has passed, whichever
  comes first. A broken program cannot freeze the view.
- **Connection loss.** The owner reconnects with backoff (up to 2 s) and
  resumes, or resnapshots, as the replica rules decide. A closed pane, or
  one that cannot catch up, is reported, not retried forever.

## Renderer

Each row's text is shaped once and reused for as long as the row is shared
between frames and the colors, font, size, and cell width stay the same.
Paint statistics report lines shaped versus reused, decorated runs, and the
cursor shape painted.

## Checks

The macOS gate runs these after building the app:

| Step | Checks | Needs |
| --- | --- | --- |
| `matrix` | Every fixture in `crates/terminal-render/fixtures` paints the statistics listed in its `expected.txt`. These are derived from the fixture content: ASCII, combining marks, CJK, emoji, fallback glyphs, box drawing, five underline styles, strikethrough, inverse, dim, and the block, bar, underline, and hidden cursor | A GUI session |
| `cache` | `dreamer --smoke-repaint`: painting the same frame again reshapes nothing | A display that keeps refreshing |
| `live` | The live pane check below | A display that keeps refreshing |

A locked screen still paints a window's first frame, but macOS then stops
refreshing it. `cache` and `live` are recorded as not run while the screen
is locked. If a requested repaint does not happen, the app itself exits
with status 5 ("inconclusive"), so a count of zero paints is never taken
as evidence when nothing could have painted.

`dreamer --smoke --expect TEXT --attach %PANE …` waits for a frame
containing TEXT. It then requests one repaint as a control. If that paints,
it watches 1.5 s of idle pane and exits with status 3 if anything woke the
window or painted. The `live` step runs this against an isolated tmux
server and private daemon directories.
