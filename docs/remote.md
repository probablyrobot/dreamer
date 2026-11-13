# Remote hosts

The local daemon reaches a daemon on another host through system OpenSSH.
There is no listener on either side: `ssh -T` runs `dream bridge --stdio`
on the remote host, and the protocol travels over that SSH session's
standard streams ([cli.md](cli.md#bridge), [wire.md](wire.md)).

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
| `incompatible` | The remote daemon rejects the handshake (protocol or build family) | No |
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
