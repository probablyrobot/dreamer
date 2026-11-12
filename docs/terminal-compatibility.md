# Terminal compatibility

What works end to end through the full path: application → tmux 3.8 →
`dreamerd` mirror (libghostty-vt 1.0.1) → Dreamer renderer. A protocol is
listed as supported only when its request and response paths both pass
through tmux in a test. Ghostty parsing a sequence is not enough.

This page grows with each milestone. Entries cite the test that proves them.

## Reply ownership

Ghostty only answers a query when a reply callback is registered. Dreamer
mirrors register none by default (`EngineConfig::capture_replies = false`),
so the application inside tmux gets tmux's answer and nothing else. Mirror
engines never write to a pane.

| Query or effect | Owner | Rule | Evidence |
| --- | --- | --- | --- |
| DA1 `CSI c`, DA2 `CSI > c` | tmux | Mirror reply suppressed | `tests/replies.rs`: exactly one reply (`?1;2;4c`, `>84;0;0c`) |
| DSR 5 (status), DSR 6 (cursor position) | tmux | Mirror reply suppressed | Same test: exactly one reply (`0n`, `1;6R`) |
| OSC 10 / OSC 11 (foreground / background color) | tmux, using colors the GUI reports | GUI sends `refresh-client -r '%N:<OSC reply>'` with its theme colors; mirror reply suppressed | Same test: without a report tmux answers `rgb:0000/0000/0000`; after a report it answers once with the reported colors |
| OSC 4 palette queries | Not delegated | No report path in tmux 3.8 (`refresh-client -r` accepts only OSC 10/11) | Not tested |
| Kitty keyboard, Kitty graphics, clipboard replies | Not delegated | Unsupported until a tested single-responder path exists | Not tested |
| Replies during bootstrap or snapshot restore | Nobody | Always suppressed | `bootstrap.rs` restores with capture off |
| Bell, title, cwd | Local UI | Applied as current state on live output; never replayed from restore | `terminal-ghostty` effects tests |
| Clipboard write and read (OSC 52) | Daemon policy | Not yet supported | Not tested |

Why suppression matters: with reply capture on, a mirror engine attached to
the same stream answered the same queries differently (`?62;22c`,
`>1;0;0c`, and a cursor position from its own state). Forwarding those
would give the application two answers that disagree.

Consequence for the GUI: tmux answers OSC 10/11 with black until a client
reports colors. The desktop must report its theme's foreground and
background for every pane it shows (and again when the theme changes), or
applications that adapt to the background will guess wrong.

## Known differences between tmux and the mirror

| Behavior | tmux 3.8 | Ghostty mirror | Effect |
| --- | --- | --- | --- |
| Lines scrolled out of a scroll region whose top margin is not the first row | Kept in history | Discarded (as in xterm) | Visible screen identical; history differs. Found by `tests/bootstrap.rs` |
| Literal tab characters | Kept in the grid; `capture-pane` prints tabs | Cells skipped; text shows spaces | Same layout; plain-text comparisons must expand tabs |

## Cold attach

What survives a cold attach and what does not: see
[`tmux-bootstrap.md`](tmux-bootstrap.md).
