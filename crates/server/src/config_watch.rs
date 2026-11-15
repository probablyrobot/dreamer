//! Live configuration: the daemon watches `dreamer.yaml` and the theme
//! files beside it, and applies a valid change at once. A new terminal
//! theme or cursor default goes to every live mirror, and every view takes
//! a checkpoint with the new colors. A file that does not parse changes
//! nothing: the settings in force stay, and the reason is logged and
//! published (`config.changed`, state `refused`).

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

fn same(a: &mirror::TerminalDefaults, b: &mirror::TerminalDefaults) -> bool {
    a.scheme == b.scheme && a.cursor == b.cursor && a.blink == b.blink
}

/// Watch the configuration directory for the daemon's life.
pub async fn run(state: Arc<State>) {
    let Some(dir) = workspace_config::config_dir() else {
        return;
    };
    let mut seen = workspace_config::check::stamp(&dir);
    loop {
        tokio::time::sleep(EVERY).await;
        let now = workspace_config::check::stamp(&dir);
        if now == seen {
            continue;
        }
        seen = now;
        apply(&state).await;
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
