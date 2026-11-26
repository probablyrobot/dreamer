//! The harness drawer: one tmux pane per session running the configured
//! agent harness, docked into the window being looked at and hidden again
//! in a holding session (`docs/desktop.md`, "The harness drawer").
//!
//! Everything is tmux: showing is `join-pane`, hiding is `break-pane` into
//! the holding session, and which pane is a session's drawer is a pane user
//! option, so other tmux clients see the same layout and a restarted
//! daemon finds every drawer again.

use std::sync::Arc;

use api::{DrawerWant, RpcError, code};
use domain::{TmuxPaneId, TmuxSessionId, TmuxWindowId};
use tmux_backend::Inventory;
use tmux_backend::ops::{self, Launch, Side, Size};
use workspace_config::{DrawerSide, Settings};

use crate::log;
use crate::service::State;

/// The other panes keep at least this many rows (bottom) or columns
/// (sides); with less, the drawer is shown zoomed instead.
const MIN_ROWS: u32 = 8;
const MIN_COLS: u32 = 20;

fn unavailable(msg: String) -> RpcError {
    RpcError::new(code::UNAVAILABLE, msg)
}

fn refused(e: &tmux_backend::CommandError) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, ops::failure_message(e))
}

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, msg)
}

/// How a session's harness is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessLaunch {
    pub harness: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Why the harness cannot start; the pane then shows this instead.
    pub problem: Option<String>,
}

/// `name` on the daemon's `PATH`, as an absolute path.
fn which(name: &str) -> Option<String> {
    if name.contains('/') {
        return std::path::Path::new(name)
            .is_file()
            .then(|| name.to_owned());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

/// `program`, run with no API-key variable in its environment.
fn without_api_keys(program: String) -> Vec<String> {
    let mut argv = vec!["env".to_owned()];
    for v in providers::session::SCRUBBED {
        argv.extend(["-u".to_owned(), v.to_owned()]);
    }
    argv.push(program);
    argv
}

/// A pane that says why the harness did not start, until Enter.
fn message_argv(message: &str) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '%s\\n\\n' \"$1\"; printf 'Press Enter to close the drawer.'; read -r _".into(),
        "dreamer-drawer".into(),
        message.to_owned(),
    ]
}

/// The harness command for a new drawer of `session`, from the settings:
/// `agents.default`, its `command`, and for pi the managed install and
/// Dreamer's pi profile. The environment lets the harness's own tools
/// reach this daemon.
pub fn harness_launch(settings: &Settings, session: TmuxSessionId) -> HarnessLaunch {
    let name = settings.agents.default_harness().to_owned();
    let block = settings.agents.harness(&name).unwrap_or_default();
    // A configured harness without a command runs the program its protocol
    // names (a second Claude Code configuration runs `claude`).
    let program = settings
        .agents
        .protocol(&name)
        .map_or(name.as_str(), |p| p.name())
        .to_owned();
    let mut env = vec![
        (
            "DREAMER_RUNTIME_DIR".to_owned(),
            platform::runtime_dir().display().to_string(),
        ),
        (
            "DREAMER_STATE_DIR".to_owned(),
            platform::state_dir().display().to_string(),
        ),
        (
            "DREAMER_DATA_DIR".to_owned(),
            platform::data_dir().display().to_string(),
        ),
        ("DREAMER_DRAWER".to_owned(), format!("${}", session.0)),
    ];
    if let Some(dir) = workspace_config::config_dir() {
        env.push(("DREAMER_CONFIG_DIR".to_owned(), dir.display().to_string()));
    }
    let mut problem = None;
    let argv = if let Some(c) = &block.command {
        // The user's own command, run exactly as given.
        c.clone()
    } else if program == "pi" {
        let install = providers::pi::Install::default();
        // pi starts only through `dream agents pi`: it checks Dreamer's
        // profile (restoring the last good one if a change broke loading),
        // shows the login step when pi has no login (so no turn starts on
        // a provider pi picks itself), pins the provider, removes API-key
        // variables, then runs pi.
        let dream = std::env::var_os("DREAMER_DREAM")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| Some(e.parent()?.join("dream")))
            })
            .filter(|p| p.is_file());
        let have_pi = install.current().is_some() || which("pi").is_some();
        match (dream, have_pi) {
            (Some(dream), true) => {
                vec![dream.display().to_string(), "agents".into(), "pi".into()]
            }
            (_, false) => {
                problem = Some(
                    "pi is not installed. Run `dream agents update` to install it, then open the drawer again."
                        .to_owned(),
                );
                Vec::new()
            }
            (None, true) => {
                problem = Some(
                    "`dream` was not found beside dreamerd (or in DREAMER_DREAM); Dreamer starts pi only through `dream agents pi`, which checks pi's login first."
                        .to_owned(),
                );
                Vec::new()
            }
        }
    } else if let Some(p) = which(&program) {
        // Claude Code and Codex run on their own login. An API key in the
        // pane's environment would make them bill that account instead,
        // so the drawer starts them without one. tmux gives the pane its
        // environment, so the variables are removed in the command itself.
        without_api_keys(p)
    } else {
        problem = Some(format!(
            "`{program}` is not on PATH. Install it, or set agents.{name}.command in dreamer.yaml."
        ));
        Vec::new()
    };
    if program == "pi" {
        let dir = block
            .agent_dir
            .clone()
            .unwrap_or_else(providers::pi::profile_dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn(format!("drawer: cannot create {}: {e}", dir.display()));
        }
        env.push(("PI_CODING_AGENT_DIR".to_owned(), dir.display().to_string()));
    }
    let argv = match &problem {
        Some(p) => message_argv(p),
        None => argv,
    };
    HarnessLaunch {
        harness: name,
        argv,
        env,
        problem,
    }
}

