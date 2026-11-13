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
  `failed` with the reason and SSH's last lines. There is no automatic
  reconnect yet.
- **Forwarding.** `hosts.call` sends one typed call to the remote daemon
  as encoded Protobuf, without decoding it, and returns the encoded result.
  Streaming methods (`terminal.subscribe`, `terminal.unsubscribe`,
  `tmux.watch`) are refused, as are `hosts.*` methods: a remote daemon
  never becomes a path to further hosts.
- **Disconnecting.** `hosts.disconnect` closes SSH's input; the remote
  bridge ends, and the remote daemon, its tmux, and every pane keep running.
- **Non-interactive.** The daemon has no terminal to prompt on, so it runs
  SSH with `BatchMode=yes`: keys must be available from an agent or an
  unencrypted key file, and the host key must already be known. An
  unknown host key or a passphrase prompt fails with SSH's own message in
  `detail`.

Remote daemons receive none of the local user's keys or grants.

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
