//! The command that reaches a remote daemon through system OpenSSH
//! (spec 7.1, 11.2).
//!
//! There are two escaping layers, and both are handled here:
//!
//! 1. **Local process arguments.** `ssh` is run from an argv, never through
//!    a local shell, so each argument arrives exactly as built.
//! 2. **The remote command.** sshd hands the command string to the remote
//!    user's login shell (`$SHELL -c`). It is built from words, each quoted
//!    for a POSIX shell, so a path or socket name reaches the remote `dream`
//!    unchanged whatever it contains.
//!
//! The remote command is fixed: `dream bridge --stdio` with the remote tmux
//! options. Application data never goes into these arguments; it travels
//! inside the protocol. The user's SSH configuration decides everything
//! else (aliases, keys, agents, `known_hosts`, jump hosts): nothing here
//! overrides host-key checking, enables agent forwarding, or opens a port.

use std::fmt;

/// Where and how to reach one remote daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remote {
    /// An SSH destination: a host alias from the user's SSH configuration,
    /// `user@host`, or an `ssh://` URI.
    pub destination: String,
    /// The remote `dream` executable. `None` runs `dream` from the remote
    /// shell's `PATH`. A leading `~/` is expanded by the remote shell.
    pub dream: Option<String>,
    /// tmux options for the remote daemon, if it has to be started.
    pub tmux: Option<String>,
    pub tmux_socket: Option<String>,
    pub tmux_socket_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshError {
    /// The destination is empty, or could be read as an `ssh` option.
    BadDestination(String),
    /// A remote word contains a control character (such as a newline),
    /// which no remote shell quotes portably.
    BadWord(String),
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshError::BadDestination(d) => write!(f, "`{d}` is not a usable SSH destination"),
            SshError::BadWord(w) => write!(f, "{w:?} contains a control character"),
        }
    }
}

impl std::error::Error for SshError {}

/// How the connection is being made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A first connection: SSH may ask for a passphrase or a host-key
    /// decision through the user's askpass facilities.
    Interactive,
    /// An automatic reconnect: `BatchMode=yes`, so SSH fails instead of
    /// prompting, and a failure is reported rather than retried tightly.
    Batch,
}

/// The argv for `ssh` (element 0 is `ssh_program`).
pub fn command(ssh_program: &str, remote: &Remote, mode: Mode) -> Result<Vec<String>, SshError> {
    check_destination(&remote.destination)?;
    let mut argv = vec![ssh_program.to_owned()];
    // No PTY, no agent or X11 forwarding: a pipe carrying the protocol.
    argv.extend(["-T", "-a", "-x"].map(String::from));
    if mode == Mode::Batch {
        argv.extend(["-o".into(), "BatchMode=yes".into()]);
    }
    argv.push("--".into());
    argv.push(remote.destination.clone());
    argv.push(remote_command(remote)?);
    Ok(argv)
}

/// The argv for a visible first connection: the user's own `ssh` to the
/// destination, in a terminal, running `true`. With a terminal, SSH asks
/// its usual questions there (an unknown host key, a passphrase), and the
/// answers go into the user's own `known_hosts` and agent. Nothing is
/// answered or weakened for the user.
pub fn verify_command(ssh_program: &str, destination: &str) -> Result<Vec<String>, SshError> {
    check_destination(destination)?;
    Ok(vec![
        ssh_program.to_owned(),
        "--".into(),
        destination.to_owned(),
        "true".into(),
    ])
}

/// The one string sshd gives the remote shell.
pub fn remote_command(remote: &Remote) -> Result<String, SshError> {
    let dream = remote.dream.as_deref().unwrap_or("dream");
    let mut words = vec![program(dream)?];
    let mut add = |w: &str| -> Result<(), SshError> {
        words.push(quote(w)?);
        Ok(())
    };
    add("bridge")?;
    add("--stdio")?;
    if let Some(t) = &remote.tmux {
        add("--tmux")?;
        add(t)?;
    }
    if let Some(s) = &remote.tmux_socket {
        add("--tmux-socket")?;
        add(s)?;
    }
    if let Some(p) = &remote.tmux_socket_path {
        add("--tmux-socket-path")?;
        add(p)?;
    }
    Ok(words.join(" "))
}

/// The executable word: quoted, except that a leading `~/` stays bare so
/// the remote shell expands it to the remote home.
fn program(path: &str) -> Result<String, SshError> {
    match path.strip_prefix("~/") {
        Some(rest) if !rest.is_empty() => Ok(format!("~/{}", quote(rest)?)),
        _ => quote(path),
    }
}

/// Quote one word for a POSIX shell: bare when plainly safe, otherwise in
/// single quotes with each `'` closed, escaped, and reopened.
pub fn quote(word: &str) -> Result<String, SshError> {
    if word.chars().any(char::is_control) {
        return Err(SshError::BadWord(word.to_owned()));
    }
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    Ok(if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    })
}

fn check_destination(d: &str) -> Result<(), SshError> {
    let bad = d.is_empty()
        || d.starts_with('-')
        || d.chars().any(|c| c.is_whitespace() || c.is_control());
    if bad {
        Err(SshError::BadDestination(d.to_owned()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(destination: &str) -> Remote {
        Remote {
            destination: destination.into(),
            ..Remote::default()
        }
    }

    #[test]
    fn the_argv_has_no_pty_no_forwarding_and_ends_options_before_the_host() {
        assert_eq!(
            command("ssh", &remote("devbox"), Mode::Interactive).expect("argv"),
            [
                "ssh",
                "-T",
                "-a",
                "-x",
                "--",
                "devbox",
                "dream bridge --stdio"
            ]
        );
        assert_eq!(
            command("/usr/bin/ssh", &remote("me@devbox"), Mode::Batch).expect("argv"),
            [
                "/usr/bin/ssh",
                "-T",
                "-a",
                "-x",
                "-o",
                "BatchMode=yes",
                "--",
                "me@devbox",
                "dream bridge --stdio"
            ]
        );
    }

    #[test]
    fn destinations_that_could_be_options_are_refused() {
        for d in ["", "-oProxyCommand=sh", "-F/tmp/x", "dev box", "dev\nbox"] {
            assert!(
                matches!(
                    command("ssh", &remote(d), Mode::Batch),
                    Err(SshError::BadDestination(_))
                ),
                "{d:?}"
            );
        }
        assert!(command("ssh", &remote("ssh://me@devbox:2222"), Mode::Batch).is_ok());
    }

    #[test]
    fn remote_words_are_quoted_for_the_remote_shell() {
        let r = Remote {
            destination: "devbox".into(),
            dream: Some("~/.local/bin/dream".into()),
            tmux: Some("/opt/my tmux/bin/tmux".into()),
            tmux_socket: Some("it's $(x);`y`".into()),
            tmux_socket_path: None,
        };
        assert_eq!(
            remote_command(&r).expect("command"),
            r"~/.local/bin/dream bridge --stdio --tmux '/opt/my tmux/bin/tmux' --tmux-socket 'it'\''s $(x);`y`'"
        );
        assert_eq!(program("~/").expect("word"), "'~/'");
        assert_eq!(program("~/a b").expect("word"), "~/'a b'");
        assert_eq!(program("/usr/bin/dream").expect("word"), "/usr/bin/dream");
    }

    #[test]
    fn control_characters_in_remote_words_are_refused() {
        let r = Remote {
            destination: "devbox".into(),
            tmux_socket: Some("a\nb".into()),
            ..Remote::default()
        };
        assert!(matches!(remote_command(&r), Err(SshError::BadWord(_))));
    }
}