/// Where a session's drawer is, relative to the window asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Place {
    /// In the window asked about.
    here: bool,
    /// In a window of its session (not the holding session).
    shown: bool,
    /// The active pane of its window, and that the session's active window.
    focused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Create,
    Show,
    Focus,
    Hide,
    Replace,
    Nothing,
}

fn decide(want: DrawerWant, place: Option<Place>) -> Step {
    match (want, place) {
        (DrawerWant::New, _) => Step::Replace,
        (DrawerWant::Hide, Some(p)) if p.shown => Step::Hide,
        (DrawerWant::Hide, _) => Step::Nothing,
        (_, None) => Step::Create,
        (_, Some(p)) if !p.here => Step::Show,
        (DrawerWant::Show, Some(_)) => Step::Focus,
        (_, Some(p)) if p.focused => Step::Hide,
        (_, Some(_)) => Step::Focus,
    }
}

fn holding(inv: &Inventory) -> Option<TmuxSessionId> {
    inv.sessions
        .iter()
        .find(|(_, s)| s.name == ops::DRAWER_HOLDING)
        .map(|(id, _)| *id)
}

/// The session's drawer pane, if it has a live one.
fn drawer_of(inv: &Inventory, session: TmuxSessionId) -> Option<TmuxPaneId> {
    inv.panes
        .iter()
        .find(|(_, p)| p.drawer_for == Some(session) && !p.dead)
        .map(|(id, _)| *id)
}

fn place(inv: &Inventory, session: TmuxSessionId, window: TmuxWindowId) -> Option<Place> {
    let pane = drawer_of(inv, session)?;
    let w = inv.panes.get(&pane)?.window;
    let info = inv.windows.get(&w)?;
    let shown = info.sessions.contains(&session);
    let active_window = inv.sessions.get(&session).and_then(|s| s.active_window);
    Some(Place {
        here: w == window,
        shown,
        focused: shown && info.active_pane == Some(pane) && active_window == Some(w),
    })
}

