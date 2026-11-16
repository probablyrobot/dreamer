# Terminal compatibility

What works end to end through the full path: application → tmux 3.8 →
`dreamerd` mirror (libghostty-vt 1.0.1) → Dreamer renderer. A protocol is
listed as supported only when its request and response paths both pass
through tmux in a test. Ghostty parsing a sequence is not enough.

Entries cite the test that proves them. A feature with no passing end-to-end
test is not listed as supported.

## Output: what a program writes

**The path.** A program in a real tmux pane writes the sequence. tmux
carries it in control mode to the daemon's mirror. A client replica
restores the mirror's checkpoint and follows its stream, and the replica's
frame must show the feature (`crates/server/tests/protocols.rs`, on the
live path). A pane that wrote before the daemon followed it starts from
tmux's own capture instead; what survives that is in
[`tmux-bootstrap.md`](tmux-bootstrap.md).

**The Drawn column** is what the desktop's renderer paints. It is checked
by the `matrix` gate stage's fixtures (`crates/terminal-render/fixtures`).

| Feature | Sequence | Mirror and replica | Drawn | Evidence |
| --- | --- | --- | --- | --- |
| 16, 256, and 24-bit colors, foreground and background | SGR 30–37, 38;5;N, 38;2;R;G;B, 48;… | Yes | Yes | `colors_and_text_attributes_reach_the_replica` |
| Bold, italic, faint, inverse, strikethrough | SGR 1, 3, 2, 7, 9 | Yes | Yes | Same test; `decorations.vt` |
| Overline | SGR 53 | Yes | No: kept in the terminal state, not painted | Same test |
| Underline: single, curly | SGR 4, 4:3 | Yes | Yes (curly as a wave) | Same test; `decorations.vt` |
| Underline: double, dotted, dashed | SGR 4:2, 4:4, 4:5 | Yes | Painted as a single underline | Same test; `decorations.vt` |
| Underline color | SGR 58;2;R;G;B | Yes | Yes | Same test |
| Wide CJK characters and emoji | UTF-8 | Yes: one wide cell and its spacer | Yes | `wide_characters_emoji_and_combining_marks_keep_their_cells`; `text-matrix.vt` |
| Combining marks | UTF-8 | Yes: joined to their base cell | Yes | Same tests |
| Flags and other multi-codepoint emoji | UTF-8, with mode 2027 set by the program | Yes: one wide cell | Not pixel-tested | Same test. Without mode 2027 each regional indicator is its own cell |
| Cursor shapes and blink | DECSCUSR `CSI N SP q` | Yes | Yes (block, bar, underline, hidden) | `cursor_shapes_the_alternate_screen_and_hyperlinks_reach_the_replica`; `cursor-*.vt` |
| Hyperlinks | OSC 8 | Yes: hit-tested over their cells | Not pixel-tested | Same test; `osc8_links_are_hit_tested_over_their_cells_and_wraps` |
| Alternate screen | `CSI ? 1049 h` | Yes: no history while it is active | Yes | Same test |
| Window title, working directory | OSC 2, OSC 7 | Yes: pane state in every view (OSC 7 as the URI written) | The window title | `titles_and_working_directories_reach_every_view` |
| Synchronized output | `CSI ? 2026 h`/`l` | Held until the end, or shown after the limit | Yes | `a_synchronized_output_hold_is_shown_after_the_limit` |
| Bell | BEL | A live effect to current viewers, never replayed | A brief flash | `a_bell_reaches_each_live_subscriber_once_and_is_never_replayed` |
| Clipboard write | OSC 52 | To the pane's focused client only | Applied under `clipboard.write` | See [Reply ownership](#reply-ownership) |

**Not supported end to end:**
- **Kitty graphics** work in the engine and the renderer: placements,
  crops, z-order, and deletes, with file and shared-memory transmissions
  refused (`crates/terminal-ghostty/tests/images.rs`, the `image-*.vt`
  fixtures). They are not claimed through tmux, which needs
  `allow-passthrough` and has no test here.
- **Sixel graphics, the kitty keyboard protocol, and OSC 4 palette
  queries** have no tested path through tmux 3.8.

## Input: what the program receives

The daemon encodes input against the pane's current modes, and tmux
delivers it (`crates/server/tests/input.rs`).

| Feature | Behavior | Evidence |
| --- | --- | --- |
| Keys in normal and application cursor mode | `CSI A` and `SS3 A` as the mode says | `keys_are_encoded_against_the_pane_s_current_modes` |
| Bracketed paste | Markers exactly once, only when the program enabled them | `bracketed_paste_markers_appear_exactly_once` |
| Large pastes | 1 MiB arrives byte for byte | `a_one_mib_paste_arrives_byte_for_byte` |
| Mouse | SGR-encoded, and only to programs that track the mouse | `mouse_reaches_only_programs_that_track_it` |
| Input for a replaced process | Refused, never typed into the new process | `input_for_a_replaced_process_is_refused` |
| Automation and a person typing | A person typing takes over from leased automation | `a_person_typing_takes_over_from_leased_automation` |

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
