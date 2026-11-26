# Wire protocol

Dreamer clients (the desktop app, `dream`, extensions) talk to `dreamerd`
using the binary protocol in `crates/wire`, over its Unix socket or over
SSH through `dream bridge --stdio` ([cli.md](cli.md#bridge)). You need
this page if you are writing a native client in another language. For
scripts, the JSON-RPC endpoint is simpler
([api.md](api.md#json-rpc-endpoint)). The protocol does not depend on
tmux's protocol or on Ghostty's C ABI.

## Byte layout

Each side opens with an 8-byte **preamble**:

| Bytes | Content |
| --- | --- |
| 0–3 | Magic `DRMR` |
| 4–5 | Protocol major version, big-endian |
| 6–7 | Protocol minor version, big-endian |

Everything after the preamble is **frames**:

| Bytes | Content |
| --- | --- |
| 0–3 | Payload length, big-endian `u32` (the header is not counted) |
| 4 | Kind |
| 5 | Flags (bit 0: more chunks follow; valid only on bulk frames) |
| 6… | Payload: a Protobuf record |

| Kind | Record | Direction |
| --- | --- | --- |
| 1 Hello | `Hello` | client → server, once |
| 2 Welcome | `Welcome` | server → client, once |
| 3 Reject | `Reject` | server → client, then close |
| 4 Request | `Request` (method name and API payload) | client → server |
| 5 Response | `Response` (payload or error) | server → client |
| 6 Event | `Event` (topic, sequence, payload) | server → client |
| 7 Bulk | `Bulk`, split across chunks | either |
| 8 Ping, 9 Pong | empty | either |
| 10 Goodbye | `Goodbye` | either |

`Request.payload` and `Response.payload` hold the method's parameter and
result records from the API schema ([api.md](api.md)), encoded as Protobuf.

## Handshake

1. Both sides send their preamble.
2. If the major versions differ, the server sends `Reject`
   (`incompatible_protocol`) and closes. It has not decoded anything else
   the client sent. A client that sees another major in the server's
   preamble stops in the same way.
3. The client sends `Hello` with its engine **build family**. The server
   compares it with its own family and refuses any difference with `Reject`
   (`incompatible_build`).
   - The family is derived from the pinned Ghostty release and commit, the
     required engine features, and Dreamer's terminal policy revision
     (`terminal_core::family`).
   - It does not depend on architecture, so a macOS build and a Linux build
     of the same revision are one family.
4. The server answers `Welcome` with the negotiated limits (the smaller of
   each side's values) and the daemon instance, epoch, host, process ID, and
   workspace. Only after `Welcome` are requests decoded.

`Reject` carries the server's version, build family, and process ID, so a
client can tell the user exactly which daemon to restart.

## Limits

| Limit | Default | Enforced |
| --- | --- | --- |
| `max_frame` | 1 MiB | A frame header that claims more is a protocol error, raised before any memory is reserved. A reply that would not fit is sent as an error naming its size instead (ask for less, for example fewer history lines), and the connection stays up |
| `max_message` | 64 MiB | Reassembled bulk data. Checked before each chunk is appended |
| `bulk_chunk` | 64 KiB | Size of the pieces bulk messages are cut into |
| `max_priority_queued` | 8 MiB | Outgoing priority bytes; beyond it a send fails with `Full` |
| `max_bulk_queued` | 128 MiB | Outgoing bulk bytes; beyond it a send fails with `Full` |

The decoder only ever holds one partial frame plus one socket read. After a
protocol error it releases its buffers and accepts nothing more on that
connection.

## Queues

Outgoing frames wait in two queues. Priority frames (input, requests,
responses, events) always go first. Bulk messages (checkpoints, screen
frames, history, artifacts) are cut into chunks, so priority traffic waits behind at most one
chunk. The daemon takes forward data (`forward.data`, see
[remote.md](remote.md#forwards)) into the bulk queue only when both queues
are empty, so it never waits ahead of anything else. A single socket
still has transport-level head-of-line blocking; the queues only decide
the order in which Dreamer writes.

## Evolution

- Record field numbers are permanent. A test pins the encoded bytes of
  `Hello`, a frame header, and the preamble.
- New optional behavior is announced in `Hello.features` and
  `Welcome.features`; a side uses a feature only when both list it.
  `Welcome.features` lists the features both sides listed.
- A new frame kind needs a feature or a new major version. Receivers treat
  unknown kinds as protocol errors.

## Features

| Feature | What it allows |
| --- | --- |
| `terminal.frames` | Terminal subscriptions on the `frames` feed: the daemon sends screen frames (bulk, topic `terminal.frame`) instead of records. A client that does not list it is refused the `frames` feed and never receives a frame. See [mirrors.md](mirrors.md#screen-frames) |

A coordinator lists `terminal.frames` when it connects to a host, so
frames for a relayed pane pass through it.
