//! `dream web install [ARCHIVE | DIR | URL]`, `dream web status`, and
//! `dream web remove`: Dreamer Web, the Chromium component that web panes
//! need, which is installed apart from the app (see the `web-component`
//! crate, whose installer the desktop shares).
//!
//! Without an argument, an install first looks for a copy already on the
//! system and uses it in place; otherwise it downloads this version's
//! archive (from `DREAMER_WEB_URL`, `web.url` in `dreamer.yaml`, or the
//! release), and its checksum beside it.

use std::io::{IsTerminal as _, Write as _};

use web_component::install::{Installer, Origin, Source, Step, describe, remove as remove_at};
use web_component::{NAME, State, check, installed_dir};

pub const USAGE: &str =
    "usage: dream web install [ARCHIVE | DIR | URL] | dream web status | dream web remove";

/// Whether the component is installed and is this Dreamer's.
pub fn status() -> Result<(), String> {
    let dir = installed_dir();
    let state = check(&dir);
    match (state.problem(&dir), state) {
        (None, State::Ready(m)) => {
            println!("{}", describe(&dir, &m));
            Ok(())
        }
        (Some(problem), _) => Err(problem),
        (None, _) => Ok(()),
    }
}

/// Remove the installed component (or the link to a copy used in place,
/// leaving the copy). Web panes then offer to add it again.
pub fn remove() -> Result<(), String> {
    let dir = installed_dir();
    match remove_at(&dir)? {
        None => println!("{NAME} is not installed"),
        Some(Origin::InPlace(to)) => println!(
            "{NAME} at {} is no longer used (it was left there)",
            to.display()
        ),
        Some(_) => println!("{NAME} removed from {}", dir.display()),
    }
    Ok(())
}

/// Install the component from `from` (an archive, an unpacked component
/// directory, or an address), or from a copy on the system or the
/// download address.
pub fn install(from: Option<&str>) -> Result<(), String> {
    let installer = Installer::for_this_build()?;
    let target = installer.want.target.clone();
    let source = match from {
        Some(arg) => Source::from_arg(arg, &target),
        None => {
            let setting = workspace_config::load_settings()
                .ok()
                .and_then(|s| s.web.url);
            Source::Find(web_component::download_url(setting.as_deref(), &target))
        }
    };
    let tty = std::io::stderr().is_terminal();
    let mut last = String::new();
    let mut progress = |step: Step| {
        let line = step.to_string();
        if line == last {
            return;
        }
        // On a terminal the download's line counts up in place; otherwise
        // it is printed once.
        let downloading = matches!(step, Step::Downloading { .. });
        let was = last.starts_with("Downloading");
        if downloading && tty {
            eprint!("\r{line}\x1b[K");
        } else if !(downloading && was) {
            if was && tty {
                eprintln!();
            }
            eprintln!("{line}");
        }
        let _ = std::io::stderr().flush();
        last = line;
    };
    if let Source::Download(url) | Source::Find(url) = &source {
        eprintln!("{NAME} {}: from {url}", installer.want.version);
    }
    let done = installer.install(&source, &mut progress);
    if tty && last.starts_with("Downloading") {
        eprintln!();
    }
    let done = done?;
    match &done.origin {
        Origin::AlreadyInstalled => println!(
            "{} (already installed)",
            describe(&done.dir, &done.manifest)
        ),
        Origin::InPlace(_) => {
            println!("{}", describe(&done.dir, &done.manifest));
            println!("Web panes opened from now on use it.");
        }
        _ => {
            println!(
                "{NAME} {} (Chromium {}) installed in {}",
                done.manifest.version,
                done.manifest.chromium(),
                done.dir.display()
            );
            println!("Web panes opened from now on use it.");
        }
    }
    Ok(())
}
