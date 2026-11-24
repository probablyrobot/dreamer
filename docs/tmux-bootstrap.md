# tmux cold attach (bootstrap)

A cold attach is how `dreamerd` builds a terminal mirror for a pane it has
not been watching, and continues it from live output without losing or
repeating bytes. You need this page if you work on the tmux backend, or
want to know what a freshly attached mirror can and cannot restore.
Baseline: tmux 3.8 (`control.c`, `cmd-refresh-client.c`,
`cmd-capture-pane.c`, `window.c` at tag `3.8`). Implementation:
`crates/tmux-backend/src/bootstrap.rs`. Tests:
`crates/tmux-backend/tests/bootstrap.rs`.

This page is about the daemon attaching to tmux. A desktop reconnecting to a
daemon that kept its mirror uses checkpoints instead and does not recapture
tmux.

## Why a capture alone is not enough

A control client receives pane output as `%output` lines, and a command's
reply between `%begin` and `%end`. In tmux 3.8 both go through one ordered
block list per client, but each pane's output is drained separately for
fairness (`control_write_pending`). A busy pane's output produced *after* a
`capture-pane` can therefore reach the client *before* that capture's
`%begin`, whenever another pane's earlier output is still queued. Notifications
raised by a command are also deferred until after its `%end`
(`control_notify_write`). So neither the `%begin`/`%end` guard nor a
notification inside one command line is a reliable boundary.

## The procedure

Both steps run on the pane's canonical control client, which must not use
`pause-after`.

**Phase 1: pause, and wait for it.**

```text
display-message -p -t '%N' '#{pane_id}' ; refresh-client -A '%N:pause'
```

- `refresh-client -A` silently ignores an unknown pane, and `display-message`
  prints an empty line rather than failing, so the first reply must equal
  `%N`. Otherwise the result is `PaneNotFound`.
- `control_pause_pane` discards the pane's unwritten output blocks and queues
  none while paused. Once `%pause %N` has been *read*, every earlier output
  for the pane is either already in the stream or discarded, and none is
  pending.
- `%output %N` received before `%pause %N` is dropped. Its effect is already
  in tmux's grid, which phase 2 captures.

**Phase 2: capture and continue, as one command line.**

```text
display-message -p -t '%N' '<state format>' ;
capture-pane -p -e -J -N -S - -E -1 -t '%N' ;   # history
capture-pane -p -e -J -N -S 0 -E - -t '%N' ;    # visible (alternate if active)
capture-pane -p -e -J -N -a -q -t '%N' ;        # saved normal screen
capture-pane -p -P -C -t '%N' ;                 # pending incomplete escape
refresh-client -A '%N:continue'
```

- A command line runs to completion in one pass of the command queue. None of
  these commands yield, so tmux reads no pane output between the captures and
  `continue`.
- `control_continue_pane` resets the client's offsets to the pane parser's
  offset. That is the exact position the captures reflect.
- Phase 2 is sent only after `%pause %N` has arrived. So every `%output %N`
  after `%pause %N` was produced after `continue` and is the exact
  continuation of the captured state, even when it arrives before the capture
  replies.
- If any command fails, the rest of the line did not run. The pane is resumed
  with a separate `refresh-client -A '%N:continue'` and bootstrap returns
  `CaptureFailed`.

Each command in a `;` line gets its own `%begin`/`%end`. Pane targets must be
quoted (`'%N:pause'`).

**Restore.** A fresh engine of the pane's size, scrollback set to
`history-limit` lines, and reply capture off. It is fed synthesized VT bytes,
its effects are discarded, then the continuation is fed:

1. Tab stops (`CSI 3 g`, then `CSI n G ESC H` per stop). These come first
   because captured lines can contain literal tabs, which tmux keeps in its
   grid.
2. History lines, then visible lines, joined with `CR LF`, each followed by
   `SGR 0`. `-J` joins soft-wrapped lines, so the engine re-wraps them at the
   same width.
3. Alternate screen active: history and the saved normal screen, a cursor
   move to `alternate_saved_x/y`, `CSI ? 1049 h`, home, then the alternate
   lines.
4. Scroll region (`DECSTBM`), modes, and cursor shape (`DECSCUSR`).
5. Cursor position, relative to the region when origin mode is set.
6. Pending bytes from `capture-pane -P -C`, octal-unescaped.

## State restored

