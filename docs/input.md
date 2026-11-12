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

When the application does not track the mouse, the daemon sends nothing.
Local selection comes with M2-05b (#124).

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
