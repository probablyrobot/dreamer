# Input

Clients never encode terminal input themselves. They send semantic events
with `terminal.input`, and the daemon encodes each one on the pane's
mirror worker against the pane's current state (spec 10.1). That state
includes the cursor-key and keypad modes, the Kitty keyboard flags,
modifyOtherKeys, focus reporting, mouse tracking, and bracketed paste, so the bytes match
what the application asked for at that moment.

| Event | Fields | Encoded as |
| --- | --- | --- |
| `key` | `key` (W3C `KeyboardEvent.code`: `KeyA`, `Digit1`, `ArrowUp`, `Enter`, `F5`, …), `text`, `mods` (1 shift, 2 ctrl, 4 alt, 8 super), `held` | Ghostty's key encoder with the terminal's modes. For example `ArrowUp` is `ESC [ A`, or `ESC O A` in application cursor mode |
| `text` | `text` | The committed text, as typed (keyboard or IME commit; never IME preedit) |
| `paste` | `data`, `more`, `id` | Bracketed exactly once when the application enabled bracketed paste (mode 2004). Control sequences inside the pasted text are neutralized, so the text cannot close the bracket early |
| `focus` | `gained` | `ESC [ I` or `ESC [ O`, only if the application enabled focus reports (mode 1004); otherwise nothing |
| `mouse` | `action` (press, release, motion), `button` (none, left, middle, right, wheel_up, wheel_down), `col`, `row`, `mods`, `buttons_held` | Ghostty's mouse encoder with the application's tracking mode (X10 9, normal 1000, button-event 1002, any-event 1003) and format (X10, UTF-8 1005, SGR 1006, urxvt 1015, SGR-pixels 1016). Nothing is sent when the application does not track that kind of event |

Mouse positions are cells of the pane's canonical grid, 0-based and
clamped to it. The daemon gives the encoder the centre of that cell in
canonical pixels (10×20 per cell), so SGR-pixels reports do not depend on
the font or window of whichever client sent the event. A wheel step is one
press of `wheel_up` or `wheel_down`.

## In the desktop app

Every key press in the window goes through one policy:

1. **Paste:** Cmd-V on macOS, Ctrl-Shift-V on Linux. The clipboard text is
   sent as a `paste`.
2. **App shortcuts:** other Cmd shortcuts on macOS stay with the
   application (Cmd-Q quits).
3. **Key events:** special keys (arrows, Enter, Tab, Escape, F-keys, Home,
   End, Page Up and Down, Insert, Delete) and anything with Ctrl or Alt.
4. **Text:** everything else goes to the platform's text input. Plain
   typing and IME composition both arrive there:
   - composition (preedit) is drawn, underlined, at the terminal cursor
     and never sent;
   - committed text is sent as `text`;
   - the IME candidate window opens at the terminal cursor.

Input leaves the window through the pane's feed. A separate task, with its
own daemon connection, sends each event with the pane generation the feed
last saw, so input never waits for the follow loop. If the daemon is
unreachable, the input is dropped and counted, never queued for replay.

`dreamer --smoke-input --expect TEXT --attach %PANE` checks this end to
end, and the macOS gate runs it as the `input` step:
- it types `echo hi` through GPUI's own keystroke dispatch;
- it plays an IME composition ("日" composed, then "日本" committed) and
  presses Enter;
- it requires the pane to print exactly `hi日本`. Sending the preedit as
  well would print `hi日日本`.

The composition is fed to the app's input handler directly. Composing with
a real input method on each platform is checked by hand.

Mouse events over the grid become cells using the painted cell size:
- a press inside the grid, and its release wherever it happens;
- motion only when the pointer enters a new cell. While a button is held,
  motion keeps reporting outside the grid, clamped to its edge;
- wheel steps, one per line. Trackpad pixel deltas are added up and sent
  as whole rows (at most 32 steps per event); the remainder carries over.

Left-button presses go to the application only when it tracks the mouse
and Shift is not held. Otherwise they make a selection (below).

## Selection and copy

Selection is local view state on the window's replica, made with
Ghostty's selection gestures. It never changes the terminal, and selecting
needs no daemon round trip (spec 9.3, 11.5).

| Gesture | Selects |
| --- | --- |
| Drag | The cells between press and pointer. Which half of a cell the pointer is on decides whether that cell is included |
| Double-click | A word |
| Triple-click | A line |
| Drag held above or below the grid | Scrolls history one row every 50 ms and keeps extending the selection |
| Shift with any of these | Selects even when the application tracks the mouse |

- **Copy:** Cmd-C on macOS, Ctrl-Shift-C on Linux (Ctrl-C still goes to
  the program). The text comes from the replica on the window's own feed
  thread. Wrapped lines are joined and trailing blanks are trimmed. It goes
  to the system clipboard and, as in tmux, becomes tmux's newest paste
  buffer.
- **Paste:** Cmd-V on macOS, Ctrl-Shift-V on Linux: the system clipboard,
  as a bracketed paste when the program asked for one.
- **Marking text, as in tmux:** a finished mouse selection becomes tmux's
  newest paste buffer, so tmux's own paste (and any other tmux client)
  has it. On Linux it is also the primary selection. With
  `clipboard.copy_on_select: true` it goes to the system clipboard too.