| Field (tmux format) | Restored with |
| --- | --- |
| History and visible text with SGR (`capture-pane -e -J -N`) | Replayed lines |
| `alternate_on`, saved normal screen, `alternate_saved_x/y` | `CSI ? 1049 h` sequence |
| `cursor_x`, `cursor_y` | `CUP` |
| `cursor_flag` | `CSI ? 25 l` |
| `scroll_region_upper/lower` | `DECSTBM` |
| `origin_flag` | `CSI ? 6 h` (with region-relative `CUP`) |
| `insert_flag` | `CSI 4 h` |
| `keypad_cursor_flag` | `CSI ? 1 h` |
| `keypad_flag` | `ESC =` |
| `wrap_flag` | `CSI ? 7 l` when off |
| `mouse_standard/button/any_flag`, `mouse_sgr_flag` | `CSI ? 1000/1002/1003/1006 h` |
| `bracket_paste_flag` | `CSI ? 2004 h` |
| `cursor_shape`, `cursor_blinking` | `DECSCUSR` |
| `pane_tabs` | `CSI 3 g` and `ESC H` per stop |
| Pending incomplete escape or UTF-8 (`capture-pane -P`) | Raw bytes, completed by the continuation |
| `history_limit` | Mirror scrollback line limit |

`pane_output_generation` is recorded with each bootstrap as evidence. It is
not a byte cursor and is not needed for the boundary.

## State that cannot be reconstructed

tmux does not expose these, so a cold-attached mirror starts from defaults:

- The current SGR pen (attributes set and not yet used). Text written after
  attach without its own SGR may render with default attributes until the
  application sets them again.
- `DECSC` saved cursor and its attributes (outside the alternate-screen save).
- Character sets (`G0`–`G3` designations, line-drawing mode).
- Title stack, OSC 4 palette changes, OSC 10/11/12 colors, and the window
  title in the engine. The title is tracked as metadata separately.
- Kitty keyboard flags, `modifyOtherKeys`, focus reporting (1004), and other
  modes absent from tmux's format list.
- Hyperlinks (OSC 8) on captured text, and the semantic prompt marks
  (OSC 133).
- Images. Captured history holds text only.
- Whether a captured history line was soft-wrapped across the boundary
  between history and the visible screen (restored as a hard break; the
  layout is identical).

Each of these is wrong only until the application rewrites it. Live output
after the attach is exact.

## Known divergence: scroll regions with a top margin

tmux 3.8 appends lines scrolled out of a scroll region whose top margin is
not the first row to its history. Ghostty, like xterm, does not. This affects
live mirroring as well as bootstrap: the visible screen and cursor match,
history differs. See [terminal-compatibility.md](terminal-compatibility.md).

## Results

macOS 27.2 arm64, tmux 3.8, `cargo test -p dreamer-tmux-backend` (8 integration
tests plus unit tests), all passing:

| Case | What was checked | Result |
| --- | --- | --- |
| Idle pane with SGR, CJK, multiple lines | Full history, visible rows, cursor, bold blue style | Exact |
| Busy stream (15,000 numbered lines), 5 rounds at different moments | Bootstrap mid-stream (about 7,900 lines in history at capture, about 70 KB dropped before pause), then follow to the end; full history | Exact; no lost or repeated line |
| Busy pane beside another busy pane (12,000 and 40,000 lines), 3 rounds | Same, with interleaved output from two panes | Exact |
| Alternate-screen application | State while in the alternate screen, then the normal screen after `CSI ? 1049 l` | Exact both times |
| Incomplete escape (`ESC [3` pending) and split UTF-8 | Pending bytes captured; continuation completes red SGR and the CJK character | Exact |
| Modes, scroll region, custom tab stops, cursor shape | Engine modes match tmux flags; output after attach lands identically | Visible rows and cursor exact; history differs only by the divergence above |
| Resize just before bootstrap | Mirror takes the new size (33x7); full history | Exact |
| Unknown pane | Fails within 2 s as `PaneNotFound`; the next bootstrap on the same client works | Pass |

Linux x86-64 results come from CI, which builds the pinned tmux 3.8 with
`cargo xtask deps tmux`.

Not yet tested:

- Resize *during* bootstrap.
- Panes in copy mode or with a tmux-generated mode active.
- Capture hooks or other yielding commands configured on the server.
- Pane respawn during bootstrap.

## Quiescence, retries, and failure

- No quiescence is required. The boundary is exact while output continues,
  which the busy-stream cases exercise.
- There are no retries for consistency. A bootstrap either produces an exact
  mirror or fails with `PaneNotFound`, `CaptureFailed` (pane resumed),
  a protocol error, or a timeout (deadline supplied by the caller).
- On failure the pane is marked unable to resynchronize, and the user can
  retry or open the normal tmux attach fallback. Nothing restarts the user's
  process or waits for output to stop.
- One limitation remains. If the pane is already paused for this client
  (`pause-after`), no `%pause` is emitted and phase 1 times out. The canonical
  client must not enable `pause-after`.
