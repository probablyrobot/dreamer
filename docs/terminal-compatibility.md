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
| Bell, title, cwd | Daemon state and live effects | Title and cwd are state in `terminal.mirrors` and every subscription start; a bell is a live effect to subscribers present at the time, never replayed | `replicas.rs`: `a_bell_reaches_each_live_subscriber_once_and_is_never_replayed` |
| Clipboard write (OSC 52) | Daemon, addressed to one client | tmux 3.8 passes the sequence to control mode. The daemon's mirror captures it (capture on only after bootstrap) and sends it only to the pane's focused client; the window applies it under `clipboard.write`. Never replayed. Writes over 1 MiB are refused | `replicas.rs`: `a_clipboard_write_reaches_only_the_focused_client_once`; input smoke |
| Clipboard read (OSC 52 `?`) | Nobody | No answer: the program gets nothing back (tmux 3.8 with default options does not answer, and mirrors never reply) | `replicas.rs`: `a_clipboard_read_gets_no_clipboard_contents` |

Why suppression matters: with reply capture on, a mirror engine attached to
the same stream answered the same queries differently (`?62;22c`,
`>1;0;0c`, and a cursor position from its own state). Forwarding those
would give the application two answers that disagree.

Consequence for the GUI: tmux answers OSC 10/11 with black until a client
reports colors. The desktop must report its theme's foreground and
background for every pane it shows (and again when the theme changes), or
applications that adapt to the background will guess wrong.

## tmux-drawn screens

| What | In the workspace | Fallback |
| --- | --- | --- |
| Floating panes (tmux 3.8) | The window is marked unsupported; no partial layout is drawn | Copyable `tmux attach-session -t '$S:@W'`, and an optional terminal launch |
| Copy mode, choose tree, other `pane_in_mode` screens | The pane gets a banner naming the mode; its program's output stays shown underneath | The same attach command |

Evidence: the workspace smoke marks copy mode started with the plain
`tmux` command, and resolves the attach target to that exact window. The
quoting has unit tests in `crates/app/src/fallback.rs`.

## Known differences between tmux and the mirror

| Behavior | tmux 3.8 | Ghostty mirror | Effect |
| --- | --- | --- | --- |
| Lines scrolled out of a scroll region whose top margin is not the first row | Kept in history | Discarded (as in xterm) | Visible screen identical; history differs. Found by `tests/bootstrap.rs` |
| Literal tab characters | Kept in the grid; `capture-pane` prints tabs | Cells skipped; text shows spaces | Same layout; plain-text comparisons must expand tabs |

## Cold attach

What survives a cold attach and what does not: see
[`tmux-bootstrap.md`](tmux-bootstrap.md).
