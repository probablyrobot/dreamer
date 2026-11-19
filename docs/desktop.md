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
  normal: "connecting…", "reconnecting: why", "could not start dreamerd:
  why" (see [daemon.md](daemon.md#files)), or "N panes catching up"
  while panes resynchronize.
- **Saved arrangement.** Every open window is remembered: the session it
  shows, whether its sidebar is open, and where it was on screen. The next
  launch opens all of them again, each on its own session. A window you
  close is not reopened, unless it was the last one open; quitting keeps
  them all. The list is kept in `desktop.yaml` in the desktop state
  directory: `$DREAMER_DESKTOP_STATE_DIR`, else `$XDG_STATE_HOME/dreamer`,
  else `~/.local/state/dreamer`. That is apart from preferences
  (`dreamer.yaml`) and from everything on the host. The app writes the
  file off the UI thread, one write at a time, and every write carries
  every window's entry, so windows saving at once never drop each other's.
  A file from an earlier version, which held one window, opens that one
  window. You are not meant to edit the file.
- **No stale ids.** tmux ids belong to one run of a tmux server (its
  incarnation, reported in the topology). A saved session id is reused
  only for the same incarnation. After the server restarts, the session is
  found again by name, and otherwise the first session is shown. An id
  from another server is never reopened (spec 15.2).

## The harness drawer