/// The session and window a request is about.
fn resolve(
    inv: &Inventory,
    p: &api::DrawerRequest,
) -> Result<(TmuxSessionId, TmuxWindowId), RpcError> {
    let hold = holding(inv);
    let session_of = |w: TmuxWindowId| {
        inv.windows
            .get(&w)
            .and_then(|i| i.sessions.iter().find(|s| Some(**s) != hold).copied())
    };
    let mut session = p.session.map(TmuxSessionId);
    let mut window = p.window.map(TmuxWindowId);
    if let Some(pane) = p.pane {
        let info = inv
            .panes
            .get(&TmuxPaneId(pane))
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no pane %{pane}")))?;
        match info.drawer_for {
            Some(s) => {
                session = session.or(Some(s));
                if window.is_none()
                    && inv
                        .windows
                        .get(&info.window)
                        .is_some_and(|w| w.sessions.contains(&s))
                {
                    window = Some(info.window);
                }
            }
            None => {
                window = window.or(Some(info.window));
            }
        }
    }
    if session.is_none() {
        session = window.and_then(session_of);
    }
    let session =
        session.ok_or_else(|| invalid("name a session, a window, or a pane for the drawer"))?;
    let info = inv
        .sessions
        .get(&session)
        .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no session ${}", session.0)))?;
    if Some(session) == hold {
        return Err(invalid("the holding session has no drawer of its own"));
    }
    let window = window
        .or(info.active_window)
        .ok_or_else(|| invalid(format!("session ${} has no window", session.0)))?;
    if !inv
        .windows
        .get(&window)
        .is_some_and(|w| w.sessions.contains(&session))
    {
        return Err(invalid(format!(
            "window @{} is not in session ${}",
            window.0, session.0
        )));
    }
    Ok((session, window))
}

/// Whether the window is too small to dock the drawer beside its other
/// panes.
fn too_small(cols: u32, rows: u32, side: DrawerSide, size: u32) -> bool {
    let (total, least) = match side {
        DrawerSide::Bottom => (rows, MIN_ROWS),
        DrawerSide::Left | DrawerSide::Right => (cols, MIN_COLS),
    };
    let drawer = total * size / 100;
    total.saturating_sub(drawer + 1) < least
}

fn window_size(inv: &Inventory, w: TmuxWindowId) -> (u32, u32) {
    match inv.windows.get(&w).map(|i| &i.layout) {
        Some(domain::WindowLayout::Tiled(node)) => {
            let r = node.rect();
            (u32::from(r.cols), u32::from(r.rows))
        }
        _ => (0, 0),
    }
}

struct Run<'a> {
    ep: &'a mut workspace_host::Endpoint,
    session: TmuxSessionId,
    window: TmuxWindowId,
    settings: Settings,
}

