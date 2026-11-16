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
  - **an expiry:** after it, every request is refused.

`grants.current` returns the calling connection's own grant.
`grants.narrow` replaces it with one within it: a connection may give up
authority (for example `dream mcp --pane %3`), never gain it.

## What the pane scope covers

| Request | Needs |
| --- | --- |
| `terminal.read`, `terminal.subscribe`, `terminal.input`, `terminal.set_tier` | The named pane in scope |
| `taps.subscribe` | The listed panes in scope; `every_pane` needs `all_panes` (see [taps.md](taps.md)) |
| `agents.runs` | `all_panes` |
| `processes.list` | The listed panes in scope; no panes (every process) needs `all_panes` |
| `jobs.start`, `jobs.cancel` | `all_panes` (a job gets its own window; cancelling signals a process) |
| `terminal.query` | The listed panes in scope; no panes (every pane) or any `hosts` needs `all_panes` |
| `actions.submit` on a pane (split, resize, select, zoom, input, keys, kill) | That pane in scope |
| `actions.submit` on a window or session (select, rename, kill, new window) | `all_panes` |
| `terminal.mirrors`, `daemon.shutdown`, every `hosts.*` method | `all_panes` |
| Metadata and the caller's own state (`tmux.topology`, `tmux.watch`, `daemon.version`, `actions.get`, `terminal.unsubscribe`, …) | Only the method |
| `extensions.restart`, `apps.launch`, `scripts.load`, `scripts.reload`, `scripts.stop` | The owner grant: no app, extension, script, or filter, whatever its grant lists |
| `filters.install`, `filters.list`, `filters.pause`, `filters.resume`, `filters.remove` | The owner grant, or `filters.manage` plus content read; see [Filters an app installs](filters.md#filters-an-app-installs) |
| `messages.*` | Only the method; messages never reach a pane (see [messages.md](messages.md)) |
| `events.subscribe` | Only the method; it then delivers only events about panes in scope, and events about no pane only with `all_panes` (see [events.md](events.md)) |

A refusal is the error `-32005` (`FORBIDDEN`), naming the method or the
pane. The grant is checked when a request arrives. A subscription that was
already allowed keeps streaming until it ends; an expired grant stops new
requests, not running streams.

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
