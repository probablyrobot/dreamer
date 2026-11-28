//! Which tmux server Dreamer drives: an option given on the command line,
//! else `tmux:` in `dreamer.yaml`, else Dreamer's own server, `-L dreamer`.
//!
//! Dreamer starts only its own server. The user's default server, or any
//! other, is driven only when chosen, and only once it is running.

use std::path::PathBuf;

use workspace_config::{TmuxConfig, TmuxServerChoice, TmuxSettings};

use crate::connection::Socket;

/// The socket name of Dreamer's own tmux server.
pub const OWN_SOCKET: &str = "dreamer";

/// `-L default` names the user's default server (tmux's own socket name
/// for it).
pub const DEFAULT_SOCKET: &str = "default";

/// The server to drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub socket: Socket,
    /// Dreamer's own server: Dreamer starts it when it is not running.
    pub own: bool,
    /// `-f` for starting Dreamer's own server: `None` lets tmux find the
    /// user's configuration.
    pub config_file: Option<PathBuf>,
}

impl Server {
    /// `default`, `-L NAME`, or `-S PATH`, as `dream status` says it.
    pub fn describe(&self) -> String {
        match &self.socket {
            Socket::Default => "your default tmux server".into(),
            Socket::Name(n) if self.own => format!("Dreamer's own (-L {n})"),
            Socket::Name(n) => format!("-L {n}"),
            Socket::Path(p) => format!("-S {}", p.display()),
        }
    }
}

/// The server for `flag` (from `-L`/`--tmux-socket` or
/// `--tmux-socket-path`), else `settings`.
pub fn choose(flag: Option<Socket>, settings: &TmuxSettings) -> Server {
    let socket = match flag {
        Some(Socket::Name(n)) if n == DEFAULT_SOCKET => Socket::Default,
        Some(s) => s,
        None => match &settings.server {
            TmuxServerChoice::Own => Socket::Name(OWN_SOCKET.into()),
            TmuxServerChoice::Default => Socket::Default,
            TmuxServerChoice::Name(n) => Socket::Name(n.clone()),
            TmuxServerChoice::Path(p) => Socket::Path(p.clone()),
        },
    };
    let own = socket == Socket::Name(OWN_SOCKET.into());
    let config_file = match (&settings.config, own) {
        (_, false) | (TmuxConfig::User, true) => None,
        (TmuxConfig::None, true) => Some(PathBuf::from("/dev/null")),
        (TmuxConfig::Path(p), true) => Some(p.clone()),
    };
    Server {
        socket,
        own,
        config_file,
    }
}

/// Why `socket` is off limits under `DREAMER_FORBID_DEFAULT_TMUX`, which
/// test harnesses set so nothing can reach the user's own tmux: the
/// default server, and Dreamer's own unless `TMUX_TMPDIR` puts its socket
/// in a private folder.
pub fn forbidden(socket: &Socket) -> Option<&'static str> {
    std::env::var_os("DREAMER_FORBID_DEFAULT_TMUX")?;
    match socket {
        Socket::Default => {
            Some("the default tmux server is off limits here (DREAMER_FORBID_DEFAULT_TMUX)")
        }
        Socket::Name(n) if n == OWN_SOCKET && std::env::var_os("TMUX_TMPDIR").is_none() => Some(
            "Dreamer's own tmux server is off limits here without a private TMUX_TMPDIR (DREAMER_FORBID_DEFAULT_TMUX)",
        ),
        _ => None,
    }
}

/// The tmux arguments that select `socket`.
pub fn socket_args(socket: &Socket) -> Vec<std::ffi::OsString> {
    match socket {
        Socket::Default => Vec::new(),
        Socket::Name(n) => vec!["-L".into(), n.into()],
        Socket::Path(p) => vec!["-S".into(), p.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(server: TmuxServerChoice, config: TmuxConfig) -> TmuxSettings {
        TmuxSettings { server, config }
    }

    #[test]
    fn dreamer_drives_its_own_server_unless_told_otherwise() {
        let s = choose(None, &TmuxSettings::default());
        assert_eq!(s.socket, Socket::Name("dreamer".into()));
        assert!(s.own && s.config_file.is_none());

        let mine = settings(TmuxServerChoice::Default, TmuxConfig::None);
        let s = choose(None, &mine);
        assert_eq!(
            (s.socket, s.own, s.config_file),
            (Socket::Default, false, None)
        );

        // A flag comes first; `-L default` is the default server.
        let s = choose(
            Some(Socket::Name("default".into())),
            &TmuxSettings::default(),
        );
        assert_eq!(s.socket, Socket::Default);
        let s = choose(Some(Socket::Name("work".into())), &TmuxSettings::default());
        assert_eq!((s.socket, s.own), (Socket::Name("work".into()), false));
        let s = choose(Some(Socket::Path("/tmp/s".into())), &mine);
        assert_eq!(s.socket, Socket::Path("/tmp/s".into()));
    }

    #[test]
    fn the_configuration_applies_only_to_dreamers_own_server() {
        let none = settings(TmuxServerChoice::Own, TmuxConfig::None);
        assert_eq!(choose(None, &none).config_file, Some("/dev/null".into()));
        let file = settings(
            TmuxServerChoice::Own,
            TmuxConfig::Path("/etc/t.conf".into()),
        );
        assert_eq!(choose(None, &file).config_file, Some("/etc/t.conf".into()));
        let other = settings(TmuxServerChoice::Name("x".into()), TmuxConfig::None);
        assert_eq!(choose(None, &other).config_file, None);
    }
}
