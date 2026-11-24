//! `dream update`: move this installation to the latest release.
//!
//! The release's own `install.sh` does the work, so an update is the same
//! steps as a first install: download, check the SHA-256, switch over. This
//! command finds the latest release, works out how Dreamer was installed
//! (so the installer writes to the same place), and runs it. An
//! installation that a system package manager owns is left to it.
//!
//! Like the rest of Dreamer, it opens no network sockets itself; it runs
//! `curl`.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const USAGE: &str = "usage:
  dream update              install the latest release over this one
  dream update --check      say whether a newer release exists
  dream update --version V  install release V (an older one too)";

/// Where releases are. `DREAMER_RELEASES_URL` names another place that
/// serves the same paths (a mirror, or a test).
fn releases() -> String {
    std::env::var("DREAMER_RELEASES_URL")
        .unwrap_or_else(|_| "https://github.com/probablyrobot/dreamer/releases".into())
}

/// How this `dream` was installed, from where it runs.
#[derive(Debug, PartialEq, Eq)]
enum Installed {
    /// By `install.sh` on Linux, under this prefix.
    Prefix(PathBuf),
    /// As `Dreamer.app` on macOS, in this directory.
    App(PathBuf),
    /// Somewhere else: a system package, or a build from source.
    Other(PathBuf),
}

fn installed(exe: &Path) -> Installed {
    let up = |n: usize| exe.ancestors().nth(n);
    let named = |n: usize, name: &str| {
        up(n).and_then(Path::file_name).and_then(|f| f.to_str()) == Some(name)
    };
    // PREFIX/share/dreamer/versions/<version>/bin/dream
    if named(1, "bin")
        && named(3, "versions")
        && named(4, "dreamer")
        && named(5, "share")
        && let Some(prefix) = up(6)
    {
        return Installed::Prefix(prefix.to_owned());
    }
    // DIR/Dreamer.app/Contents/MacOS/dream
    if named(1, "MacOS")
        && named(2, "Contents")
        && let (Some(app), Some(dir)) = (up(3), up(4))
        && app.extension().is_some_and(|e| e == "app")
    {
        return Installed::App(dir.to_owned());
    }
    Installed::Other(exe.to_owned())
}

/// `1.2.3` (or `v1.2.3`) as numbers; anything else is `None`.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let v = (next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

/// The latest release's version: where `releases/latest` redirects to.
fn latest() -> Result<String, String> {
    let url = format!("{}/latest", releases());
    let out = Command::new("curl")
        .args(["-fsSLI", "--retry", "2", "--connect-timeout", "20"])
        .args(["-o", "/dev/null", "-w", "%{url_effective}"])
        .arg(&url)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    let landed = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let tag = landed.rsplit_once("/tag/").map(|(_, t)| t.to_owned());
    match tag {
        Some(t) if out.status.success() && parse(&t).is_some() => {
            Ok(t.trim_start_matches('v').to_owned())
        }
        _ => Err(format!(
            "could not find the latest release at {url} ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

pub fn run(args: &[&str]) -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    let (check, wanted) = match args {
        [] => (false, None),
        ["--check"] => (true, None),
        ["--version", v] => (false, Some(*v)),
        _ => return Err(USAGE.into()),
    };
    let target = match wanted {
        Some(v) => {
            parse(v).ok_or_else(|| format!("`{v}` is not a version like 1.2.3"))?;
            v.trim_start_matches('v').to_owned()
        }
        None => latest()?,
    };
    let newer = parse(&target) > parse(current);
    if check {
        if newer {
            println!("Dreamer {target} is available (this is {current}). Run `dream update`.");
        } else {
            println!("Dreamer {current} is the latest release.");
        }
        return Ok(());
    }
    if wanted.is_none() && !newer {
        println!("Dreamer {current} is the latest release.");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("cannot tell where dream is installed: {e}"))?;
    let mut install = Command::new("sh");
    let script = std::env::temp_dir().join(format!("dreamer-install-{}.sh", std::process::id()));
    install.arg(&script).args(["--version", &target]);
    match installed(&exe) {
        Installed::Prefix(prefix) => install.arg("--prefix").arg(prefix),
        Installed::App(dir) => install.arg("--app-dir").arg(dir),
        Installed::Other(exe) => {
            return Err(format!(
                "this Dreamer ({}) was not installed by install.sh, so `dream update` leaves it \
                 alone. If a package manager installed it, get the {target} package from \
                 {}/tag/v{target} and install it the same way (`apt install ./FILE.deb`, \
                 `dnf install ./FILE.rpm`, `pacman -U FILE`).",
                exe.display(),
                releases()
            ));
        }
    };

    let url = format!("{}/download/v{target}/install.sh", releases());
    println!("dream: updating {current} to {target}");
    let fetched = Command::new("curl")
        .args(["-fsSL", "--retry", "2", "--connect-timeout", "20", "-o"])
        .arg(&script)
        .arg(&url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !fetched.success() {
        return Err(format!("could not download {url}"));
    }
    let status = install.status();
    let _ = std::fs::remove_file(&script);
    match status {
        Ok(s) if s.success() => {
            println!(
                "dream: restart Dreamer to use {target} (`dream shutdown` stops the daemon; \
                 your tmux sessions keep running)."
            );
            Ok(())
        }
        Ok(s) => Err(format!(
            "the installer failed ({s}); {current} is still installed"
        )),
        Err(e) => Err(format!("could not run the installer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_is_recognized_from_where_dream_runs() {
        assert_eq!(
            installed(Path::new(
                "/home/u/.local/share/dreamer/versions/0.9.0/bin/dream"
            )),
            Installed::Prefix("/home/u/.local".into())
        );
        assert_eq!(
            installed(Path::new("/Applications/Dreamer.app/Contents/MacOS/dream")),
            Installed::App("/Applications".into())
        );
        // A system package and a source build are not ours to replace.
        for other in ["/usr/bin/dream", "/src/dreamer/target/debug/dream"] {
            assert_eq!(
                installed(Path::new(other)),
                Installed::Other(other.into()),
                "{other}"
            );
        }
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(parse("0.10.0") > parse("0.9.3"));
        assert_eq!(parse("v1.2.3"), Some((1, 2, 3)));
        for bad in ["1.2", "1.2.3.4", "1.2.x", "latest", ""] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