impl Run<'_> {
    async fn refresh(&mut self) -> Result<(), RpcError> {
        self.ep
            .tracker
            .refresh()
            .await
            .map(drop)
            .map_err(|e| unavailable(format!("cannot read tmux: {e}")))
    }

    fn inv(&self) -> &Inventory {
        self.ep.tracker.inventory()
    }

    /// The holding session, created when missing. A new one starts with a
    /// placeholder pane, returned so it can go once a drawer is there.
    async fn holding(&mut self) -> Result<(TmuxSessionId, Option<TmuxPaneId>), RpcError> {
        if let Some(h) = holding(self.inv()) {
            return Ok((h, None));
        }
        let placeholder = Launch {
            argv: vec!["/bin/sh".into(), "-c".into(), "exec sleep 86400".into()],
            background: true,
            ..Launch::default()
        };
        let (h, pane) = self.new_holding(&placeholder).await?;
        Ok((h, Some(pane)))
    }

    async fn new_holding(&mut self, l: &Launch) -> Result<(TmuxSessionId, TmuxPaneId), RpcError> {
        let conn = &self.ep.conn;
        let (h, pane) = ops::new_session(conn, Some(ops::DRAWER_HOLDING), l)
            .await
            .map_err(|e| refused(&e))?;
        ops::keep_unattached(conn, h)
            .await
            .map_err(|e| refused(&e))?;
        Ok((h, pane))
    }

    /// Start the harness in a new pane of the holding session.
    async fn create(&mut self) -> Result<(TmuxPaneId, HarnessLaunch), RpcError> {
        let launch = harness_launch(&self.settings, self.session);
        let cwd = match self
            .inv()
            .windows
            .get(&self.window)
            .and_then(|w| w.active_pane)
        {
            Some(p) => ops::pane_current_path(&self.ep.conn, p)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let l = Launch {
            argv: launch.argv.clone(),
            cwd,
            env: launch.env.clone(),
            background: true,
        };
        let pane = match holding(self.inv()) {
            Some(h) => ops::new_window(&self.ep.conn, h, Some("drawer"), &l)
                .await
                .map_err(|e| refused(&e))?,
            None => self.new_holding(&l).await?.1,
        };
        let conn = &self.ep.conn;
        ops::set_pane_option(conn, pane, ops::DRAWER_FOR, &format!("${}", self.session.0))
            .await
            .map_err(|e| refused(&e))?;
        ops::set_pane_option(conn, pane, ops::DRAWER_HARNESS, &launch.harness)
            .await
            .map_err(|e| refused(&e))?;
        log::info(format!(
            "drawer: started {} in pane %{} for session ${}",
            launch.harness, pane.0, self.session.0
        ));
        self.refresh().await?;
        Ok((pane, launch))
    }

    /// Dock the drawer into the window and focus it.
    async fn show(&mut self, drawer: TmuxPaneId, back: Option<TmuxPaneId>) -> Result<(), RpcError> {
        let inv = self.inv();
        let target = inv
            .windows
            .get(&self.window)
            .and_then(|w| w.active_pane)
            .or_else(|| {
                inv.panes
                    .iter()
                    .find(|(_, p)| p.window == self.window)
                    .map(|(id, _)| *id)
            })
            .ok_or_else(|| invalid(format!("window @{} has no pane", self.window.0)))?;
        let (cols, rows) = window_size(inv, self.window);
        let small = too_small(
            cols,
            rows,
            self.settings.drawer.side,
            self.settings.drawer.size,
        );
        let back = back.unwrap_or(target);
        let conn = &self.ep.conn;
        ops::set_pane_option(conn, drawer, ops::DRAWER_RETURN, &format!("%{}", back.0))
            .await
            .map_err(|e| refused(&e))?;
        let side = match self.settings.drawer.side {
            DrawerSide::Bottom => Side::Bottom,
            DrawerSide::Right => Side::Right,
            DrawerSide::Left => Side::Left,
        };
        let size = (!small).then_some(Size::Percent(self.settings.drawer.size));
        ops::dock_pane(conn, drawer, target, side, size)
            .await
            .map_err(|e| refused(&e))?;
        if small
            && !ops::window_zoomed(conn, self.window)
                .await
                .map_err(|e| refused(&e))?
        {
            ops::toggle_zoom(conn, drawer)
                .await
                .map_err(|e| refused(&e))?;
        }
        self.focus(drawer).await
    }

    async fn focus(&mut self, drawer: TmuxPaneId) -> Result<(), RpcError> {
        let active = self
            .inv()
            .sessions
            .get(&self.session)
            .and_then(|s| s.active_window);
        let conn = &self.ep.conn;
        ops::select_pane(conn, drawer)
            .await
            .map_err(|e| refused(&e))?;
        if active != Some(self.window) {
            ops::select_window(conn, self.window)
                .await
                .map_err(|e| refused(&e))?;
        }
        Ok(())
    }

    /// The pane focused before the drawer was shown, if it is still there.
    async fn back(&self, drawer: TmuxPaneId) -> Option<TmuxPaneId> {
        let text = ops::pane_option(&self.ep.conn, drawer, ops::DRAWER_RETURN)
            .await
            .ok()?;
        let pane = TmuxPaneId(text.trim().strip_prefix('%')?.parse().ok()?);
        self.inv().panes.contains_key(&pane).then_some(pane)
    }

    /// Move the drawer to the holding session; focus goes back to where it
    /// came from when the drawer had it.
    async fn hide(&mut self, drawer: TmuxPaneId) -> Result<(), RpcError> {
        let inv = self.inv();
        let Some(from) = inv.panes.get(&drawer).map(|p| p.window) else {
            return Ok(());
        };
        let had_focus = inv.windows.get(&from).and_then(|w| w.active_pane) == Some(drawer);
        let back = self.back(drawer).await;
        let (h, placeholder) = self.holding().await?;
        let conn = &self.ep.conn;
        ops::break_pane_to(conn, drawer, h, "drawer")
            .await
            .map_err(|e| refused(&e))?;
        if let Some(p) = placeholder {
            let _ = ops::kill_pane(conn, p).await;
        }
        if had_focus
            && let Some(back) = back
            && self
                .inv()
                .panes
                .get(&back)
                .is_some_and(|p| p.window == from)
        {
            let _ = ops::select_pane(conn, back).await;
        }
        // The pane stays recorded: `from_pane` still names it while the
        // drawer is hidden, and the next show records it again.
        Ok(())
    }
}

