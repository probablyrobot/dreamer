# Messages

Connections to `dreamerd` can send each other structured messages: a
pane app reporting progress, a monitor asking another app for its state.
Messages travel only between connections. **Receiving a message never
types anything into a shell**: input to a pane is always a separate,
authorized `terminal.input` or action.

## Who sends

Every connection has an identity as a sender and recipient:

- a registered pane app: its instance, `<app id>-<launch id>` (see
  [grants.md](grants.md));
- one of the user's own clients (the owner grant): `owner:<n>`, unique
  per connection while the daemon runs.

The daemon stamps that identity on every message it delivers, in
`sender`: the instance, the app id and pane for an app, and whether it was
sent under the owner grant. A client cannot choose it. Whatever a client
says about itself in the `sender` field of a request is delivered only as
`claimed_sender`, next to the stamped sender, and carries no authority.

## Sending and receiving

| Method | Does |
| --- | --- |
| `messages.subscribe` | Start receiving. `topics` lists exact topics, `service.*` prefixes, or `*`. Direct messages and requests to this connection arrive regardless. The reply names this connection's `instance`. Messages then arrive as `messages.deliver` events |
| `messages.publish` | Send to a `topic` (every subscriber whose topics match, except the sending connection) or to one `recipient` instance |
| `messages.request` | Send to one `recipient` and wait for its reply (`timeout_ms`, default 10 s, at most 60 s) |
| `messages.reply` | Answer a request delivered to this connection. Only the request's recipient can answer it |
| `messages.unsubscribe` | Stop receiving |

A message has a `schema` version, an optional `correlation` id, an
optional `expires_unix_ms`, and a `payload` of at most 64 KiB. The daemon
assigns each message an `id`. A direct message or request to an instance
that is not receiving fails with not found.

If a subscriber falls 256 messages behind, newer messages for it are
dropped, and its next delivery says how many it `missed`.

## Reserved topics

Topics under `dreamer.`, `human.`, `policy.`, and `approval.` belong to
the daemon (its lifecycle, human input, policy, and approvals). No client
can publish on them, whatever its grant, so no extension can impersonate
those events.

## Loops and limits

- **Causal depth.** A message that names its `cause` (the message it
  answers or reacts to) is one step further from its chain's `origin`. A
  reply is one step from its request. A chain may be at most 8 steps deep;
  the daemon refuses the next one with `-32006` (limited).
- **Rate.** Each connection may send 100 messages a second, with bursts of
  200 (publish, request, and reply together); beyond that, `-32006`.
- **No echo.** A topic message is never delivered back to the connection
  that published it.
- **Expiry.** A message whose expiry has passed is refused.

All `messages.*` methods go through the connection's grant like any
other method.