- **Middle click:** pastes tmux's newest buffer, as tmux does
  (`clipboard.middle_click: off` turns it off). Like the left button, it
  goes to the program instead when the program tracks the mouse, unless
  Shift is held.
- **Painting:** the highlight is drawn over cell backgrounds and under the
  text. Frames carry the selected columns per row, apart from the shared
  rows.
- **Lifetime:** Ghostty tracks the selection's ends. It survives new output
  while its rows exist and goes when they are evicted or the screen is
  reset. A single click without a drag clears it.

The input smoke ends with a selection. It clears the screen, prints
`SELECT-ME`, drags across it through GPUI's event dispatch, and requires
both the highlighted columns and the replica's selected text to match
exactly. It does not use the system clipboard.

## tmux's prefix and copy mode

Dreamer sends keys to panes as bytes, so tmux's key tables never see
them. In a workspace window on the local daemon, Dreamer watches for the
tmux server's own prefix and handles the key after it:

| After the prefix | Does |
| --- | --- |
| `[` | Enters Dreamer's copy mode on the focused pane |
| `]` | Pastes tmux's newest paste buffer, as a paste (bracketed when the program asked for that) |
| `=` | Opens a list of tmux's buffers, newest first. Up and Down (or `j`, `k`) choose, Enter pastes, Escape or `q` closes |
| Any other key | Goes to tmux, which runs its binding for it: tmux's defaults (prefix `c` makes a window) and the user's own |

- **Which prefix:** `prefix` and `prefix2`, read from tmux with
  `tmux.key_options` (with `mode-keys` and `mode-style`). They are read
  when the window opens, whenever it becomes active, and after each
  prefix key; Dreamer never changes them. A small `prefix` label shows
  while the next key is awaited.
- **How other keys reach tmux:** `tmux.prefix_key` types the prefix and
  the key with `send-keys -K` on a short-lived control client of the
  daemon's, attached to the pane with `ignore-size,no-output`. The
  binding runs on that pane's session, as it would for a person at a
  terminal attached there. tmux sees that client attach and detach, so
  `client-attached` and `client-detached` hooks run. A binding that acts
  on its client (detach, a prompt, a menu) affects only that client.
- **Modes tmux would draw:** if a binding leaves the pane in a mode
  (`choose-tree`, a `view-mode` listing), the mode is cancelled again and
  a notice names it, because tmux draws modes only for ordinary clients.
  A binding that opens tmux's copy mode (prefix PageUp) opens Dreamer's
  instead.

### Copy mode

Copy mode works on the pane's local history and screen, as the replica
holds them; it never asks the daemon. It uses tmux's key table for the
server's `mode-keys`:

| Action | vi | emacs |
| --- | --- | --- |
| Cell, line | `h` `j` `k` `l`, arrows | `C-b` `C-n` `C-p` `C-f`, arrows |
| Word start, end, back | `w` `e` `b` (`W` `E` `B` by blanks only) | `M-f` (past the word), `M-b` |
| Line start, first non-blank, end | `0` `^` `$`, Home, End | `C-a` `M-m` `C-e`, Home, End |
| History top, bottom | `g` `G` | `M-<` `M->` |
| Screen top, middle, bottom | `H` `M` `L` | |
| Half page, page | `C-u` `C-d`, `C-b` `C-f`, PageUp, PageDown | `M-v` `C-v`, PageUp, PageDown |
| Scroll a line | `C-y` `C-e`, `C-Up` `C-Down` | `C-Up` `C-Down` |
| Search down, up | `/`, `?` | `C-s`, `C-r` |
| Repeat search, reversed | `n`, `N` | `n`, `N` |
| Begin selection | `v` or Space | `C-Space` |
| Rectangle on or off | `C-v` | `R` |
| Swap selection ends | `o` | |
| Copy and leave | `y` or Enter | `M-w` or `C-w` |
| Clear selection | Escape | `C-g` |
| Leave | `q`, Escape with no selection, `C-c` | `q`, Escape, `C-c` |

- **Copying:** the text becomes tmux's newest paste buffer, and goes to
  the system clipboard when `clipboard.tmux` brings tmux's copies there
  (and `clipboard.write` is not `deny`). Trailing blanks are trimmed from
  each row. Copying with no selection just leaves.
- **Drawing:** Dreamer draws copy mode over the pane: the selection in
  `mode-style`'s background (translucent over the text; the theme's
  accent if the style has no background), the cursor as a box, and
  tmux's position indicator `[rows back/history rows]` at the top right.
  The search prompt, `(search down)` or `(search up)`, and messages such
  as `Search failed` show at the bottom.
- **Search:** plain text, typed at the prompt and run on Enter; an empty
  prompt repeats the last search. Case is ignored unless the text has a
  capital letter. It wraps past either end of history.

What differs from tmux's own copy mode:
- The pane keeps updating underneath. Copy mode's text is read again
  when history grows, and positions keep their rows from the top of
  history; once history is full and its oldest rows are dropped, the
  cursor and selection can drift by those rows.
