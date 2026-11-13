# Grants

Every connection to `dreamerd` holds a **grant**: what it may do. The daemon
checks it before every request, before the request acts on anything.
Nothing a client sends, and no terminal output, manifest, or model
response, can widen a grant. Only the daemon assigns one.

## Owner and narrow grants

- **Owner.** The daemon's socket is owner-only, so whoever connects to it is
  the user. The user's own clients (the desktop, `dream`, scripts the user
  runs) hold the owner grant: everything.
- **Narrow.** A registered pane app holds a narrow grant (registration is
  described below, once launch tickets land). It lists:
  - **methods:** exact names such as `terminal.read`, or a whole service
    such as `tmux.*` (which covers `tmux.topology`, not `tmuxfoo.bar`);
  - **panes:** the panes it may read, follow, type into, or act on, or
    `all_panes`;
  - **an expiry:** after it, every request is refused.

`grants.current` returns the calling connection's own grant.

## What the pane scope covers

| Request | Needs |
| --- | --- |
| `terminal.read`, `terminal.subscribe`, `terminal.input`, `terminal.set_tier` | The named pane in scope |
| `actions.submit` on a pane (split, resize, select, zoom, input, keys, kill) | That pane in scope |
| `actions.submit` on a window or session (select, rename, kill, new window) | `all_panes` |
| `terminal.mirrors`, `daemon.shutdown`, every `hosts.*` method | `all_panes` |
| Metadata and the caller's own state (`tmux.topology`, `tmux.watch`, `daemon.version`, `actions.get`, `terminal.unsubscribe`, …) | Only the method |

A refusal is the error `-32005` (`FORBIDDEN`), naming the method or the
pane. The grant is checked when a request arrives. A subscription that was
already allowed keeps streaming until it ends; an expired grant stops new
requests, not running streams.
