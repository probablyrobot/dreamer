//! `dream service install|status|uninstall`: an optional per-user service
//! that keeps `dreamerd` running. Without it, the desktop and
//! `dream` start the daemon on demand, and a daemon started through SSH
//! already outlives the SSH session. A service also survives logout where
//! the system ends a user's processes then, and starts at login.
//!
//! - Linux: a systemd user unit, `~/.config/systemd/user/dreamerd.service`.
//! - macOS: a launch agent, `~/Library/LaunchAgents/dev.dreamer.dreamerd.plist`.
//!
//! Exactly one file is written and removed. The commands that load it are
//! printed before they run, and `--no-start` writes the file only.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "dev.dreamer.dreamerd";

/// Where the definition goes for this platform, under `home`.
pub fn definition_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    } else {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        config.join("systemd/user/dreamerd.service")
    }
}

/// The systemd user unit running `daemon` with `args` in the foreground.
pub fn systemd_unit(daemon: &Path, args: &[String]) -> String {
    let mut exec = vec![systemd_word(&daemon.display().to_string())];
    exec.extend(args.iter().map(|a| systemd_word(a)));
    format!(
        "# Written by `dream service install`; removed by `dream service uninstall`.\n\
         # Stopping it never stops tmux: sessions and panes keep running.\n\
         [Unit]\n\
         Description=Dreamer daemon (tmux mirrors and typed automation)\n\
         \n\
         [Service]\n\
         # Foreground: logs go to the journal.\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" ")
    )
}

/// One word of an `ExecStart=` line: quoted when it has spaces, quotes,
/// or backslashes; `%` and `$` doubled so systemd takes them literally.
fn systemd_word(w: &str) -> String {
    let w = w.replace('%', "%%").replace('$', "$$");
    if w.is_empty()
        || w.chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\' || c == '\'')
    {
        format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        w
    }
}

/// The launch agent running `daemon` with `args` in the foreground.
pub fn launch_agent(daemon: &Path, args: &[String], log: &Path) -> String {
    let xml = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let program: String = std::iter::once(daemon.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| format!("    <string>{}</string>\n", xml(&a)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `dream service install`; removed by `dream service uninstall`. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n{program}\x20 </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
         \x20 <key>ProcessType</key><string>Background</string>\n\
         \x20 <key>StandardErrorPath</key><string>{}</string>\n\
         </dict>\n\
         </plist>\n",
        xml(&log.display().to_string())
    )
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| "HOME is not set".into())
}

/// Print a command, then run it.
fn run(argv: &[String]) -> Result<(), String> {
    println!("$ {}", argv.join(" "));
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} failed ({status})", argv[0]))
    }
}

fn uid() -> String {
    platform::uid().to_string()
}

pub fn install(daemon_args: &[String], start: bool) -> Result<(), String> {
    let home = home()?;
    let daemon = client::daemon_executable();
    let daemon = daemon.canonicalize().unwrap_or(daemon);
    let path = definition_path(&home);
    let text = if cfg!(target_os = "macos") {
        launch_agent(
            &daemon,
            daemon_args,
            &home.join("Library/Logs/dreamer/dreamerd.log"),
        )
    } else {
        systemd_unit(&daemon, daemon_args)
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if cfg!(target_os = "macos") {
        let logs = home.join("Library/Logs/dreamer");
        std::fs::create_dir_all(&logs).map_err(|e| format!("{}: {e}", logs.display()))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    if !start {
        return Ok(());
    }
    // A daemon started on demand holds the startup lock; stop it so the
    // service's own can start. tmux keeps running.
    let _ = crate::daemon::shutdown_quietly();
    if cfg!(target_os = "macos") {
        run(&[
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{}", uid()),
            path.display().to_string(),
        ])
    } else {
        if Command::new("systemctl").arg("--version").output().is_err() {
            return Err(
                "systemctl is not available: this system has no systemd user services; \
                 the daemon still starts on demand"
                    .into(),
            );
        }
        run(&["systemctl".into(), "--user".into(), "daemon-reload".into()])?;
        run(&[
            "systemctl".into(),
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "dreamerd.service".into(),
        ])
    }
}

pub fn uninstall(stop: bool) -> Result<(), String> {
    let home = home()?;
    let path = definition_path(&home);
    if !path.exists() {
        println!("no service installed ({} does not exist)", path.display());
        return Ok(());
    }
    if stop {
        let r = if cfg!(target_os = "macos") {
            run(&[
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{}/{LABEL}", uid()),
            ])
        } else {
            run(&[
                "systemctl".into(),
                "--user".into(),
                "disable".into(),
                "--now".into(),
                "dreamerd.service".into(),
            ])
        };
        if let Err(e) = r {
            println!("({e}; removing the definition anyway)");
        }
    }
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("removed {}", path.display());
    if stop && !cfg!(target_os = "macos") {
        let _ = run(&["systemctl".into(), "--user".into(), "daemon-reload".into()]);
    }
    Ok(())
}

pub fn status() -> Result<(), String> {
    let home = home()?;
    let path = definition_path(&home);
    if !path.exists() {
        println!(
            "not installed: the daemon starts on demand ({} does not exist)",
            path.display()
        );
        return Ok(());
    }
    println!("installed: {}", path.display());
    let (cmd, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        (
            "launchctl",
            vec!["print".into(), format!("gui/{}/{LABEL}", uid())],
        )
    } else {
        (
            "systemctl",
            vec![
                "--user".into(),
                "is-active".into(),
                "dreamerd.service".into(),
            ],
        )
    };
    match Command::new(cmd).args(&args).output() {
        Ok(o) if o.status.success() => println!("loaded and running"),
        Ok(_) => println!("not running (the service manager does not have it loaded)"),
        Err(e) => println!("cannot ask {cmd}: {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_start_words_are_quoted_and_specifiers_kept_literal() {
        let unit = systemd_unit(
            Path::new("/home/me/My Apps/dreamerd"),
            &["--tmux-socket".into(), "100%$x".into()],
        );
        assert!(
            unit.contains("ExecStart=\"/home/me/My Apps/dreamerd\" --tmux-socket 100%%$$x\n"),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn the_launch_agent_escapes_its_strings() {
        let plist = launch_agent(
            Path::new("/Users/me/A&B/dreamerd"),
            &["--tmux".into(), "/opt/<x>/tmux".into()],
            Path::new("/Users/me/Library/Logs/dreamer/dreamerd.log"),
        );
        assert!(
            plist.contains("<string>/Users/me/A&amp;B/dreamerd</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/opt/&lt;x&gt;/tmux</string>"),
            "{plist}"
        );
        assert!(plist.contains(&format!("<key>Label</key><string>{LABEL}</string>")));
    }
}