The drawer is the agent harness (pi by default; see
[agents.md](agents.md#the-harness-drawer)) in an ordinary tmux pane that
slides up at the bottom of the window and away again. Each tmux session
has its own. Nothing exists until the first use.

| Key (macOS) | Key (Linux) | Does |
| --- | --- | --- |
| Cmd-J | Ctrl-Shift-J | Hidden: show it and focus it. Shown but not focused: focus it. Focused: hide it, and focus the pane you came from |
| Cmd-Shift-J | Ctrl-Alt-Shift-J | Start a fresh harness in the drawer. The running one ends |

The palette has both: "Harness drawer: show, focus, or hide", and "New
harness drawer (ends the running harness)…", which asks for a second
Enter.

- **Showing** docks the pane into the window you are looking at, across
  its whole width, at the bottom, taking 35 % of the height (`drawer.side`
  and `drawer.size` in [settings](settings.md#harness-drawer)). The other
  panes reflow as tmux lays them out, and every tmux client sees the same
  layout. A window that would keep fewer than 8 rows (or 20 columns, on a
  side) for its other panes shows the drawer zoomed instead.
- **Hiding** moves the pane to a holding session, `_dreamer_drawers`. The
  harness keeps running there. Showing it again brings back the same pane
  with everything in it. Tabs, the sidebar, and the session chosen at
  startup leave the holding session out.
- **In another window,** the key brings the drawer to the window you are
  looking at.
- **The pane you came from** is the pane that had focus when the drawer
  was shown, or when the key moved focus from it to the shown drawer.
  Hiding focuses it again; it stays recorded while the drawer is hidden,
  until the next show. `drawer.status` reports it as `from_pane`, and pi's
  `@pane` attaches its text ([agents.md](agents.md#pane-context-and-send-to-pane)).
- The daemon does the moving (`drawer.toggle`); the window follows the
  topology. `dream drawer` does the same from a shell
  ([cli.md](cli.md#harness-drawer)), for a tmux key of your own. Dreamer
  never changes tmux's configuration.

## Windows

Each desktop window shows one tmux session, and windows behave as a
terminal's do:
- **New Window** (Cmd-N, Ctrl-Shift-N) opens another window on a **new
  tmux session**: a fresh shell in your home directory, in a session tmux
  names. It never reuses a session another window shows. The new window
  opens a little below and to the right of the active one.
- **Tabs are tmux windows.** New Tab (Cmd-T) adds a tmux window to this
  window's session; Next Tab and Previous Tab switch between them.
- **Two windows show the same session** only if you pick it in one of
  them from the sidebar. They then show the same tmux windows and panes.
- **Close Window** (Cmd-W, Ctrl-Shift-W) closes the desktop window only.
  Its tmux session, panes, and programs keep running, and the sidebar of
  any window can show the session again. Quitting closes every window the
  same way, and the daemon keeps running.
- On Linux the app quits when its last window closes. On macOS it stays in
  the Dock, and clicking the icon opens a new window.
- The window title follows the focused pane's title.
- `window.decorations: none` in [settings](settings.md#window-decorations)
  hides the title bar and the close, zoom, and minimize buttons; the tab
  strip still moves the window.

## Menus and shortcuts

Commands, their shortcuts, and the macOS menu bar come from one table
(`crates/app/src/keymap.rs`). macOS uses Cmd; Linux uses Ctrl-Shift, so
plain Ctrl and Alt combinations (Ctrl-C, Ctrl-W, Ctrl-Q, …) always reach
the program in the pane. Every menu command is also in the
[command palette](#command-palette), so a command with no Linux shortcut
is still a few keystrokes away.

| Command | macOS | Linux |
| --- | --- | --- |
| About Dreamer (name and version) | (menu) | (palette) |
| Settings…: open `dreamer.yaml` | Cmd-, | (palette) |
| Quit | Cmd-Q | Ctrl-Shift-Q |
| New window (a new session) | Cmd-N | Ctrl-Shift-N |
| New tab (a tmux window in this session) | Cmd-T | Ctrl-Shift-T |
| Split right | Cmd-D | Ctrl-Shift-O |
| Split down | Cmd-Shift-D | Ctrl-Shift-E |
| Close window | Cmd-W | Ctrl-Shift-W |
| Next tab, previous tab | Cmd-Shift-], Cmd-Shift-[ | Ctrl-Shift-PageDown, Ctrl-Shift-PageUp |
| Copy the selection | Cmd-C | Ctrl-Shift-C |
| Paste | Cmd-V | Ctrl-Shift-V |
| Cut, select all (in a [web pane](#web-panes) only) | Cmd-X, Cmd-A | Ctrl-Shift-X, Ctrl-Shift-A |
| Find in the focused pane | Cmd-F | Ctrl-Shift-F |
| Command palette | Cmd-Shift-P | Ctrl-Shift-P |
| Sessions sidebar | Cmd-Shift-S | Ctrl-Shift-S |
| Harness drawer: show, focus, or hide | Cmd-J | Ctrl-Shift-J |
| New harness drawer | Cmd-Shift-J | Ctrl-Alt-Shift-J |
| Zoom the focused pane | Cmd-Shift-Enter | Ctrl-Shift-Enter |
| Bigger text, smaller text, actual size | Cmd-=, Cmd--, Cmd-0 | (palette) |
| Full screen | Ctrl-Cmd-F | F11 |
| Hide, hide others | Cmd-H, Alt-Cmd-H | (window manager) |
| Minimize | Cmd-M | (window manager) |

Bigger and smaller text change the terminal font by one point (between 6
and 72) in every window, without changing `dreamer.yaml`; actual size
returns to `font.size`. Settings… opens `dreamer.yaml` in your editor
(`open -t` on macOS, `xdg-open` on Linux). If the file does not exist, it
is created first with no settings in it, only comments: every setting
takes its default, and `dream config schema` lists them.

The macOS menu bar is short:
- **Dreamer:** About Dreamer, Settings…, Services, Hide Dreamer, Hide
  Others, Show All, Quit Dreamer.
- **Shell:** New Window, New Tab, Split Right, Split Down, Close Window.
- **Edit:** Copy, Paste, Find….
- **View:** Command Palette, Sessions Sidebar, Harness Drawer, Zoom Pane,
  Bigger, Smaller, Actual Size, Enter Full Screen.
- **Window:** Minimize, Zoom, Next Tab, Previous Tab, Bring All to Front,
  and the list of open windows, which macOS keeps.

Each item shows its shortcut. Linux has no menu bar.

### Context menu

A right click in a terminal pane opens a small menu, drawn in the theme:
Copy, Paste, New Window, New Tab, Split Right, Split Down, and Command
Palette, each with its shortcut. When the program in the pane has turned
on mouse reporting, a right click goes to the program instead, and
Shift-right-click opens the menu. A click elsewhere or any key closes it.
It is the same on macOS and Linux. App panes draw their own views and
have no context menu.

## Find

Cmd-F (Ctrl-Shift-F on Linux) opens a find bar over the focused pane. It
searches that pane's history and screen in the window's own mirror. The
daemon is not asked, and nothing is sent from another machine.

- **Typing** is the query, and the search runs after a short pause. While
  the bar is open, no key reaches the program.
- **Case** matters only when the query has a capital letter. Alt-R
  switches to a regular expression (shown as `.*`), matched in linear time.
- **Moving:** Enter or Up goes to the previous (older) match, and
  Shift-Enter or Down to the next. Each move scrolls the match into view.
  The bar shows `3 of 17`.
- **Highlights:** matches in view are highlighted, and the current one is
  outlined in the accent color. They are view state, like a selection:
  the terminal and what tmux shows are unchanged.
- **New output:** when output arrives, or the pane is resized or
  re-checkpointed, the search runs again. The current match is kept if it
  still exists, without jumping.
- **Escape** closes the bar and clears the highlights.

**Bounds**
- The newest 100,000 rows and 16 MiB of text are searched, and at most
  5,000 matches are kept. When a bound cuts the search short, the count
  shows `+`.
- The matching runs on a thread of its own, never on the UI thread or the
  thread that forwards input. A new query stops the search before it.
- A pattern the match stage refuses (over 4 KiB, or too large to compile)
  is shown in the bar as the reason.

Search across panes and hosts is `terminal.query` ([taps.md](taps.md#queries)),
bounded on the daemon. An inspector pane uses it.

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
the `app-model` crate. Input while the panel has focus (click it)
goes to the app as described in [Input in app views](#input-in-app-views),
with the same names and the same commands as in a pane.

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

### App panes

A pane whose program is a registered app host (an app package started
with `dream app launch` or `dream app run`, see
[termapp.md](termapp.md#app-panes)) is drawn natively, in the pane's own
cell rectangle, instead of its terminal: the same table, list, log,
text, form, gauge, chart, or agent view as a panel, and the text, input,
and status components of a view tree
([termapp.md](termapp.md#text-input-and-status-components)). Any number of app
panes can be shown, in any window, and they follow splits, resizes,
zoom, and tab switches like terminal panes, because tmux still owns the
pane and its layout. A plain tmux client attached to the same session
sees the app's text interface in that pane.

The desktop runs none of the package. Each app pane's view has a thread
of its own that follows the view tree the pane's host publishes through
the daemon, the whole tree once and then only what changed
([topology.md](topology.md#app-panes)), and sends the input made while
the pane has focus back to the host (`apps.input`), where keys act
exactly as keys typed in the pane.

Focus moves between terminal panes and app panes the same way:

- A click on a pane focuses it and makes it tmux's active pane.
- When tmux selects another pane of the window shown, focus follows:
  `select-pane` from any client, the palette's "Focus next pane", or
  tmux's prefix and an arrow typed here. In an app pane the prefix works
  as in a terminal pane: the key after it runs your tmux binding for it,
  and the prefix and `]` pastes tmux's newest buffer into the focused
  input.
- Dreamer's shortcuts (the palette, zoom, the drawer, Find, and the rest
  in [Menus and shortcuts](#menus-and-shortcuts)) work whichever kind of
  pane has focus.

The desktop draws the tree's layout ([termapp.md](termapp.md#view-trees))
natively: splits by their sizes with a divider, tabs with the active
page, titled nodes in a frame that is highlighted while the node has
the focus.

A `terminal` node ([termapp.md](termapp.md#terminal-and-image-components))
is drawn with the same terminal renderer as a terminal pane, at the
terminal font, from the desktop's own replica of the pane. The pane is
followed on a thread of its own, as a terminal pane is, while the node is
shown (not on a hidden tab). It is read only: keys, clicks, and the
wheel never reach the pane shown. The pane is drawn from its top left
and cut off at the node's edges. A node the host marked refused shows
`refused:` and why, and the desktop does not follow that pane.

An `image` node's file is read and decoded off the UI thread, then drawn
at the image's size, shrunk to fit the node. Until then the node shows
the image's name; when the file cannot be decoded, why. The desktop only
reads the file the host resolved in the app's package, and only a PNG
or JPEG of at most 8 MiB. Each table, list, and scroll node keeps its scroll position
by its key when the tree changes, and a table or list scrolls to its
selection only when the selection moves. A host that publishes no tree
(an older one) is drawn from its model as before.

A `transcript` or `composer` bound to an agent session, and a `log`
bound to a tap ([termapp.md](termapp.md#bound-streams)), are drawn from
the stream itself: the pane's view thread follows it on the desktop's
connection for the app (`for_pane`), so the daemon checks the app's
grant too, and the tokens never pass through the host. The transcript is
drawn as cards (your prompts, the answer as markdown as it streams,
thinking folded until clicked, tool calls with their output, approval
requests with a button per answer, the plan) above its state and usage,
and follows the newest while it is scrolled to the bottom; it builds at
most its newest 600 lines per frame. An approval card's buttons answer
for the app (`agent.sessions.approve` or `deny` with `for_pane`), which
its grant must allow. The composer has Send, Stop while a turn runs,
Attach selection, and its attachments, each removed with a click;
Shift-Enter starts a new line.

When the host stops (the pane stays open, for example with
`remain-on-exit`), or the daemon cannot be reached, the pane keeps
showing the last view, with `stale:` and the reason in its status line.
When a pane's program becomes an app host, or stops being one, its view
is replaced on the next topology update. App panes are drawn for local
windows only; a remote host's app panes show their terminals.

A live preview (`dream app preview DIR --open`,
[termapp.md](termapp.md#live-preview)) is an app pane like any other:
its host runs the app from its fixtures and starts it again when the
package changes, and the pane follows the new tree.

The desktop also makes images of panes for `dream app preview --png`.
Each desktop process waits for capture requests from the local daemon
on a connection of its own ([topology.md](topology.md#images-of-app-panes)).
For a request, the UI thread only finds the window that shows the pane
and the pane's rectangle in it, and waits (on a timer, never blocking)
until the frame drawn has the view tree version asked for, up to 5 s.
Then, on a thread of its own, it captures the window with the system's
`screencapture -l` (macOS) and writes the pane's rectangle as a PNG at
the display's resolution. A desktop that does not show the pane, or a
platform other than macOS, answers why instead.

### Input in app views

App panes and app panels take input through the same policy as terminal
panes:

| Input | In an app view |
| --- | --- |
| Arrows, Enter, Esc, Tab, Backspace, Delete, Home, End, Page Up/Down, F-keys, Ctrl combinations | A key by the model's names (`Up`, `Enter`, `Ctrl-c`) |
| Text, Space included | Comes through the platform's text input, so IME composition works: the composition is shown at the bottom of the view and only committed text is sent. It goes into the focused input as one edit (a text or number field, an `input` or `composer` component, or an agent's prompt); in a view with no input it is the keys it is made of (`j`, `q`, a command's key) |
| Cmd-C (macOS), Ctrl-Shift-C (Linux) | Copies the lines selected with the mouse; with none, the selected row of the focused table or list (a table's columns separated by tabs), the focused form field or `input`, the focused `select`'s option, a text view's text, or an agent view's latest answer. The copy goes to the clipboard and, with `clipboard.tmux` `sync` or `to_tmux`, becomes tmux's newest buffer |
| Cmd-V (macOS), Ctrl-Shift-V (Linux) | Pastes the clipboard into the focused input as one edit: line breaks become spaces, other control characters are dropped. A view with no focused input takes no paste, so a paste never runs a command |
| Click | Selects a table's or list's row, or focuses a form field; presses a `button`, flips a `toggle`, picks a `select`'s option, or focuses an `input`. In a [view tree](termapp.md#view-trees), the node clicked takes the focus |
| Drag over lines | Selects whole lines (rows, form values, transcript entries, a text view's text, lines of markdown, code, or a diff) of one node for Copy, drawn highlighted. Selection is by line: a part of a line cannot be selected |
| Middle click | Pastes tmux's newest buffer into the focused input (`clipboard.middle_click: off` turns it off) |
| Wheel | Moves the selection of the table or list under the pointer (which takes the focus), one row per step |
| Cmd and Alt combinations | Stay with the desktop |

A pane's host gets text, clicks, and wheel steps as `apps.input` events
with `text`, `select`, or `scroll` instead of `key`; a click or wheel
steps also carry `node`, the key of the tree node they landed on
([termapp.md](termapp.md#app-panes)).

### Contributions

Installed app packages can add palette commands (with keybindings),
sidebar entries, and status items; see
[termapp.md](termapp.md#contributions). Contributed commands follow the
built-in ones in the palette, labelled `<package title>: <command>`.
Sidebar entries and refused packages are listed under Apps in the
sidebar. Status items show at the right of the tab strip, followed by
the last contributed command's outcome. Contributions apply to local
windows only.

## Web panes

macOS only so far, and only with [Dreamer Web](#dreamer-web) added. A
web pane shows one web page in a pane's rectangle, drawn by Chromium
itself, and is
the [browser](#the-browser): its chrome shows only when asked for. In tmux
it is an ordinary pane, so splits, resizes, zoom, moving it, and keeping
it across desktop restarts all come from tmux.

- **Opening one.** "Open a web page…" in the palette asks for a URL and
  opens it in a pane beside the focused one; `dream web open URL` and
  `dream browser ADDRESS` ([cli.md](cli.md#web-panes)) do the same from a
  shell. `http`, `https`, `file`, and `data` URLs and `about:blank` are
  accepted, and addresses as typed into the [browser](#the-browser)'s
  address bar; others are refused.
- **What tmux holds.** The pane runs a small placeholder program that
  prints the page's title, its address, and "Open in Dreamer to see the
  page", which is what a plain tmux client shows, and carries a pane
  option, `@dreamer_surface` = `web:<URL>`. The daemon reports it in the
  topology as the pane's `web` URL. The desktop drawing the page keeps
  its title (as the pane's title, and in the placeholder's text), its
  address when the page went elsewhere (a link, back), and its scroll
  position (`@dreamer_web_scroll`) there: when the page leaves the
  screen, and every few seconds while it is shown. When the address
  there changes otherwise (`dream browser --pane`), the page loads it.
- **What the desktop draws.** Chromium's own view, placed over the pane's
  rectangle and moved with it on every layout change: splits, resizes,
  zoom. It is hidden, not closed, while the pane is not shown (another
  tab, or another pane zoomed). A native view is drawn above anything
  Dreamer draws, so while the command palette or the context menu is
  open, web panes show a snapshot of the page taken as it opened, and the
  live page comes back when it closes. Pages are drawn in sRGB, as the
  terminals are, so the snapshot has the live page's colors.
- **Focus.** Clicking the page focuses its pane, here and in tmux; keys
  then go to the page. Focusing another pane (a click, the palette, the
  find bar, the drawer, tmux's prefix keys) takes the keys back, and
  focusing the web pane again gives them to the page.
- **Input.** The mouse, the wheel, trackpad gestures, keys, and input
  methods go to Chromium as in Chrome. Dreamer's own shortcuts still
  work while a page
  has the keys: Cmd+J shows the drawer from inside a page, for example.
  See [Menus and shortcuts](#menus-and-shortcuts).
- **Copy and paste.** In a page, Cmd+C, Cmd+X, Cmd+V, and Cmd+A
  (Ctrl+Shift+C, X, V, and A on Linux) are Chromium's own copy, cut,
  paste, and select all, so the clipboard, rich and HTML contents
  included, works as in Chrome. Cmd+X and Cmd+A are taken only in a page;
  elsewhere they still reach the program in the pane.
- **Where pages run.** On this Mac, in the desktop's process and
  Chromium's helper processes, which run from Dreamer Web's helper apps;
  Chromium's profiles are in `web/` in the desktop's state directory. See
  [Pages of a remote host](#pages-of-a-remote-host) for web panes in a
  host's window.
- **Lifetime.** The page lives while a desktop window shows its pane (or
  kept it among its recently shown panes) and loads again when the pane
  is shown after that, after a desktop restart for example, scrolled back
  to where tmux keeps it. Closing the pane closes the page. Chromium
  starts with the first web pane, not with the app.

### Pages of a remote host

A web pane in a remote host's window shows its page here too, on this
machine. A page on the host's own loopback (`http://localhost:3000`,
`127.0.0.1`, `[::1]`) is reached through a forward over the SSH
connection the coordinator already holds
([remote.md](remote.md#forwards)): no new SSH connection, and nothing
listening on any network.

- **Addresses.** Chromium loads the page from the forward's port on this
  machine's 127.0.0.1 (`http://localhost:49731/` for the host's
  `http://localhost:3000/`). Everything else keeps the address as the host
  means it: the address bar, what the placeholder and tmux keep, and
  `dream browser --pane`. Only the forwarded port is translated; a link
  to another loopback port loads this machine's. The note under the page
  names the host (`web: http://localhost:3000/ on devbox`).
- **Who may.** The desktop holds the owner grant, so it forwards any page
  of a host's window the user looks at. An agent or app that opens or
  loads such a page on a host needs `web.forward` in its grant
  ([grants.md](grants.md#reaching-a-hosts-loopback)).
- **Lifetime.** The pane holds the forward while its page is held (shown,
  or among recently shown panes), on a connection of its own to the local
  daemon; when no pane holds it, the forward and every connection through
  it close. A page moved to another port gets a new forward.
- **Not drawn.** A `file:` page in a host's window is the host's file, so
  the pane shows the placeholder's text instead.

### The browser

A web pane is a browser for sites that are not packaged as apps, with as
little chrome as a terminal: the page fills the pane, and a thin strip
pulls down over its top (moving the page down) only while it has
something to show.

| | macOS | Linux |
| --- | --- | --- |
| Address bar: show it, the address selected | Cmd+L | Ctrl+Shift+L |
| Load what is typed, or hide the bar | Enter, Esc | Enter, Esc |
| Back, forward | Cmd+[, Cmd+]; mouse buttons 4 and 5; a two-finger swipe | Alt+Left, Alt+Right; mouse buttons 4 and 5 |
| Reload; stop a load | Cmd+R; Esc in the page | Ctrl+Shift+R; Esc in the page |
| Find in the page (Enter next, Shift+Enter previous, Esc ends it) | Cmd+F | Ctrl+Shift+F |
| Zoom the page in, out, back | Cmd+=, Cmd+-, Cmd+0 | Ctrl+=, Ctrl+-, Ctrl+0 |

- **Addresses.** The bar takes a URL or an address as typed: `example.com`
  goes to `https://example.com`, `localhost:3000` (or anything with a
  port, or a loopback address) to `http:`, an absolute path to a `file:`
  URL. A search is refused: there is no search engine. A page that fails
  to load shows the bar again, with the error.
- **One page per pane.** No tabs, bookmarks, or history list; the back
  stack is the pane's own. A link that wants a new window opens in the
  same pane.
- **Logins.** The browser has a profile of its own (`web/browser/`),
  apart from your own browser's, and keeps session cookies in it, so you
  stay logged in across restarts. `dream browser clear`
  ([cli.md](cli.md#web-panes)) empties it.
- **Downloads** are refused for now, with a line in the strip naming the
  file. Saving them is planned, behind a `web.download` capability a
  package cannot ask for yet.
- **No Dreamer API.** A site in the browser never gets `window.dreamer`.
  The script that turns the mouse's side buttons and the swipe into back
  and forward runs in a world of its own, which the page cannot see.
- **From the command line and agents.** `dream browser ADDRESS` opens an
  address in a new pane, and `dream browser ADDRESS --pane %N` in an
  existing one ([cli.md](cli.md#web-panes)); agents have the same as
  `browser_open` ([mcp.md](mcp.md#browser-tool)).

### App pages

A `web` node of an app pane, or a web app's whole view
([termapp.md](termapp.md#web-pages-and-web-apps)), is drawn the same
way: Chromium's view over the node's place in the pane, hidden and
covered like a web pane's. What is different:

- **The app's own.** The page opens in a browser profile of the app's
  (`web/app-<app id>/` in the desktop's state directory). It may
  navigate only to the app's origin and the origins its manifest allows;
  any other navigation is refused and the page stays.
- **The package's files** are served on `dreamer-app://<app id>/` from
  the package's `web/` directory, by the desktop itself: nothing listens
  on a port.
- **The bridge.** A page on the app's origin gets `window.dreamer.call`.
  The desktop answers it only for frames on that origin, and passes each
  call to the daemon as `apps.call` for the pane, so it runs as the app,
  under the app's grant ([grants.md](grants.md#web-pages-of-apps)).
- **Keys.** A web app's page takes the keys while its pane has the
  focus. In an app with other nodes, a click in the page gives it the
  keys, and a click on the rest of the app takes them back.
- Its title and scroll position are not kept in tmux.

Not yet: a page's own clipboard writes following `clipboard.write`, the
Linux primary selection, more than one Mac display scale, Chromium's
sandbox, signed helper apps, Linux.

### Agents and pages

An agent whose grant holds `web.inspect`
([grants.md](grants.md#reading-and-driving-pages)) reads and drives the
page of a web pane, or of a web app's pane, through the daemon. The
daemon hands each call to the desktops connected, as it does an app
pane's image (`apps.capture`); the window that shows the pane runs it on
the page, through Chromium's DevTools protocol, and answers:

| Method | Does |
| --- | --- |
| `web.read` | The page's address, title, and text (the body's `innerText`, at most 100,000 characters), and with `tree` its accessibility tree in document order: each node's id, depth, role, name, and value, without ignored, generic, and inline text nodes, at most 2,000 nodes |
| `web.click` | Scrolls the element into view and clicks the middle of its first box with the left button, as a mouse would |
| `web.type` | Focuses the element, if one is named, inserts the text at the caret as typed, and with `submit` presses Enter, which submits a form from its field |
| `web.screenshot` | Writes the page as Chromium draws it, alone and at the display's scale, as a PNG file |

- **Naming an element.** `node` is a node id from `web.read`'s tree (the
  element's DevTools backend node id, which stays the element's while
  the page is loaded), or `selector` is a CSS selector, whose first match
  is taken. A selector is passed to `document.querySelector` as a
  string, never as code.
- **Where.** The pane must be shown in a Dreamer window (the active
  window of its session); otherwise the desktop says so, and with no
  desktop running the call fails at once. A page still loading is waited
  for, up to 10 seconds.
- **Nothing else.** Only these DevTools methods run, with parameters
  Dreamer builds; the clipboard is never used.
- **Previews.** `dream app preview --png` of a web app
  ([termapp.md](termapp.md#live-preview)), and `apps.capture` of any pane
  that shows a page, is the page's own image from Chromium, the same as
  `web.screenshot`.

MCP has these as `web_read`, `web_click`, `web_type`, and
`web_screenshot` ([mcp.md](mcp.md#page-tools)), and so does Dreamer's pi
extension ([agents.md](agents.md#dreamers-tools-inside-pi)).

### Dreamer Web

Chromium is not in the Dreamer package. It is a separate component,
Dreamer Web, so an install that only wants terminals stays small: the
Dreamer zip is the same with or without it, and Dreamer Web is about
150 MB to download and 330 MB installed. Add it once:

```sh
dream web install
```

This downloads the Dreamer Web zip of your Dreamer's version from the
release, checks its SHA-256, checks the component, and installs it in
`web/` of Dreamer's data directory (`~/.local/share/dreamer/web` unless
`DREAMER_DATA_DIR` or `XDG_DATA_HOME` says otherwise). With a zip you
already have, give its path: `dream web install
dreamer-web-<version>-aarch64-apple-darwin.zip`. See
[cli.md](cli.md#dreamer-web) for `dream web status` and `dream web
remove`.

- **What it holds.** CEF's framework (`Chromium Embedded
  Framework.framework`), the five helper apps Chromium runs its renderer,
  GPU, and utility processes from (`dreamer Helper.app`, `dreamer Helper
  (GPU).app`, `(Renderer)`, `(Plugin)`, `(Alerts)`), each signed, CEF's
  license and Chromium's notices (`LICENSE.txt`, `CREDITS.html`), and
  `component.json`, which names the Dreamer version and CEF release it
  was built for.
- **Loaded on first use.** Chromium starts with the first web pane, from
  the installed component, not with the app.
- **Without it,** a web pane shows its address and says that web pages
  need Dreamer Web and to run `dream web install`; the rest of Dreamer
  works as before. A pane opened after you add it draws its page; there
  is no need to restart.
- **One version.** The helper apps are built with Dreamer, so Dreamer Web
  must be the one of the same Dreamer version (and CEF release). After
  updating Dreamer, run `dream web install` again; until then web panes
  say which version is installed and which is needed.
- **Signing.** Like the app, the component is signed ad hoc, not with a
  Developer ID or notarized. `dream web install` downloads with `curl`,
  which does not mark the file as quarantined. A zip that a browser
  downloaded is quarantined, the mark carries over to what is unpacked
  from it, and Gatekeeper may then stop Chromium's helper apps from
  starting. Install such a zip with `dream web install` and no file
  instead, or remove the mark from the zip yourself (`xattr -d
  com.apple.quarantine FILE`) if you trust where it came from.

### A build with web panes

To build Dreamer with web panes yourself:

```sh
cargo xtask deps web                  # the pinned CEF, cmake, and Ninja, checksum-verified
cargo xtask web bundle [--install]    # in target/web-bundle (add --release for a release build)
```

`target/web-bundle` then holds `Dreamer.app`, the app built with its
`web` feature (without Chromium), and `dreamer-web/`, the Dreamer Web
component built from the same tree, its helper apps signed ad hoc.
`--install` installs that component with the bundle's own `dream web
install` (or run `dream web install target/web-bundle/dreamer-web`
later); then run `Dreamer.app/Contents/MacOS/dreamer`. `cargo xtask
package` makes the release's two zips the same way
([packaging.md](packaging.md#dreamer-web)). See
[dependency-baseline.md](dependency-baseline.md#web-engine) for the pins.

## Command palette

Cmd-Shift-P on macOS, or Ctrl-Shift-P on Linux, opens the command palette;
the shortcut is never sent to the pane. Type letters to filter (each one,
in order, anywhere in a command's name). Up and Down move, Enter runs, and
Escape closes.

| Command | Does |
| --- | --- |
| Split pane right, Split pane down | Splits the focused pane (`split_pane`) |
| New tab (tmux window) | Adds a tmux window at the end of the session (`new_window`) |
| Rename window… | Asks for a name, then renames the shown tmux window |
| Next tab, Previous tab (tmux window) | Switches tmux windows, wrapping around |
| Focus next pane | Moves focus here and in tmux |
| Zoom or unzoom pane | Toggles zoom |
| Harness drawer: show, focus, or hide | As Cmd-J ([the harness drawer](#the-harness-drawer)) |
| New harness drawer (ends the running harness)… | Destructive: a second Enter starts a fresh harness in the drawer |
| Close view (programs keep running) | Closes this desktop window only; tmux and every program keep running |
| Kill pane (ends its program)…, Kill window (ends its programs)… | Destructive. Shown in red; the first Enter only asks, a second Enter runs it, and Backspace backs out |
| Show or hide decorations in this pane (filter marks) | Switches drawing [decorations](#decorations) for the focused pane, in this window |
| Open an app package as a panel…, Close the app panel | Shows an [app panel](#app-panels), or removes it |
| Open a web page… | Asks for a URL and opens it in a [web pane](#web-panes) beside the focused pane |
| Theme… (colors of the terminals and the window) | Lists every theme with a swatch and previews the highlighted one; Enter keeps it in `dreamer.yaml`, Escape puts the file back ([settings](settings.md#terminal)) |
| New Dreamer window (on a new session), Settings…, About Dreamer, Copy, Paste, Find in pane…, Bigger text, Smaller text, Actual text size, Full screen, Minimize window, Zoom window, Bring all windows to front, Quit Dreamer | The [menu commands](#menus-and-shortcuts) |

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
  second session, and the arrangement is saved;
- New Window opens a second window on a session that did not exist
  before, the first window keeps its session, and both windows are saved.

The gate then launches the app a second time, against a new tmux server.
A session `decoy` there now has the old id of `other`. The app must come
back on `other`, matched by name rather than by the stale id, with the
sidebar open, and must open every saved window (the second one's session
is gone on the new server, so it shows the first session).

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
| `clipboard` | The [clipboard check](#clipboard-check) | A window that is being drawn |
| `screens` | [Pixel evidence](#pixel-evidence) | A display that keeps refreshing, and Screen Recording permission |
| `preview` | The [preview check](#preview-check) | A display that keeps refreshing, and Screen Recording permission |
| `web` | The [web pane check](#web-pane-check) | `cargo xtask deps web`, and a display that keeps refreshing |

A locked screen still paints a window's first frame, but macOS then stops
refreshing it. `cache` and `live` are recorded as not run while the screen
is locked. If a requested repaint does not happen, the app itself exits
with status 5 ("inconclusive"), so a count of zero paints is never taken
as evidence when nothing could have painted.

`dreamer --smoke --expect TEXT --attach %PANE …` waits for a frame
containing TEXT. It then waits for the pane to settle (no new frame for
0.5 s), watches 4.5 s of idle pane, and exits with status 3 if any frame
woke the window. Repaints the pane did not ask for are not the pane's
work: macOS has GPUI repaint a window now and then for activity elsewhere
on the desktop. One is allowed. More, with no view wakeup, means the
desktop was busy (someone using the Mac), so the result is status 5, not
run. A repaint loop in the pane would show as view wakeups. A passing count is then checked by requesting one
repaint as a control: if the window does not paint (occluded, or the
display asleep), the result is status 5, not a pass. The `live` step runs
this against an isolated tmux server and private daemon directories.

### Clipboard check

`dreamer --smoke-clipboard` checks copy and paste in every kind of pane
there is, against an isolated tmux server, private daemon directories,
and a private configuration whose harness drawer runs a shell. A smoke
run never reads or replaces the clipboard you use: every desktop smoke
uses an in-memory clipboard and primary selection instead. Every gesture
is dispatched to the window as input, so the window's own input policy
decides what it does.

- **Terminal pane:** a drag, a double click, and a triple click select a
  part of a line, a word, and the line; Cmd-C (Ctrl-Shift-C) copies each.
  A finished selection becomes tmux's newest buffer at once and reaches
  the clipboard only on Copy, or on its own with `copy_on_select`. On
  Linux it is also the primary selection. Cmd-V (Ctrl-Shift-V) arrives as
  a bracketed paste in a program that asked for one; a middle click
  pastes tmux's newest buffer; a program's OSC 52 write reaches the
  clipboard.
- **tmux:** a buffer set by another tmux client reaches the clipboard
  once, and a Dreamer copy becomes tmux's newest buffer without coming
  back to the clipboard.
- **Harness drawer:** a drag and Copy copy from it, and Paste types into
  it, as in a terminal pane.
- **App pane:** a form app launched as a registered app and joined beside
  the terminal pane. A click focuses it; typed keys and an IME commit go
  into its first field; the palette shortcut opens the palette over it,
  and closing the palette leaves it focused; Tab moves to the second
  field; Paste and a middle click type into it; Copy takes the field to
  the clipboard and tmux's newest buffer; tmux's prefix and Left move
  focus to the terminal pane, and the prefix and Right back.
- **Web pane:** not checked: copy and paste in a page is Chromium's own,
  on the system clipboard, which a check never touches. The [web pane
  check](#web-pane-check) checks only that Cmd+A is Chromium's select
  all.

The macOS gate runs this as the `clipboard` step, and the Linux desktop
gate on each backend ([linux-desktop-gate.md](linux-desktop-gate.md)).
A window that is not being drawn (a locked screen, a display asleep)
exits with status 5 and the step is recorded as not run, never failed.

### Web pane check

When `cargo xtask deps web` has provisioned CEF, the macOS gate's `web`
step runs clippy on the desktop with its `web` feature, builds the
[bundle with web panes](#a-build-with-web-panes), checks that the
framework and the five helper apps are in it and that a helper loads the
framework, and then runs `dreamer --smoke-web DIR` from the bundle
against an isolated tmux server with private daemon, desktop, and
configuration directories:

- the smoke opens a `data:` page beside the session's shell pane: one
  screen of `#2a6fdb`, then three screens of `#d9822b`, and a title that
  counts animation frames and reports the scroll position;
- it waits until Chromium has loaded the page and drawn 30 frames, and
  captures the window (`before.png`);
- it sends a wheel event to the page (or, if CEF does not deliver it,
  scrolls by script and says so), waits for the scroll to settle, and
  captures again (`after.png`);
- it resizes the pane through tmux (`resized.png`) and zooms it
  (`zoomed.png`), waiting each time for the page to follow and draw;
- it opens a new tab (`tab.png`), then goes back to the page's tab,
  checking that the page was kept rather than loaded again (`back.png`);
- it opens the palette, waits for the pane to show the page's snapshot
  (`palette.png`), closes it, and waits for the live page
  (`closed.png`);
- then input, sent through AppKit as a person's would be: a click in the
  page's text field focuses the web pane (it starts on the shell pane);
  keys, a Delete, and an input method composing "日" and committing "日本"
  type `hi日本` into the field, read back through DevTools; Cmd+J pressed
  in the page shows and focuses the harness drawer (a shell in this
  check's configuration), and pressed again hides it and gives the keys
  back to the page; Cmd+A selects the whole field (its selection read
  back through DevTools), through Chromium's own select all. Copy, cut,
  and paste are not pressed: they use the system clipboard, which the
  gate never touches, so copy and paste in a page is not checked by the
  gate. Each passed input check is a `smoke: web input:` line, and the
  gate wants all five;
- then an [app page](#app-pages): the example web app
  (`examples/apps/panes-web`), granted `tmux.topology`, opens beside the
  shell pane. Its page lists the panes through the bridge, and a call
  through it lists the app's own pane (`app.png`); `terminal.list`,
  outside its grant, is refused with `-32005`, and so is `web.read`
  (its grant has no `web.inspect`); `apps.capture` of its pane, as
  `dream app preview --png` asks, writes its page's own image
  (`app-page.png`, mostly the page's `#1e1e1e`); a sandboxed frame
  inside it (another origin) has no `window.dreamer` and cannot reach
  the bridge, nor does the plain web pane have one; and a navigation to
  `https://example.invalid/`, outside its allow-list, is refused and the
  page stays on its origin. Each passed check is a `smoke: web app:`
  line, and the gate wants all seven;
- then the [browser](#the-browser), in the plain web pane, against a
  test site the gate serves on `127.0.0.1` (nothing from the internet):
  `web.navigate` loads its login, which sets a session cookie; Cmd+L,
  an address typed without a scheme, and Enter load its first page; a
  link is followed; Cmd+[ and Cmd+] go back and forward, and so do mouse
  buttons 4 and 5; Cmd+R loads the page again (the site counts its
  loads); Cmd+F and a word typed get matches from Chromium's find, and
  Esc ends it; Cmd+= zooms the page and Cmd+0 back; a download is
  refused with its status line; and a link that wants a new window
  opens in the same pane. Then the gate starts the desktop again on the
  same profile (`dreamer --smoke-web-restart`), and a page of the site
  must still get the login's session cookie. Each passed check is a
  `smoke: web browser:` line, and the gate wants all eleven. The swipe is
  not checked;
- then [an agent and a page](#agents-and-pages), through the daemon on
  connections narrowed to an agent's grant at `own`: the agent opens the
  test site's form in a window of its own; without `web.inspect`,
  `web.read` is refused; with it, `web.read` reads the form and finds its
  field in the tree, `web.type` fills the field by its node and presses
  Enter, `web.click` follows a link by its selector, and
  `web.screenshot` writes the page (`web-inspect.png`). Each passed check
  is a `smoke: web inspect:` line, and the gate wants all five, the site
  must have got the form with the name typed, and the screenshot must be
  mostly the form's `#3fa34d`;
- the gate checks each capture as the smoke's `checks.txt` lists: the
  page's color covers at least 90% of the pane's rectangle (of the
  zoomed pane, or of the part beside the palette) and at most 1% of the
  window left of it, none of it shows where the pane was while another
  tab is shown, and at most 5% of the palette's query line where it
  crosses the pane is the page's.

The captures and `checks.txt` are kept in `target/gate/web/`.
`cargo xtask web smoke` builds the bundle and runs the same check by
itself, keeping them in `target/web-smoke/`.
Without CEF provisioned, without a display that refreshes, when
Chromium stops drawing frames, or when the window is not the key window
(it cannot be typed into), the step is recorded as not run.

### Pixel evidence

The macOS gate's `screens` step captures the `single-pane` reference
scene in both of Dreamer's schemes (`cargo xtask screenshot` does the same
by hand) and checks the pixels:
- the main color is the scheme's background, so the theme reached the
  terminals and the chrome alike;
- the scheme's green, which the scene prints, is drawn;
- the image is not flat.

It captures the `app-components` scene in both schemes too: the
reference app with every text, input, and status component
(`examples/apps/components/`), drawn natively as an app pane on the left
and, started from a shell, as its terminal interface on the right. The
main color is the scheme's background (or the app pane's surface), the
accent is drawn, and a badge is drawn both ways: natively on its danger
surface, and in the terminal in the scheme's red. A window that drew
neither (it stopped repainting) records the step as not run.

The captures stay in `target/gate/screens/`. The step is not run on a
locked screen, or when the terminal running the gate lacks Screen
Recording permission (System Settings > Privacy & Security > Screen &
System Audio Recording).

### Preview check

The macOS gate's `preview` step opens the desktop against an isolated
tmux server, private daemon directories, and a temporary configuration,
in each of Dreamer's two schemes, and in each:

- writes the `table` template app (`dream app new`) and opens its live
  preview (`dream app preview --open`);
- writes `dream app preview --png`, and checks the image: at least half
  of it is the scheme's background or the app pane's surface, and more
  than a few colors are drawn on it.

In the dark scheme it then saves a change to the app's `app.ts` (the
table's title) and times it: until the host's tree has the change
(`apps.tree`), then until the desktop has drawn it, taken as how much
longer a capture that waits for the new version takes than one of what
was already drawn. The two together must stay under one second, and the
image after the save must differ from the one before. The images stay
in `target/gate/screens/`. A locked screen, a window that stops
repainting, or a terminal without Screen Recording permission records
the step as not run.
