# Grants

Every connection to `dreamerd` holds a **grant**: what it may do. The daemon
checks it before every request, before the request acts on anything.
Nothing a client sends, and no terminal output, manifest, or model
response, can widen a grant. Only the daemon assigns one.

## Owner and narrow grants

- **Owner.** The daemon's socket is owner-only, so whoever connects to it is
  the user. The user's own clients (the desktop, `dream`, scripts the user
  runs) hold the owner grant: everything.
- **Narrow.** A registered pane app holds a narrow grant (see
  [Launching and registering apps](#launching-and-registering-apps)). It
  lists:
  - **methods:** exact names such as `terminal.read`, or a whole service
    such as `tmux.*` (which covers `tmux.topology`, not `tmuxfoo.bar`);
  - **panes:** the panes it may read, follow, type into, or act on, or
    `all_panes`;
  - **an expiry:** after it, every request is refused;
  - **an act level**, for agents: see
    [Act levels and approvals](#act-levels-and-approvals).

`grants.current` returns the calling connection's own grant.
`grants.narrow` replaces it with one within it: a connection may give up
authority (for example `dream mcp --pane %3`), never gain it.

## What the pane scope covers

| Request | Needs |
| --- | --- |
| `terminal.read`, `terminal.subscribe`, `terminal.input`, `terminal.set_tier`, `terminal.watch` | The named pane in scope |
| `terminal.run` | The pane it splits in scope; without one (a new window), `all_panes` |
| `taps.subscribe` | The listed panes in scope; `every_pane` needs `all_panes` (see [taps.md](taps.md)) |
| `agents.runs` | `all_panes` |
| `processes.list` | The listed panes in scope; no panes (every process) needs `all_panes` |
| `jobs.start`, `jobs.cancel` | `all_panes` (a job gets its own window; cancelling signals a process) |
| `collect.create` | The listed panes in scope when `from` lists panes; any other `from` needs `all_panes` (see [collectors.md](collectors.md#grants)) |
| `collect.list`, `collect.query`, `collect.stop` | `all_panes` |
| `terminal.query` | The listed panes in scope; no panes (every pane) or any `hosts` needs `all_panes` |
| `actions.submit` on a pane (split, resize, select, zoom, input, keys, kill) | That pane in scope |
| `actions.submit` on a window or session (select, rename, kill, new window) | `all_panes` |
| `terminal.mirrors`, `daemon.shutdown`, every `hosts.*` method | `all_panes` |
| Metadata and the caller's own state (`tmux.topology`, `tmux.watch`, `terminal.list`, `daemon.version`, `actions.get`, `terminal.unsubscribe`, …) | Only the method |
| `approvals.list`, `approvals.answer` | The owner grant: an agent never answers its own approvals |
| `drawer.toggle` | The owner grant: it starts the user's harness with the user's authority. `drawer.status` needs only the method |
| `extensions.restart`, `apps.launch`, `scripts.load`, `scripts.reload`, `scripts.stop` | The owner grant: no app, extension, script, or filter, whatever its grant lists |
| `apps.view`, `apps.input` | The owner grant: app panes are the user's to read and type into |
| `apps.publish` | A registered app host's own connection, whatever its grant lists; it publishes for its own pane only |
| `filters.install`, `filters.list`, `filters.pause`, `filters.resume`, `filters.remove` | The owner grant, or `filters.manage` plus content read; see [Filters an app installs](filters.md#filters-an-app-installs) |
| `messages.*` | Only the method; messages never reach a pane (see [messages.md](messages.md)) |
| `agent.sessions.*` | Only the method. A new session's grant must be within the caller's, and a caller other than the owner sees only the sessions it created (see [agents.md](agents.md#grants)) |
| `events.subscribe` | Only the method; it then delivers only events about panes in scope, and events about no pane only with `all_panes` (see [events.md](events.md)) |

Panes the connection created itself (a split, a new window, a job, a
`terminal.run`) are in its scope too, whatever its pane list says.

A refusal is the error `-32005` (`FORBIDDEN`), naming the method or the
pane. The grant is checked when a request arrives. A subscription that was
already allowed keeps streaming until it ends; an expired grant stops new
requests, not running streams.

## Act levels and approvals

An agent's grant has an act level on top of its methods and panes. The
level decides how far the agent may act on panes; the methods and panes
still decide what it may call at all. A grant without a level (every
grant from before levels, the owner's included) is unaffected.

| Level | May | Asks the user |
| --- | --- | --- |
| `observe` | List, read, search, and watch every pane in scope | Never; any action is refused |
| `own` | Also type, run, and change the layout in panes and windows this connection created | Never; acting on another pane is refused |
| `elsewhere` | Also act on other panes | Each time, or once for the pane; closing always asks |

What counts as acting, and on which panes:

| Request | Acts on |
| --- | --- |
| `terminal.input`, `actions.submit` input and keys | The pane |
| `leases.acquire` | The pane, but never asks: the input sent with the lease asks |
| `actions.submit` split, resize, select, zoom | The pane |
| `actions.submit` move a pane | The pane, and the panes of the window it moves into |
| `actions.submit` select or rename a window, rename a session | Every pane of that window or session |
| `actions.submit` close a pane or a window, `jobs.cancel` | Those panes; closing |
| `terminal.run` with `split` | The pane it splits |
| `jobs.start`, `terminal.run` without `split`, `actions.submit` new window | Nothing: they create, and the connection owns what they create |

Reading, searching, watching, and listing are never acting. Neither is
starting or querying a collector: collectors only read panes, so any
level may create and query them. Stopping one is an action (it may be the
user's), given from `own` up ([collectors.md](collectors.md#grants)).

**Ownership.** A connection owns the panes it creates: the new pane of a
split or new window, a job's pane, a `terminal.run` pane. Ownership
belongs to the connection and ends when it closes; a pane created by
another connection, or before this one connected, is not its own.

**Approvals.** At `elsewhere`, a request that acts on panes the connection
does not own waits while the user decides:

1. The daemon records a pending approval: who asks, the method, what it
   would do in words, the panes, and whether it closes something. An
   `approvals.requested` event announces it ([events.md](events.md)).
2. The user answers with `approvals.answer` (`dream approvals`):
   - `once`: this action runs;
   - `pane`: it runs, and later actions of this connection in the same
     panes run without asking, except closing;
   - `deny`: it is refused.
3. With no answer within 60 seconds it is refused as denied. When the
   connection closes, its pending approvals go with it.

A refused or denied action fails with `FORBIDDEN` and a message saying
why. `approvals.list`
lists what is waiting. Only the owner grant may list or answer approvals.

**Typing** goes through an interaction lease as for any automation
([input.md](input.md)): a person's keystroke in the pane revokes the
lease at once, and the agent's next input with it fails.

**Narrowing** may lower the level, never raise it; no level counts as
above `elsewhere`. `dream mcp --level LEVEL` narrows its connection to an
agent's grant: every pane, the methods of its tools, and the level.

Levels govern what the agent does through the daemon on this host. A
program it runs has the user's privileges, and `hosts.call` forwards to
another host's daemon as the owner there, so an agent's grant should not
include `hosts.call` unless it may act there freely.

`crates/server/tests/agent_tools.rs` checks this against a real tmux: an
observing agent reads and searches a pane it did not create without being
asked and is refused acting; at `own`, typing into another pane is
refused; at `elsewhere`, typing waits for an approval, is refused when
denied, works once allowed for the pane, and closing still asks and times
out as denied; a person's keystroke ends the agent's lease.

## Launching and registering apps

An app gets its narrow grant through a launch the user starts:

1. **Launch.** `apps.launch` (owner only; on the command line,
   `dream app run APP_ID --method … -- PROGRAM ARGS`) names the app, its
   grant, and the program. The daemon:
   - writes a random single-use ticket (32 bytes) to an owner-only file in
     its private runtime directory (`launch/<launch id>`, mode 0600);
   - creates a tmux window running the program, with only
     `DREAMER_LAUNCH_ID`, `DREAMER_LAUNCH_TICKET` (the file's path), and
     `DREAMER_RUNTIME_DIR` added to its environment;
   - records the new pane.

   The ticket itself appears in no argv, pane, window name, tmux option,
   or log line.
2. **Register.** The program connects and calls `apps.register` with the
   launch id and the file's contents. The daemon:
   - uses the ticket up (removes it and its file), whatever happens next;
   - refuses an expired ticket (60 s by default, `ttl_ms` to change it) or
     a wrong one;
   - reads the connecting process from the socket's peer credentials and
     refuses unless it is the pane's process or descends from it.

   The connection then holds the app's grant. With no panes given, the
   scope is the app's own pane.
3. **Afterwards.** The grant belongs to that connection and ends with it.
   A registered connection cannot register again or launch apps, and no
   grant can include launching apps.

A brand-new pane can take a moment to be mirrored. `terminal.read` on a
pane tmux already has waits up to 3 s for its mirror instead of failing,
so an app can read its own pane right after launch.

### What this does and does not protect

The socket is owner-only, so any process of the same user can still
connect directly and act as the owner. Grants confine apps started through
Dreamer's launch, scripts in the sandboxed tier, and future remote and
agent clients. They are not an OS sandbox (spec 12.2): an app with native
code runs with the user's privileges.

`crates/cli/tests/cli.rs` launches a probe app into a real tmux pane. It
registers, reads its own pane, is refused another pane, `daemon.shutdown`,
and launching an app, and cannot register twice. An expired ticket and a
process outside the pane are refused. The ticket appears in no process's
argv, the pane, tmux's environment, options, or window names, or the
daemon log.
