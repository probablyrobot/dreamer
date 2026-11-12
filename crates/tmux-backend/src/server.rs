//! Isolated tmux servers for tests and fixtures.
//!
//! Every server uses its own `-L` socket name and `-f /dev/null`, runs with a
//! minimal environment, and is killed on drop. Nothing here can reach the
//! developer's own tmux server or configuration.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// The tmux executable: `DREAMER_TMUX`, or `tmux` on `PATH`.
pub fn tmux_program() -> PathBuf {
    std::env::var_os("DREAMER_TMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tmux"))
}

#[derive(Debug)]
pub struct TmuxServer {
    socket: String,
}

impl TmuxServer {
    /// Start a server with one detached session `main` running `command`
    /// in a `cols` x `rows` window with manual sizing.
    pub fn start(cols: u16, rows: u16, command: &str) -> io::Result<Self> {
        let socket = format!(
            "dreamer-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let server = Self { socket };
        server.run_checked(&[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
            command,
        ])?;
        // Keep the window at the requested size regardless of which clients
        // attach. This is an isolated server; no user option is touched.
        server.run_checked(&["set-option", "-g", "window-size", "manual"])?;
        server.run_checked(&["set-option", "-g", "history-limit", "50000"])?;
        Ok(server)
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// A `tmux` command preconfigured for this server.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(tmux_program());
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .env("HOME", std::env::temp_dir())
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .args(["-L", &self.socket, "-f", "/dev/null"]);
        cmd
    }

    /// Run a one-shot tmux command and return stdout.
    pub fn run(&self, args: &[&str]) -> io::Result<Output> {
        self.command().args(args).stdin(Stdio::null()).output()
    }

    pub fn run_checked(&self, args: &[&str]) -> io::Result<String> {
        let out = self.run(args)?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "tmux {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `display-message -p` for a target, trimmed.
    pub fn format(&self, target: &str, format: &str) -> io::Result<String> {
        Ok(self
            .run_checked(&["display-message", "-p", "-t", target, format])?
            .trim_end()
            .to_owned())
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}