- Search is plain text, not a regular expression, and is not
  incremental. Matches other than the one jumped to are not highlighted.
- Words follow vi's classes (letters, digits, and `_`; other symbols;
  blanks), not tmux's `word-separators` option.
- Soft-wrapped lines are rows of their own: a selection across a wrap
  copies a newline there.
- `mode-style`'s foreground and attributes are not applied, and copy-mode
  bindings changed with `bind-key -T copy-mode` or `copy-mode-vi` are not
  followed: only tmux's default tables are.
- After the prefix, one key is handled. tmux's repeat time (bindings made
  with `bind-key -r`) does not keep the prefix table active for more.
- A single-pane window (`dreamer --attach`) and hosts reached over SSH do
  not have the prefix layer.

## Scrolling

Scrolling is local view state on the window's replica. It is never sent to
the daemon and never changes the terminal (spec 10.1, 11.5). Each frame
carries the viewport position, whether the application tracks the mouse,
and whether the alternate screen is active, so the window decides locally.

| Input | Application tracks the mouse | Alternate screen, no tracking | Otherwise |
| --- | --- | --- | --- |
| Wheel or trackpad | Sent as wheel buttons | One Up or Down arrow key per step | Scrolls history, one row per step |
| Shift-PageUp, Shift-PageDown | Scroll history by a page | Sent as keys | Scroll history by a page |
| Shift-Home, Shift-End | Scroll to the top or bottom | Sent as keys | Scroll to the top or bottom |

- **New output while scrolled back:** the view keeps showing the same rows.
  A note at the top right says how many lines below the view there are.
- **Typing returns to the bottom:** sending a key, text, or a paste
  scrolls a scrolled-back view down first.
- **History depth:** the subscription's scrollback depth (10,000 lines by
  default). A restored checkpoint starts at the bottom again.

The input smoke checks the wheel path too. After the mouse probe it prints
200 lines, scrolls up five lines with the wheel through GPUI's event
dispatch, and requires a frame five rows back from the bottom.

The input smoke continues after `hi日本`: it runs a probe that turns on SGR
mouse reporting, clicks and releases at cell (3, 1), sends one wheel step
down, and requires the pane to read exactly `ESC[<0;4;2M ESC[<0;4;2m
ESC[<65;4;2M`. The events go through GPUI's own event dispatch to the
window's mouse listeners.

## Hyperlinks

OSC 8 links are hit-tested on the replica. A link is never opened because
of terminal output alone.

- **Hover:** with the open modifier held (Cmd on macOS, Ctrl on Linux),
  the link under the pointer is underlined across all its cells,
  including where it wraps onto the next row. The pointer becomes a hand.
- **Open:** a modifier-click opens the link. A plain click never does:
  it selects, or goes to a program that tracks the mouse.
- **Schemes:** only allowed schemes open: `http`, `https`, and `mailto`
  unless `links.schemes` says otherwise (see `docs/settings.md`). Any
  other scheme is refused, and a notice at the bottom of the grid shows
  the refused link for four seconds.

The input smoke checks this after the selection step:
- it prints an OSC 8 link;
- it clicks the link plainly and requires nothing to open;
- it hovers with the open modifier and requires the underline to cover
  exactly the link's cells;
- it modifier-clicks and requires exactly that URI to open.

In smoke mode, an open is recorded instead of launching a browser.

## Delivery

Encoded bytes reach the pane through tmux's literal-byte path
(`send-keys -H`):
- in batches of 64 commands of 256 bytes (16 KiB per batch);
- each batch acknowledged before the next is sent.

Large input is never one unbounded command line and is never reordered.
The daemon delivers one input event at a time, so a paste cannot
interleave with other input.

A paste larger than one request (the protocol's frames are at most 1 MiB)
is sent in parts with the same `id`, with `more` set on all but the last.
The daemon reassembles it, at most 64 MiB, and encodes it once.
`client::paste` does the splitting.

With `generation` set, input is refused (`-32002`) if the pane's process
was replaced since the client read that generation. Typed text can never
reach a respawned program by accident.

## Automation and interaction leases

Automation that types into a pane (an agent, a script acting for one)
holds the pane's interaction lease and sends every input with it:
- `leases.acquire {pane, ttl_ms}` returns a lease (`id`, `generation`,
  expiry). One connection holds a pane's lease at a time. It expires after
  `ttl_ms` without use (default 30 s, at most 10 min), and each use starts
  that time again. It ends with its connection, or with `leases.release`.
- `terminal.input` and the `send_input` and `send_keys` actions take the
  lease in `lease`. It is checked in the pane's input lane, right before
  sending, so input queued behind a person's keystroke is refused, not sent
  after it.
- Typed input without a lease from the user's own clients is a person
  typing: it revokes the pane's lease. The lease's later input fails
  (`-32002`, "a person typed into the pane"), and a new lease has a new
  generation. Focus and mouse events do not count as typing.
- While a pane is leased, input without the lease from an app, extension,
  or narrowed connection is refused.

Bytes already sent cannot be taken back, and nothing is replayed. A
connection that may send input to a pane (`terminal.input` or
`actions.submit` in its grant) may lease it.
