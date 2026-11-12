# Input

Clients never encode terminal input themselves. They send semantic events
with `terminal.input`, and the daemon encodes each one on the pane's
mirror worker against the pane's current state (spec 10.1). That state
includes the cursor-key and keypad modes, the Kitty keyboard flags,
modifyOtherKeys, focus reporting, and bracketed paste, so the bytes match
what the application asked for at that moment.

| Event | Fields | Encoded as |
| --- | --- | --- |
| `key` | `key` (W3C `KeyboardEvent.code`: `KeyA`, `Digit1`, `ArrowUp`, `Enter`, `F5`, …), `text`, `mods` (1 shift, 2 ctrl, 4 alt, 8 super), `held` | Ghostty's key encoder with the terminal's modes. For example `ArrowUp` is `ESC [ A`, or `ESC O A` in application cursor mode |
| `text` | `text` | The committed text, as typed (keyboard or IME commit; never IME preedit) |
| `paste` | `data`, `more`, `id` | Bracketed exactly once when the application enabled bracketed paste (mode 2004). Control sequences inside the pasted text are neutralized, so the text cannot close the bracket early |
| `focus` | `gained` | `ESC [ I` or `ESC [ O`, only if the application enabled focus reports (mode 1004); otherwise nothing |

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

Mouse input is still to come (#119). Its encoding needs the screen and
cell geometry the window has.

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