/// `drawer.toggle` (`act`) and `drawer.status`.
pub async fn run(
    state: &Arc<State>,
    p: api::DrawerRequest,
    act: bool,
) -> Result<api::DrawerStatus, RpcError> {
    let mut guard = state.endpoint().await?;
    let ep = guard
        .as_mut()
        .ok_or_else(|| unavailable("endpoint not connected".into()))?;
    ep.tracker
        .refresh()
        .await
        .map_err(|e| unavailable(format!("cannot read tmux: {e}")))?;
    let (session, window) = resolve(ep.tracker.inventory(), &p)?;
    let settings = if act {
        workspace_config::load_settings().unwrap_or_else(|e| {
            log::warn(format!("drawer: settings not read, using defaults: {e}"));
            Settings::default()
        })
    } else {
        Settings::default()
    };
    let mut run = Run {
        ep,
        session,
        window,
        settings,
    };
    let mut started = None;
    if act {
        let want = DrawerWant::try_from(p.want)
            .ok()
            .filter(|w| *w != DrawerWant::Unspecified)
            .unwrap_or(DrawerWant::Toggle);
        let existing = drawer_of(run.inv(), session);
        let step = decide(want, place(run.inv(), session, window));
        match (step, existing) {
            (Step::Nothing, _) => {}
            (Step::Create, _) | (Step::Replace, None) => {
                let (pane, launch) = run.create().await?;
                run.show(pane, None).await?;
                started = Some(launch);
            }
            (Step::Replace, Some(old)) => {
                let back = run.back(old).await;
                ops::kill_pane(&run.ep.conn, old)
                    .await
                    .map_err(|e| refused(&e))?;
                run.refresh().await?;
                let (pane, launch) = run.create().await?;
                let back =
                    back.filter(|b| run.inv().panes.get(b).is_some_and(|p| p.window == window));
                run.show(pane, back).await?;
                started = Some(launch);
            }
            (Step::Show, Some(d)) => {
                // Leaving another window: focus goes back there first.
                if place(run.inv(), session, window).is_some_and(|p| p.shown) {
                    run.hide(d).await?;
                    run.refresh().await?;
                }
                run.show(d, None).await?;
            }
            (Step::Focus, Some(d)) => {
                // Coming from another pane of the window: that pane is now
                // the one to return to.
                let came_from = run
                    .inv()
                    .windows
                    .get(&window)
                    .and_then(|w| w.active_pane)
                    .filter(|p| *p != d);
                if let Some(p) = came_from {
                    ops::set_pane_option(&run.ep.conn, d, ops::DRAWER_RETURN, &format!("%{}", p.0))
                        .await
                        .map_err(|e| refused(&e))?;
                }
                run.focus(d).await?
            }
            (Step::Hide, Some(d)) => run.hide(d).await?,
            (_, None) => {}
        }
        run.refresh().await?;
        crate::service::publish_topology(state, run.ep);
    }
    let inv = run.inv();
    let pane = drawer_of(inv, session);
    let at = place(inv, session, window);
    let in_window = pane.and_then(|d| inv.panes.get(&d)).map(|p| p.window);
    let zoomed = at.is_some_and(|a| a.shown)
        && in_window
            .and_then(|w| inv.windows.get(&w))
            .is_some_and(|w| w.zoomed && w.active_pane == pane);
    let harness = match (&started, pane) {
        (Some(l), _) => l.harness.clone(),
        (None, Some(d)) => ops::pane_option(&run.ep.conn, d, ops::DRAWER_HARNESS)
            .await
            .unwrap_or_default(),
        (None, None) => String::new(),
    };
    let from_pane = match pane {
        Some(d) => run.back(d).await.map(|p| p.0),
        None => None,
    };
    Ok(api::DrawerStatus {
        session: session.0,
        from_pane,
        pane: pane.map(|d| d.0),
        shown: at.is_some_and(|a| a.shown),
        window: in_window.map(|w| w.0),
        focused: at.is_some_and(|a| a.focused),
        zoomed,
        created: started.is_some(),
        harness,
        command: started
            .as_ref()
            .filter(|l| l.problem.is_none())
            .map(|l| l.argv.clone())
            .unwrap_or_default(),
        problem: started.and_then(|l| l.problem),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(here: bool, shown: bool, focused: bool) -> Option<Place> {
        Some(Place {
            here,
            shown,
            focused,
        })
    }

    #[test]
    fn one_key_shows_focuses_and_hides() {
        let t = DrawerWant::Toggle;
        assert_eq!(decide(t, None), Step::Create);
        assert_eq!(decide(t, at(false, false, false)), Step::Show);
        // Shown in another window of the session: it comes here.
        assert_eq!(decide(t, at(false, true, true)), Step::Show);
        assert_eq!(decide(t, at(true, true, false)), Step::Focus);
        assert_eq!(decide(t, at(true, true, true)), Step::Hide);
        assert_eq!(decide(DrawerWant::Show, at(true, true, true)), Step::Focus);
        assert_eq!(
            decide(DrawerWant::Hide, at(false, false, false)),
            Step::Nothing
        );
        assert_eq!(decide(DrawerWant::Hide, None), Step::Nothing);
        assert_eq!(decide(DrawerWant::New, at(true, true, true)), Step::Replace);
    }

    #[test]
    fn a_window_without_room_for_the_others_gets_the_drawer_zoomed() {
        assert!(!too_small(100, 30, DrawerSide::Bottom, 35));
        assert!(too_small(100, 12, DrawerSide::Bottom, 35));
        assert!(!too_small(100, 12, DrawerSide::Right, 35));
        assert!(too_small(28, 40, DrawerSide::Left, 35));
    }

    #[test]
    fn a_harness_found_on_path_starts_without_api_keys() {
        let argv = without_api_keys("/usr/bin/claude".into());
        assert_eq!(argv.first().map(String::as_str), Some("env"));
        assert_eq!(argv.last().map(String::as_str), Some("/usr/bin/claude"));
        for key in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
            assert!(
                argv.windows(2).any(|w| w[0] == "-u" && w[1] == key),
                "{key} is not removed: {argv:?}"
            );
        }
        // The program runs the same way with the keys gone.
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..argv.len() - 1])
            .arg("/usr/bin/true")
            .env("ANTHROPIC_API_KEY", "sk-test")
            .status()
            .expect("env");
        assert!(out.success());
    }

    #[test]
    fn the_configured_command_runs_with_dreamers_environment() {
        let s = workspace_config::settings::parse_settings(
            "agents:\n  default: claude\n  claude:\n    command: [/bin/echo, hi]\n",
            None,
        )
        .expect("settings");
        let l = harness_launch(&s, TmuxSessionId(4));
        assert_eq!(l.harness, "claude");
        assert_eq!(l.argv, ["/bin/echo", "hi"]);
        assert!(l.problem.is_none());
        let env = |k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(env("DREAMER_DRAWER").as_deref(), Some("$4"));
        assert!(env("DREAMER_RUNTIME_DIR").is_some());
        assert!(env("PI_CODING_AGENT_DIR").is_none());
    }

    #[test]
    fn opencode_in_the_drawer_is_its_own_interface_from_path() {
        let s = workspace_config::settings::parse_settings("agents:\n  default: opencode\n", None)
            .expect("settings");
        let l = harness_launch(&s, TmuxSessionId(4));
        assert_eq!(l.harness, "opencode");
        match which("opencode") {
            // The TUI, not its ACP server, without API keys.
            Some(p) => {
                assert_eq!(l.argv, without_api_keys(p));
                assert!(l.problem.is_none());
            }
            None => {
                let why = l.problem.expect("a reason");
                assert!(why.contains("`opencode` is not on PATH"), "{why}");
            }
        }
    }
}
