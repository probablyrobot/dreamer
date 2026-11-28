//! Live configuration: the daemon watches `dreamer.yaml` and the theme
//! files beside it, and applies a valid change at once. A new terminal
//! theme or cursor default goes to every live mirror, and every view takes
//! a checkpoint with the new colors. A file that does not parse changes
//! nothing: the settings in force stay, and the reason is logged and
//! published (`config.changed`, state `refused`).
//!
//! Installed apps are unpacked from their packages (`packages/`): on
//! start, and whenever a package changes (a restore, a pull), `dream app
//! sync` unpacks each app whose folder is missing or differs from it.

use std::sync::Arc;
use std::time::Duration;

use crate::log;
use crate::service::State;

/// How often the files are looked at. Cheap: a few `stat` calls.
const EVERY: Duration = Duration::from_secs(1);

/// The terminal defaults `settings` give.
pub fn defaults(settings: &workspace_config::Settings) -> mirror::TerminalDefaults {
    mirror::TerminalDefaults {
        scheme: settings.scheme(),
        cursor: settings.terminal.cursor_style.shape(),
        blink: settings.terminal.cursor_blink,
    }
}

/// The tier policy `settings` give.
pub fn tiers(t: &workspace_config::TierSettings) -> mirror::TierPolicy {
    mirror::TierPolicy {
        cold_after: t.cold_after,
        warm_limit: t.warm_limit,
        warm_memory: t.warm_memory,
    }
}

fn same(a: &mirror::TerminalDefaults, b: &mirror::TerminalDefaults) -> bool {
    a.scheme == b.scheme && a.cursor == b.cursor && a.blink == b.blink
}

/// Watch the configuration directory for the daemon's life.
pub async fn run(state: Arc<State>) {
    let Some(dir) = workspace_config::config_dir() else {
        return;
    };
    let mut seen = workspace_config::check::stamp(&dir);
    let mut packages = stamp_packages(&dir);
    sync_apps().await;
    loop {
        tokio::time::sleep(EVERY).await;
        let now = stamp_packages(&dir);
        if now != packages {
            packages = now;
            sync_apps().await;
        }
        let now = workspace_config::check::stamp(&dir);
        if now == seen {
            continue;
        }
        seen = now;
        apply(&state).await;
    }
}

/// The installed packages: names, sizes and times.
fn stamp_packages(
    dir: &std::path::Path,
) -> Vec<(std::ffi::OsString, u64, Option<std::time::SystemTime>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir.join("packages"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| {
            let m = e.metadata().ok();
            (
                e.file_name(),
                m.as_ref().map_or(0, std::fs::Metadata::len),
                m.and_then(|m| m.modified().ok()),
            )
        })
        .collect();
    out.sort();
    out
}

/// Run `dream app sync` (beside this daemon, or `DREAMER_DREAM`) and log
/// what it did.
async fn sync_apps() {
    // A test daemon that was not given its own configuration and data
    // directories never writes to the user's.
    let private = |v: &str| std::env::var_os(v).is_some();
    if private("DREAMER_FORBID_DEFAULT_TMUX")
        && !(private("DREAMER_CONFIG_DIR") && private("DREAMER_DATA_DIR"))
    {
        return;
    }
    let dream = std::env::var_os("DREAMER_DREAM")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| Some(e.parent()?.join("dream")))
        })
        .filter(|p| p.is_file());
    let Some(dream) = dream else {
        log::warn(
            "installed apps not checked against their packages: `dream` is not beside dreamerd",
        );
        return;
    };
    let out = tokio::process::Command::new(&dream)
        .args(["app", "sync"])
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match out {
        Ok(o) => {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                log::info(format!("apps: {line}"));
            }
            if !o.status.success() {
                log::warn(format!(
                    "apps: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
        }
        Err(e) => log::warn(format!("apps: {}: {e}", dream.display())),
    }
}

/// Read the configuration again and apply what changed; keep what is in
/// force if it does not read.
async fn apply(state: &State) {
    let settings = match workspace_config::load_settings() {
        Ok(s) => s,
        Err(e) => {
            log::warn(format!(
                "configuration not applied, the settings in force are kept: {e}"
            ));
            state.events.config(false, &e.to_string());
            return;
        }
    };
    state.memory.configure(&settings.memory);
    let policy = tiers(&settings.tiers);
    {
        let mut current = state.tiers.lock().await;
        if *current != policy {
            *current = policy;
            log::info(format!(
                "configuration applied: tiers {}",
                crate::tiers::describe(&policy)
            ));
        }
    }
    let frames = crate::feeds::Policy::of(&settings.render);
    if let Ok(mut current) = state.frames.write()
        && *current != frames
    {
        *current = frames;
        log::info(format!(
            "configuration applied: frames above {} B/s, below {} B/s, dwell {:?}, hold {:?}",
            frames.above, frames.below, frames.dwell, frames.hold
        ));
    }
    let new = defaults(&settings);
    let changed = {
        let mut current = state.terminal.lock().await;
        let changed = !same(&current, &new);
        *current = new;
        changed
    };
    if changed {
        let host = state.mirrors.read().await.clone();
        if let Some(host) = host {
            host.set_terminal_defaults(new).await;
        }
        log::info(format!(
            "configuration applied: terminal theme {}",
            new.scheme.name
        ));
    }
    state.events.config(true, new.scheme.name);
}
