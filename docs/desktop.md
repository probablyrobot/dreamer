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
  gaps tmux leaves between panes show as dividers, and a narrow inset in
  the same color keeps text off the window's edges. A zoomed window shows
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
  action. The shortcut is never sent to the pane. The control appears only
  while the pointer is over the focused pane, and only when the window has
  more than one pane. It sits on its own chip in the pane's bottom-right
  corner.

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
  never names, and each word is quoted for a POSIX shell when needed. It
  is shown in full on a line of its own, wrapped if the pane is narrow.
  **Copy** puts it on the clipboard; only an explicit click does.
- **Open in terminal**, if `fallback.launch` is set in `dreamer.yaml`. It
  runs that argv with the `{command}` element replaced by the attach
  command's words. That is an exact argv, never a shell string. See
  [settings.md](settings.md).

Either way it is a normal tmux client with its own terminal, clearly apart
from Dreamer's pane rendering. It does not count as Dreamer drawing these
screens.

## Decorations

Lines that a [filter](filters.md#decorations) marks are drawn over the
grid:

| Style | Drawn as |
| --- | --- |
| `highlight` | A wash in the accent color across the line |
| `dim` | The terminal's background over the line, partly see-through |
| `fold` | The line covered, with `⋯`; the row stays, so nothing below moves |
| `badge` | The badge text at the line's right edge (the filter's name if it has none) |

A mark is drawn only while the line it names still holds the marked text
at the width it was marked at. A view that has not caught up, a reflowed
pane, or a line that has scrolled away draws nothing rather than marking
the wrong line. "Show or hide decorations in this pane" in the palette
switches them per pane, in this window only. Drawing never changes what
the program wrote, the daemon's mirror, or what tmux shows; copying text
copies the line as written.

The workspace lists marks for the panes it shows when a
`decorations.changed` event names them. Panes of a host opened through
the coordinator show none: the host's events are not relayed.

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
  window on it. A host that failed on authentication or its host key says
  "click to verify": the click opens a local tmux window running
  `dream hosts verify NAME`, where SSH asks its questions
  ([remote.md](remote.md#first-connection)). The list is read once a second, and only while the sidebar
  is open.
- **Palette.** "Open host…", "Reconnect host…", "Verify host in a
  terminal…", and "Disconnect host…" take the host's name. Disconnecting closes SSH; the host's daemon, tmux,
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

**Profiles.** "Add a host profile…", "Edit a host profile…", and "Remove a
host profile…" in the palette change `hosts.yaml`
([remote.md](remote.md#profiles)). Add and edit open a short form, one
field at a time:

- name (required);
- SSH destination (required);
- remote `dream` path (blank: the installed one, else `PATH`);
- remote tmux socket name (blank: the default);
- tags, comma separated.

Enter moves to the next field, Up and Down move between fields, and the
last Enter saves. The new file is checked with the same rules as reading
it before anything is written. A refused profile reopens the form with the
reason, and the file stays as it was. Saving keeps the file's leading
comments and the order of the profiles. Settings the form does not show
(the remote tmux executable, a socket path, the workspace, the reconnect
policy) are kept from the profile being edited. Removing a profile never
touches the host. The daemon reads the file on use, so changes need no
restart. Comments between profiles are not kept.

The `remote` gate stage (macOS) and the Linux desktop gate run
`dreamer --smoke-remote --host devbox` against a second isolated tmux and
daemon behind a stand-in `ssh`.

## App panels

An [app package](termapp.md) can be shown as a native panel beside the
panes, as well as running as a pane app. "Open an app package as a
panel…" in the palette asks for the manifest's path (`~/` works). The
desktop draws the package's view itself (table, list, log, text, form,
gauge, or chart) from the same model `dreamer-termapp` draws in a pane,
the `app-model` crate. Keys typed while the panel has focus (click it)
go to the app, with the same names and the same commands as in a pane.
Cmd and Alt shortcuts stay with the desktop.

The model runs on a thread of its own, with its own connection to the
local daemon, and reloads the package when its files change. That
connection gives up the user's grant at once and keeps only the
package's `capabilities` (plus `events.subscribe` when a source follows
events), with every pane in scope (see [grants.md](grants.md)). So a
panel can do no more than its manifest declares, and a reload cannot
widen that; reopen the panel after adding a capability.
Nothing from the package runs on the UI thread or while drawing. `q`
(or Esc in a form) ends the app; "Close the app panel" removes it. One
panel is shown at a time.

### Contributions

Installed app packages can add palette commands (with keybindings),
sidebar entries, and status items; see
[termapp.md](termapp.md#contributions). Contributed commands follow the
built-in ones in the palette, labelled `<package title>: <command>`.
Sidebar entries and refused packages are listed under Apps in the
sidebar. Status items show at the right of the tab strip, followed by
the last contributed command's outcome. Contributions apply to local
windows only.

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
| Show or hide decorations in this pane (filter marks) | Switches drawing [decorations](#decorations) for the focused pane, in this window |
| Open an app package as a panel…, Close the app panel | Shows an [app panel](#app-panels), or removes it |

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
- installed packages (in a private `DREAMER_CONFIG_DIR`): one package's
  palette command renames a window through its typed action, its
  keybinding opens it as a panel, and its sidebar entry and status item
  are there; a second package with the same id and an invalid one are
  refused with reasons;
- clicking a pane focuses it, and typed text reaches only that pane;
- with a refreshing display, a filter's mark on a line of the focused
  pane is drawn, hidden from the palette, and drawn again, while
  `tmux capture-pane` still shows the line as written;
- with a refreshing display, one app package runs as a pane app in a
  tmux window and as a panel: both show the same rows, a key pressed in
  the panel moves its selection, and the panel closes from the palette;
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
containing TEXT. It then waits for the pane to settle (no new frame for
0.5 s), watches 4.5 s of idle pane, and exits with status 3 if any frame
woke the window or it painted more than once. One stray paint is allowed:
macOS has GPUI repaint a background window now and then for activity
elsewhere on the desktop, which the pane did not cause; a repaint loop
paints more often. A passing count is then checked by requesting one
repaint as a control: if the window does not paint (occluded, or the
display asleep), the result is status 5, not a pass. The `live` step runs
this against an isolated tmux server and private daemon directories.
