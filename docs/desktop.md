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

## What the workspace does not draw

Two things are tmux's own screens, not a program's output, so independent
panes do not draw them:
- windows with floating panes (`layout_supported: false`);
- panes in a tmux-drawn mode such as copy mode or the choose tree,
  started by another tmux client. The topology reports these as the pane's
  `mode`.

They are never hidden. An unsupported window shows a notice instead of a
partial layout, and a pane in a mode gets a banner across its top. Both
offer the fallback:
- **The attach command.** It targets the same tmux server and window, as
  an ordinary tmux client. It is built from the daemon's tmux options
  (`--tmux`, `-L`, or `--tmux-socket-path`). It uses ids (`'$SESSION:@WINDOW'`),
  never names, and each word is quoted for a POSIX shell when needed.
  **Copy** puts it on the clipboard; only an explicit click does.
- **Open in terminal**, if `fallback.launch` is set in `dreamer.yaml`. It
  runs that argv with the `{command}` element replaced by the attach
  command's words. That is an exact argv, never a shell string. See
  [settings.md](settings.md).

Either way it is a normal tmux client with its own terminal, clearly apart
from Dreamer's pane rendering. It does not count as Dreamer drawing these
screens.

## Sidebar, status, and the saved arrangement

- **Sidebar.** Cmd-Shift-S on macOS, Ctrl-Shift-S on Linux, or "Toggle
  sidebar (sessions)" in the palette opens or closes a narrow sidebar listing the endpoint's sessions. Clicking one shows it
  in this window. This is local: it does not switch any tmux client.
- **Status.** The tab strip says something only when something is not
  normal: "connecting…", "reconnecting: why", or "N panes catching up"
  while panes resynchronize.
- **Saved arrangement.** The window remembers the session it shows,
  whether the sidebar is open, and where it was on screen. It is kept in
  `desktop.yaml` in the desktop state directory: `$DREAMER_DESKTOP_STATE_DIR`,
  else `$XDG_STATE_HOME/dreamer`, else `~/.local/state/dreamer`. That is
  apart from preferences (`dreamer.yaml`) and from everything on the host.
  The app writes the file off the UI thread, one write at a time, and you
  are not meant to edit it.
- **No stale ids.** tmux ids belong to one run of a tmux server (its
  incarnation, reported in the topology). A saved session id is reused
  only for the same incarnation. After the server restarts, the session is
  found again by name, and otherwise the first session is shown. An id
  from another server is never reopened (spec 15.2).

## Menus and shortcuts

Commands, their shortcuts, and the macOS menu bar come from one table
(`crates/app/src/keymap.rs`). macOS uses Cmd; Linux uses Ctrl-Shift, so
plain Ctrl and Alt combinations (Ctrl-C, Ctrl-W, Ctrl-Q, …) always reach
the program in the pane.

| Command | macOS | Linux |
| --- | --- | --- |
| Quit | Cmd-Q | Ctrl-Shift-Q |
| New window | Cmd-N | Ctrl-Shift-N |
| Close window | Cmd-W | Ctrl-Shift-W |
| Copy the selection | Cmd-C | Ctrl-Shift-C |
| Paste | Cmd-V | Ctrl-Shift-V |
| Command palette | Cmd-Shift-P | Ctrl-Shift-P |
| Zoom the focused pane | Cmd-Shift-Enter | Ctrl-Shift-Enter |
| Sessions sidebar | Cmd-Shift-S | Ctrl-Shift-S |
| Full screen | Ctrl-Cmd-F | F11 |
| Hide, hide others | Cmd-H, Alt-Cmd-H | (window manager) |
| Minimize | Cmd-M | (window manager) |

On macOS the menu bar has Dreamer (Services, Hide, Hide Others, Show All,
Quit), Shell (New Window, Close Window), Edit (Copy, Paste), View
(Command Palette, Sessions Sidebar, Zoom Pane, Enter Full Screen), and
Window (Minimize, Zoom), each item showing its shortcut.

Closing a window or quitting closes views only: tmux sessions, panes, and
the daemon keep running. On Linux the app quits when its last window
closes. On macOS it stays in the Dock, and clicking the icon opens a
window again.

