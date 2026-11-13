# Effects and replies

A program in a pane does more than draw. It answers queries, sets a
title, reports its working directory, rings the bell, and may write the
clipboard. Each of these has one owner (spec 10.3, 11.7). Nothing is
repeated because a client reconnected or a checkpoint was restored.

| What | Owner and rule |
| --- | --- |
| Replies to terminal queries (DA, DSR, cursor position, modes) | tmux answers them. Engines on the daemon and in windows never send a reply. `crates/tmux-backend/tests/replies.rs` checks that each query gets exactly one reply |
| OSC 10/11 color queries | tmux, from the colors the desktop reported (`refresh-client -r`) |
| Title (OSC 0/2) and working directory (OSC 7) | **State** kept by the daemon's mirror. See below |
| Bell | **Live effect**. See below |
| Clipboard write (OSC 52) | **Live effect to one client**. See below |
| Clipboard read (OSC 52 `?`) | Nobody answers: the program gets nothing back (spec 15.4) |

## Title and working directory are state

The daemon's mirror keeps the latest title and working directory each
program set since the mirror attached. They appear in:
- `terminal.mirrors` (`title`, `cwd`);
- every subscription's start (`Subscription.title`, `Subscription.cwd`),
  so a client that reconnects or starts from a checkpoint has the current
  labels at once, without old events.

A change while a client is subscribed also arrives as a live effect.

The window title follows the pane: its title, else its working directory,
else "Dreamer". The tmux pane title (`#{pane_title}`) is in the topology.
The mirror reports only what the program set after the mirror attached.

## Bells are live effects

Applying a live record can produce a `TerminalEffect` in the same
`RecordBatch`. It has an increasing `id`, the `sequence` of the record
that produced it, a bell count, and a new title or working directory.

Effects go only to subscribers that are live when they happen. They are
never journaled, so these produce none:
- replayed records (a reconnecting client);
- a checkpoint (a new or recovering client);
- bootstrap.

A client acts on effects only from records its replica applied, so a
duplicate batch rings nothing. Client replicas never act on what their
own engine sees.

In the window, a bell flashes the grid for 120 ms, at most once every
240 ms.

`a_bell_reaches_each_live_subscriber_once_and_is_never_replayed` checks
this with two live subscribers, a replay reconnect, and a checkpoint
start. The input smoke ends by having a program set a title and ring the
bell, and requires the window title and the flash.

## Clipboard writes go to one window

A program in a pane can write the clipboard with OSC 52. tmux 3.8 passes
the sequence to control mode, and the daemon's mirror captures the text.
Capture is turned on only after bootstrap, so restoring a pane never
produces a write.

- **One window.** Each window has a client ID. It sends that ID with its
  subscription and its input, and the window's focus events tell the
  pane's owner which client is focused. A write is sent only on the
  focused client's stream. With no focused client, nobody gets it. Two
  windows on one pane never both apply a write.
- **Never replayed.** Like every effect, a write is never journaled, so it
  never arrives again by replay, checkpoint, or reconnect.
- **Size limit.** A write over 1 MiB is refused whole, never truncated.
  Requests to clear the clipboard, and writes to other selections, are
  refused.
- **The window decides last.** It applies the write under
  `clipboard.write` in `dreamer.yaml`:

| Setting | When the write reaches the system clipboard |
| --- | --- |
| `focused` (default) | Only while the window is active |
| `allow` | Whenever the write is addressed to this window |
| `deny` | Never |

**Reads get nothing back.** Mirrors never reply, and tmux 3.8 with default
options does not answer either. This is the limited profile of spec 15.4.
Pasting stays an ordinary input operation.

**Evidence.**
- `a_clipboard_write_reaches_only_the_focused_client_once`: two clients;
  focus moves between them; a replay reconnect; no focused client.
- `a_clipboard_read_gets_no_clipboard_contents`.
- The input smoke's last step sends a focus event the way window
  activation does, then has a program write the clipboard. It requires
  the write to arrive once. The smoke records the write instead of
  touching the system clipboard.
