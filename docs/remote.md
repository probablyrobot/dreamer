# Remote hosts

The local daemon reaches a daemon on another host through system OpenSSH.
There is no listener on either side: `ssh -T` runs `dream bridge --stdio`
on the remote host, and the protocol travels over that SSH session's
standard streams ([cli.md](cli.md#bridge), [wire.md](wire.md)).

SSH is the only way between hosts. Dreamer opens no network sockets of its
own, so every byte between the main terminal and a host is SSH's
encrypted, authenticated channel, under your own SSH configuration and
keys. `cargo xtask policy` (the gate's first stage) fails if product code
uses a TCP or UDP socket. The one exception is [forwards](#forwards):
a listener on this machine's 127.0.0.1 and connections to a host's own
loopback, both in `crates/server/src/forward.rs`, which the policy allows
TCP only on the loopback. The HTTP client is allowed in two files only,
neither of which reaches a Dreamer host: app `fetch`
([dependency-baseline.md](dependency-baseline.md#http-client-fetch-in-script-apps)) and the
`openai` memory embedder ([memory.md](memory.md#openai)), which runs only
when configured.

## The coordinator

The local daemon is the coordinator: it owns outbound connections and
exposes them to local clients through its API (`hosts.connect`,
`hosts.list`, `hosts.call`, `hosts.disconnect`; on the command line,
`dream hosts`, see [cli.md](cli.md#remote-hosts)).

- **Connecting.** `hosts.connect` runs system `ssh` with the command below,
  then performs the ordinary client handshake over the SSH child's pipes,
  allowing 30 seconds for SSH and the remote daemon together. The remote
  `dream bridge --stdio` starts the remote daemon if it is not running.
- **Ownership.** A connection belongs to the local daemon, not to the
  client that asked for it. It stays up after that client (the GUI, or a
  `dream` command) exits.
- **Diagnostics.** SSH's standard error is read separately and never
  parsed as protocol. The last eight lines are kept per host, written to
  the daemon log, and shown in the host's `detail` when it is not live.
- **State.** `hosts.list` reports `connecting`, `live`, `failed`, or
  `closed`, the remote daemon's host, process ID, instance, and version,
  and the time of the last successful exchange. When the connection drops
  (SSH ends, the network goes, the remote daemon stops), the host becomes
  `failed` with the reason, the kind of failure, and SSH's last lines.
  See [Reconnecting](#reconnecting).
- **Forwarding.** `hosts.call` sends one typed call to the remote daemon
  as encoded Protobuf, without decoding it, and returns the encoded result.
  Streaming methods (`terminal.subscribe`, `terminal.unsubscribe`,
  `tmux.watch`) are refused there; they go through `hosts.subscribe` and
  `hosts.watch` ([Remote streams](#remote-streams)). `hosts.*` methods are
  refused too: a remote daemon never becomes a path to further hosts.
  A call in flight when the link drops fails and is never sent again, so
  input and actions are never replayed automatically.
- **Disconnecting.** `hosts.disconnect` closes SSH's input; the remote
  bridge ends, and the remote daemon, its tmux, and every pane keep running.
- **Non-interactive.** The daemon has no terminal to prompt on, so it runs
  SSH with `BatchMode=yes`: keys must be available from an agent or an
  unencrypted key file, and the host key must already be known. An
  unknown host key or a passphrase prompt fails with SSH's own message in
  `detail`.

Remote daemons receive none of the local user's keys or grants.

**Survival.** A host's link, its relayed subscriptions, and its automatic
reconnects belong to the local daemon, so they continue when every local
client (the desktop included) has gone. When SSH drops, the remote daemon,
its tmux, and its programs keep running, and the reconnect reaches the
same daemon instance with the same journal. `crates/server/tests/hosts.rs`
checks both.

## First connection

The coordinator's SSH never prompts (`BatchMode=yes`). A host whose key is
not yet known, or whose key needs a passphrase that no agent holds, fails
with `host_key` or `auth` and is not retried. The user answers SSH's
questions in a visible connection instead:

- `dream hosts verify NAME` runs the user's own `ssh -- DESTINATION true`
  in the current terminal. SSH asks its usual questions there, and the
  answers go into the user's own `known_hosts` and agent. On success it
  asks the coordinator to reconnect; on failure SSH's message stays on
  screen and the host stays as it was. `--wait` waits for Enter before
  ending.
- In the desktop, clicking such a host in the sidebar, or "Verify host in a
  terminal…" in the palette, opens a local tmux window running that
  command, so the prompt appears in an ordinary pane.

Dreamer never accepts a host key for the user, never turns off host-key
checking, and never stores a passphrase. A changed host key fails exactly
as SSH reports it, with no option to bypass it. For the coordinator to keep
connecting after a passphrase prompt, the key must stay available
non-interactively, for example in an agent; that is the user's SSH
configuration.

## Installing a server

The coordinator can install this daemon's own server version on a host,
from the headless archives ([packaging.md](packaging.md#headless-server-archive)).
It is one reviewable operation:

1. **Probe.** `hosts.install_plan` runs a fixed script on the host over
   SSH: `uname -s`, `uname -m`, `$HOME`, and the installed `VERSION`, if
   any. Linux x86-64, Linux ARM64, and macOS ARM64 have archives. The
   archive for that target and this version is taken from the artifacts
   directory: the one given, else `DREAMER_SERVER_ARTIFACTS`, else
   `share/dreamer/server` beside the daemon. Its SHA-256 is checked against
   its `.sha256` here.
2. **Review.** The plan lists every change in plain words, with the
   archive, its size and SHA-256, and the remote paths. It has an id.
3. **Install.** `hosts.install` probes again and refuses if the plan is no
   longer the one reviewed. It streams the archive over SSH's standard
   input to a script that:
   - checks the SHA-256 on the host before unpacking anything;
   - unpacks into `~/.local/share/dreamer/versions/<version>/`, and runs
     the unpacked `dream --version` there;
   - points `~/.local/share/dreamer/current` at it with one atomic rename
     (`mv -T`, or `mv -fh` on BSD).

   A failure at any step leaves the previous version in place and removes
   the partial upload.

Nothing outside `~/.local/share/dreamer` changes: no root, no package
manager, no shell profiles. Scripts run under `sh` whatever the login
shell is (`sh -c '<script>' sh <arguments>`), and their variable parts are
quoted arguments, never text pasted into the script.

When a host's profile names no `dream`, the bridge command prefers
`~/.local/share/dreamer/current/bin/dream` and falls back to `dream` on the
remote `PATH`.

`dream hosts install NAME [--artifacts DIR] [--yes]` prints the plan, asks
before installing (unless `--yes`), installs, and connects. In the desktop,
a host that failed as `not_installed` or `incompatible` says "click to
install"; the click, or "Install or update the server on a host…" in the
palette, opens a local tmux window running that command.

## Incompatible hosts

A host's daemon that speaks another protocol major version, or comes from
another build family, refuses the coordinator in the handshake, before
`Welcome`. Nothing of the host's (no topology, no snapshot) is decoded.
The host is marked `incompatible`, is never retried by itself, and says
what it is and what this one is. `dream hosts list` shows each live host's
version, protocol, and build family.

The fix is an upgrade. With no usable link to the old daemon, the newly
installed `dream shutdown` stops it on the host; a daemon from another
build is stopped by the process ID in its startup lock. Then the
coordinator connects to the new one.

## Upgrading a server

`hosts.upgrade` installs the reviewed plan, then restarts the host's
daemon on it:

1. Install as above.
2. Ask the running daemon to shut down through the link. Its tmux
   sessions, panes, and programs keep running.
3. Reconnect: the bridge starts the new version.

This is a restart, not a handover. Relayed subscriptions end with
`host_restarted`, and views start again from fresh checkpoints. The
result says so, with the daemon's process ID before and after.

- **Same version:** if the same version is installed and running, nothing
  changes.
- **Older version:** if the host has a newer version than this one, the
  upgrade is refused unless `allow_older` is set
  (`dream hosts upgrade NAME --allow-older`).

`dream hosts upgrade NAME [--artifacts DIR] [--yes] [--allow-older]` shows
the plan plus the restart step and asks first. The desktop's "Install or
update the server on a host…", and its "click to install" on a host,
run `dream hosts upgrade NAME`, which installs where nothing is installed.

## Remote streams

- **`hosts.subscribe`** follows a pane on a connected host. The reply and
  the `terminal.records`, `terminal.checkpoint`, and `terminal.end` events
  are exactly those of `terminal.subscribe`, under a subscription id of the
  local connection. `terminal.unsubscribe` ends it and unsubscribes on the
  host.
- **`hosts.watch`** returns the host's topology, then sends it as
  `tmux.topology` events after every change. The coordinator renumbers
  revisions, so they keep increasing across reconnects and remote
  restarts. One topology watch (local or a host) per connection.

**When SSH drops.** The local subscription stays open while the host
reconnects ([Reconnecting](#reconnecting)). On the new link the
coordinator subscribes again, resuming after the last record it
delivered. The remote daemon kept running and its mirror stayed valid, so
its journal replays only what was missed. tmux is not captured again and
no checkpoint is sent. Records already delivered are not sent twice.

When it cannot resume, the subscription ends with a reason, and the client
subscribes again for a fresh checkpoint:

| End reason | When |
| --- | --- |
| `host_restarted` | The remote daemon restarted (new instance), so its journal is gone |
| `connection_lost` | The remote daemon lost its tmux connection (new epoch) |
| `lagged` | The remote journal no longer holds the missed records, or the local client fell behind the relay |
| `pane_closed` | The pane no longer exists on the host |
| `host_lost` | The host was disconnected, or failed in a way that is not retried |

Relayed data waits in bounded queues: up to 1024 items per relay, and up
to 4096 per remote subscription that arrived before its reply. A relay
that overflows them ends as `lagged` rather than holding up other panes on
the same host.

`crates/server/tests/hosts.rs` follows a remote pane through the
coordinator into a replica and cuts SSH under it while the pane keeps
printing. The replica catches up from the journal with no second
checkpoint and matches the remote mirror. Killing the remote daemon then
ends the subscription with `host_restarted`. A second test keeps a host's
topology updating across a cut.

`cargo xtask bench remote` times key to echo in a host's pane through
this path, split into the link, the host's processing, and the rest
([performance.md](performance.md)).

## Forwards

A web pane in a host's window shows a page on that host's own loopback
(a dev server on `localhost:3000`) through a forward over the host's
existing SSH connection ([desktop.md](desktop.md#pages-of-a-remote-host)).

- **`hosts.forward`** `{name, port}` returns a port on this machine's
  127.0.0.1 (ephemeral, chosen by the system) that reaches `port` on the
  host's loopback (127.0.0.1, else ::1). Nothing is bound on any other
  address, here or on the host, and the host side only connects.
- **Holds.** The forward is held by the calling connection until
  `hosts.unforward` `{forward}` or until that connection closes. Holds of
  one host port share one listener, which closes, with every connection
  through it, when its last hold goes. `hosts.unforward` of the last hold
  returns once the port is closed.
- **Streams.** Each connection accepted on the local port becomes a
  stream on the host's link: `forward.open` `{port}` on the host's daemon
  connects there, `forward.write` carries the browser's bytes (with `eof`
  for the end of its side), the host's bytes come back as `forward.data`,
  and `forward.close` ends the stream. `forward.*` are the coordinator's:
  the owner grant only, and never forwarded by `hosts.call`. A host's
  streams close with the connection that opened them; when the link drops,
  connections through the forward close, and new ones use the next link.
- **Terminal output first.** Data from the host travels only within the
  credit the coordinator gives as it passes data on (`forward.ack`, at most
  128 KiB per stream), as bulk records that the host's daemon writes only
  when no terminal record, reply, or other frame is waiting. Data to the
  host goes one 16 KiB `forward.write` at a time per stream. A busy
  forward therefore never queues terminal records behind it; what remains
  is SSH's own buffering of the data already in flight.
- **Grants.** The owner may forward. A narrow grant needs `web.forward`
  ([grants.md](grants.md#reaching-a-hosts-loopback)).

`crates/server/tests/hosts.rs` forwards a port through a stand-in SSH:
four 8 MiB pages at once arrive byte for byte while terminal echoes on
the host stay on time, the listener refuses connections from this
machine's other addresses, a second hold shares the listener, and the
listener closes with its last hold and with a closed connection's holds.
A connection without `web.forward` is refused.

## Profiles

Hosts can be defined in `hosts.yaml` in the configuration directory
(`~/.config/dreamer/hosts.yaml`, or under `DREAMER_CONFIG_DIR`):

```yaml
hosts:
  - name: devbox                  # the label; unique
    destination: devbox           # SSH host alias, user@host, or ssh:// URI
    dream: ~/.local/bin/dream     # optional; default `dream` on the remote PATH
    tmux: /usr/bin/tmux           # optional remote tmux executable
    tmux_socket: work             # optional; or tmux_socket_path, not both
    workspace: default            # optional workspace namespace
    tags: [work, linux]           # optional, for grouping
    reconnect: auto               # auto (default) or manual
```

Profiles hold no credentials. Unknown keys and invalid values are errors
with the file, line, and column; `hosts.list` reports such an error in
`profiles_error` and still lists hosts connected directly. The daemon reads
the file on every `hosts.list`, `hosts.connect`, and `hosts.reconnect`, so
edits take effect without a restart.

`hosts.connect` with only a name connects to that profile. `hosts.list`
shows every profile (`not connected` until first used), then hosts given
directly with a destination, which reconnect only when asked unless
`auto_reconnect` is set.

## Reconnecting

Every failed attempt is classified, and only a `transient` failure is
retried by itself:

| Failure | Seen as | Retried automatically |
| --- | --- | --- |
| `transient` | Network errors, SSH ending, the remote daemon going away | Yes, under `reconnect: auto` |
| `auth` | SSH says `Permission denied`, `Too many authentication failures`, or `No more authentication methods` | No |
| `host_key` | `Host key verification failed`, a changed host identification, or no matching host key type | No |
| `incompatible` | The remote daemon rejects the handshake (protocol or build family). The detail names both sides' version, protocol, and build family, and the fix: `dream hosts upgrade NAME` | No |
| `not_installed` | The remote shell cannot find `dream` (exit status 127 or `command not found`), or `ssh` itself cannot be run | No |

Retries wait 1 s, doubling to at most 60 s, with up to a fifth taken off at
random so hosts that dropped together do not retry together. The status
shows the failed attempts and the next retry time. A failure that is not
retried stays until `hosts.reconnect` (`dream hosts reconnect NAME`), so a
refused login never becomes a loop. `hosts.disconnect` cancels any pending
retry. A reconnect reaches the same remote daemon if it kept running; the
remote side's `remote_instance` changes only when that daemon restarted.

## The SSH command

`client::ssh::command` builds it. For a host alias `devbox`:

```text
ssh -T -a -x -- devbox 'dream bridge --stdio'
```

| Part | Why |
| --- | --- |
| `-T` | No PTY: the session is a byte pipe |
| `-a`, `-x` | No agent or X11 forwarding |
| `-o BatchMode=yes` | SSH fails instead of prompting (the daemon has no terminal) |
| `--` | Ends options; the destination cannot be read as one |

Everything else comes from the user's SSH configuration: host aliases,
keys, agents, `known_hosts`, `ProxyJump`, and connection sharing. Dreamer
never disables host-key checking, enables agent forwarding, or edits SSH
configuration. Credentials stay in the user's SSH facilities.

The destination must be non-empty, must not start with `-`, and must not
contain whitespace or control characters. `user@host` and `ssh://` URIs are
accepted.

### Two escaping layers

1. **Local arguments.** `ssh` is started from an argv, never through a
   local shell.
2. **The remote command.** sshd passes the command string to the remote
   user's login shell. The command is built from words, each quoted for a
   POSIX shell: bare when it contains only letters, digits, and `-_./:=@%+,`,
   otherwise in single quotes with any `'` written as `'\''`. A remote
   executable path that starts with `~/` keeps that prefix bare so the
   remote shell expands it to the remote home.

Words with control characters (such as a newline) are refused, since no
remote shell quotes them portably. The remote login shell must accept POSIX
single-quote syntax (sh, bash, zsh, dash, and fish do).

The remote command is fixed: the remote `dream`, `bridge --stdio`, and the
remote daemon's tmux options (`--tmux`, `--tmux-socket`,
`--tmux-socket-path`). Application data never goes into these arguments.

`crates/client/tests/ssh.rs` runs the generated argv through a stand-in
`ssh` that hands the command to `/bin/sh -c`, as sshd does, and checks that
the remote side receives exactly the original words for a socket name full
of quotes, `$(…)`, backquotes, globs, and separators.

## Acceptance through real OpenSSH

`cargo xtask remote-acceptance` runs the section 18.2 remote cases through
real OpenSSH on this machine, with nothing of the user's SSH setup
involved:

- **Setup:** an unprivileged `sshd` listens on a free loopback port. It
  has a generated host key and accepts only a generated client key, with
  its own `sshd_config`, `known_hosts`, and client config.
- **Privacy:** its sessions get a private HOME and private daemon
  directories through `SetEnv`. Before installing anything, the run
  checks that the session's HOME really is the private one.
- **Cases:**
  - probe and plan;
  - install this build's server archive;
  - connect with no PTY and no listener;
  - cut SSH under a followed pane, which resumes from the journal with no
    second checkpoint and the same remote daemon;
  - input sent while the link is down, which is never replayed;
  - a new view's megabyte checkpoint while timing small calls, to show
    there is no starvation;
  - a page served only on the host's loopback, fetched through a
    forward (`hosts.forward`) byte for byte while the host's terminal
    output is timed, and the forward closed with its hold;
  - a remote daemon restart, which ends the view with `host_restarted`.
- **Cleanup:** everything started from the private directory is stopped,
  and the directory is removed.

It needs `sshd` (on Linux, the `openssh-server` package). The Mac's full
gate runs it on ARM64 Linux in a container, as root, before every push;
the weekly Linux workflow (`.github/workflows/full.yml`, also run on
version tags and on demand) runs it on x86-64 as an unprivileged user.
Incompatible-build cases run against a stand-in SSH in
`crates/server/tests/hosts.rs`.