## Remote hosts

A window can show a remote host's tmux, reached through the local daemon's
coordinator over SSH ([remote.md](remote.md)). Topology, pane output,
typing, pastes, and actions go through the same local connection, routed
with `hosts.watch`, `hosts.subscribe`, and `hosts.call`.

- **Sidebar.** With the sidebar open, a Hosts section lists every profile
  and connected host with its state: live, connecting, retrying (with the
  attempt count), the failure kind (auth, host key, …), disconnected, or
  not connected. Clicking a host connects it unless it is live and opens a
  window on it. The list is read once a second, and only while the sidebar
  is open.
- **Palette.** "Open host…", "Reconnect host…", and "Disconnect host…"
  take the host's name. Disconnecting closes SSH; the host's daemon, tmux,
  and programs keep running.
- **Launch.** `dreamer --host NAME` opens a window on that host,
  connecting it first.
- **Stale.** While the host is not live, the window's tab strip says
  "reconnecting: host NAME: why", and the panes keep their last content.
  When the host comes back, panes resume from the host's journal (no new
  checkpoint when its daemon kept running) and typing works again. Input
  sent while the host is down is dropped, never replayed later.
- **Title.** The window title starts with the host's name.
- **What differs from a local window.** A host's window saves no
  arrangement (the saved arrangement belongs to the local tmux). The
  fallback attach command for floating panes and tmux modes is not offered
  yet, because a local `tmux attach` cannot reach a remote server.

Profiles are edited in `hosts.yaml` for now; editing them in the desktop is
tracked in #169.

The `remote` gate stage (macOS) and the Linux desktop gate run
`dreamer --smoke-remote --host devbox` against a second isolated tmux and
daemon behind a stand-in `ssh`.

## Command palette

Cmd-Shift-P on macOS, or Ctrl-Shift-P on Linux, opens the command palette;
the shortcut is never sent to the pane. Type letters to filter (each one,
in order, anywhere in a command's name). Up and Down move, Enter runs, and
Escape closes.

| Command | Does |
| --- | --- |
| Split pane right, Split pane down | Splits the focused pane (`split_pane`) |
| New window | Adds a window at the end of the session (`new_window`) |
| Rename window… | Asks for a name, then renames the shown window |
| Next window, Previous window | Switches windows, wrapping around |
| Focus next pane | Moves focus here and in tmux |
| Zoom or unzoom pane | Toggles zoom |
| Close view (programs keep running) | Closes this desktop window only; tmux and every program keep running |
| Kill pane (ends its program)…, Kill window (ends its programs)… | Destructive. Shown in red; the first Enter only asks, a second Enter runs it, and Backspace backs out |

No destructive action is hidden behind "close". Every command goes
through the typed actions, and its result shows only in a newer topology.

`dreamer --smoke-workspace` (and `--smoke-workspace-restore`) checks this against an isolated tmux whose
session `main` has two windows, the first split into three panes, each
pane printing `READY-%<id>`. The macOS gate runs it as the `workspace`
step, and `gate linux-desktop` runs it on each backend. It checks that:
- the workspace has two tabs and three panes, each showing its own
  output;
- copy mode started with the plain `tmux` command marks the pane, and the
  attach command's target resolves (`tmux display-message`) to exactly
  that session and window;
- a split made with the plain `tmux` command appears;
- clicking a pane focuses it, and typed text reaches only that pane;
- zoom shows the focused pane alone, and unzoom brings all four back;
- tmux sizes the shown window to the workspace's area;
- dragging the vertical divider 10 cells in 10 moves resizes the pane to
  exactly that width, with at most 11 resize commands;
- the palette splits, kills only after the second Enter, moves focus,
  makes and renames a window, and switches windows (wrapping both ways);
- with a refreshing display, the palette opens from its shortcut and
  Escape closes it;
- a tab click switches the tmux window and shows only its pane.
- the palette opens the sidebar, the sidebar switches the window to a
  second session, and the arrangement is saved.

The gate then launches the app a second time, against a new tmux server.
A session `decoy` there now has the old id of `other`. The app must come
back on `other`, matched by name rather than by the stale id, with the
sidebar open.

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
