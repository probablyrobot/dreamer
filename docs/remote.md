# Remote hosts

The local daemon reaches a daemon on another host through system OpenSSH.
There is no listener on either side: `ssh -T` runs `dream bridge --stdio`
on the remote host, and the protocol travels over that SSH session's
standard streams ([cli.md](cli.md#bridge), [wire.md](wire.md)).

## The SSH command

`client::ssh::command` builds it. For a host alias `devbox`:

```text
ssh -T -a -x -- devbox 'dream bridge --stdio'
```

| Part | Why |
| --- | --- |
| `-T` | No PTY: the session is a byte pipe |
| `-a`, `-x` | No agent or X11 forwarding |
| `-o BatchMode=yes` | Added for automatic reconnects only, so SSH fails instead of prompting |
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
