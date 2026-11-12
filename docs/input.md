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

Mouse input is part of the desktop input work (#119). Its encoding needs
screen and cell geometry that only the GUI has.

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
