//! Web panes: a tmux placeholder pane marked with a web page
//! (`Pane.web`), which the desktop fills with the page, drawn by Chromium
//! itself in a native view kept on the pane's rectangle.
//!
//! Chromium's view sits above everything GPUI draws. The workspace places
//! each web pane ([`Place`]): its view follows the pane's rectangle while
//! the pane is drawn, and is hidden (the page kept) while it is not, on
//! another tab or zoomed away. While an overlay of the desktop's (the
//! palette, the context menu) is open, the pane shows the page's last
//! snapshot instead, taken as the overlay opens; the overlay shows once
//! the live page is off the screen ([`decide`]).
//!
//! Input: Chromium's view takes the mouse, the wheel, and trackpad
//! gestures itself, and the keys while the pane has Dreamer's focus; a
//! click in the page gives the pane that focus. Dreamer's own shortcuts
//! come first: AppKit offers every shortcut to the window's own view
//! before the page sees the key, so they work from inside a page, and the
//! edit shortcuts (copy, cut, paste, select all) become Chromium's own
//! edit commands there.
//!
//! A plain web pane is the browser: chrome on demand, in a strip over the
//! top of the pane that moves the page down while it shows ([`Bar`]): the
//! address bar (Cmd+L, Enter goes, Esc hides it), find in page (Find), and
//! a passing status line (a refused download). Back, forward, reload, and
//! zoom are keys ([`crate::keymap::BROWSER_SHORTCUTS`]); the mouse's side
//! buttons and a swipe go back and forward in the page itself
//! ([`web_engine::browser`]). There are no tabs, bookmarks, or history to
//! show: the back stack is the page's own.
//!
//! A download the page starts shows a row over the page ([`Download`]):
//! its file name and size, with Save and Cancel. Nothing is written until
//! Save: then the file is reserved in the downloads folder
//! (`web.downloads.folder`, else the platform's; see
//! [`workspace_config::downloads`]) under a unique name, written as
//! `NAME.part` with its progress in the row (Cancel still there), and
//! renamed once complete. A canceled or failed download leaves nothing.
//! The browser always asks; an app's page asks only if the app holds
//! `web.download`, and is otherwise refused with a status line. The files
//! are reserved, renamed, and removed off the UI thread.
//!
//! The page's title, scroll position, and (in the browser) its address are
//! kept in the placeholder (`web.state`), so a page opened again (after the
//! desktop restarts, or once its view was let go) is where it was. The
//! address in tmux changing by anything else (`dream browser --pane`)
//! loads it in the pane ([`WebPane::follow`]).
//!
//! The same view draws a `web` node of an app pane's tree (an app's own
//! page): it is then placed where the node is drawn, opens in the app's
//! own browser profile, may navigate only to the app's origin and the
//! origins the app allows, and its page on the app's origin calls the
//! daemon through the bridge as the app ([`web_engine::site`]). It has no
//! browser chrome, and its title and scroll position are not kept anywhere.
//!
//! A web pane in a remote host's session shows its page on this machine.
//! A page on the host's own loopback (`http://localhost:3000`) is reached
//! through a forward over the host's SSH connection ([`Forward`]): the
//! page loads from the forward's port on this machine's loopback, while
//! the address bar, the placeholder, and what is kept in tmux keep the
//! address as the host means it. The forward is held while the pane's
//! page is, and closes when no web pane uses it.
//!
//! Chromium starts with the first web pane, not with the app, loaded from
//! Dreamer Web, the component installed apart from the app. A build
//! without the `web` feature shows the page's address and why it cannot
//! draw it. One without the component (or with another version's) shows
//! the address and a button that adds it, with its progress, and loads the
//! page once it is there, with no restart ([`crate::webcomp`]); with
//! `web.component: auto` it is added without the click.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    RenderImage, StatefulInteractiveElement as _, Styled, StyledImage as _, Subscription,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use web_engine::Edit;

use crate::theme::c;
use crate::workspace::Workspace;

/// Where the workspace has a web pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// Not in the window: on another tab, or another pane is zoomed.
    Hidden,
    /// Drawn, with nothing of the desktop's over it.
    Shown,
    /// Drawn, under an overlay of the desktop's.
    Covered,
}

/// A covered page's snapshot.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
enum Snapshot {
    None,
    /// Asked of Chromium; the page stays live until it comes.
    Asked,
    /// The image, or `None` when Chromium gave none in time.
    Taken(Option<Arc<RenderImage>>),
}

/// Whether the page's view is on screen, and whether to ask for a snapshot
/// first, for a pane at `place` whose view is `live` (on screen now) and
/// whose snapshot is `asked` for or `taken`. A hidden pane's page is never
/// on screen. A covered page stays live until its snapshot is taken, so
/// the pane never goes blank under the overlay; a page that was not live
/// is not shown for it.
fn decide(place: Place, live: bool, asked: bool, taken: bool) -> (bool, bool) {
    match place {
        Place::Hidden => (false, false),
        Place::Shown => (true, false),
        Place::Covered if !live || taken => (false, false),
        Place::Covered => (true, !asked),
    }
}

/// How long a status line shows.
const STATUS_FOR: Duration = Duration::from_secs(6);

/// The browser's strip over the top of its pane, shown on demand.
#[derive(Debug, Default)]
pub(crate) struct Bar {
    /// The address bar: what is typed, and whether all of it is selected
    /// (the address as it was, which the first key replaces).
    pub(crate) address: Option<(String, bool)>,
    /// Why the address did not load.
    pub(crate) error: Option<String>,
    /// Find in page: what is looked for, and how many matches and which
    /// is current, once Chromium says.
    pub(crate) find: Option<(String, Option<(u32, u32)>)>,
    /// The address bar or the find field has the keys (not the page).
    pub(crate) keys: bool,
    /// A passing line, and when it came.
    pub(crate) status: Option<(String, Instant)>,
    /// Downloads the page started that are not over, oldest first.
    pub(crate) downloads: Vec<Download>,
}

/// A download a page started, as its pane shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Download {
    pub(crate) id: u32,
    /// The file name it is saved under: the one the site suggests, made
    /// safe.
    pub(crate) name: String,
    pub(crate) size: Option<u64>,
    pub(crate) state: Saving,
}

/// Where a download is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Saving {
    /// Waiting for Save or Cancel; nothing is written.
    Asked,
    /// Save chosen: its name in the folder is being reserved.
    Reserving,
    /// Being written to `part`, to be renamed `file` once complete.
    Writing {
        file: PathBuf,
        part: PathBuf,
        received: u64,
    },
    /// Canceled while written: Chromium is stopping it.
    Canceling { part: PathBuf },
}

/// How long a canceled download waits for Chromium to say it ended
/// before its partial file is removed anyway.
const CANCEL_WAIT: Duration = Duration::from_secs(5);

/// A number of bytes as people read it: `12 B`, `3.4 MB`.
fn size_text(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1000.0;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if v < 1000.0 {
            break;
        }
        v /= 1000.0;
        unit = u;
    }
    format!("{v:.1} {unit}")
}

/// What a download's row says.
fn download_text(d: &Download, folder: &str) -> String {
    let size = d
        .size
        .map(|n| format!(" ({})", size_text(n)))
        .unwrap_or_default();
    match &d.state {
        Saving::Asked => format!("Save {}{size} to {folder}?", d.name),
        Saving::Reserving => format!("Saving {}…", d.name),
        Saving::Writing { received, .. } => match d.size {
            Some(total) if total > 0 => format!(
                "Saving {}: {} of {} ({}%)",
                d.name,
                size_text(*received),
                size_text(total),
                (received.saturating_mul(100) / total).min(100)
            ),
            _ => format!("Saving {}: {}", d.name, size_text(*received)),
        },
        Saving::Canceling { .. } => format!("Canceling {}…", d.name),
    }
}

/// A path with the home directory written `~`.
fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What a key typed into the bar asks for.
#[derive(Debug, PartialEq, Eq)]
enum Typed {
    Nothing,
    /// Hide the field; the page takes the keys.
    Close,
    /// Load the address typed.
    Go(String),
    /// Find this, from the start.
    Search(String),
    /// The next match (`true`) or the one before.
    Step(bool),
}

impl Bar {
    fn shown(&self) -> bool {
        self.address.is_some() || self.find.is_some() || self.status.is_some()
    }

    /// The address bar, showing `url`, all of it selected, with the keys.
    fn open_address(&mut self, url: &str) {
        self.address = Some((url.to_owned(), true));
        self.error = None;
        self.keys = true;
    }

    /// Find in page, with the keys, its last query kept.
    fn open_find(&mut self) {
        self.find.get_or_insert_with(|| (String::new(), None));
        self.keys = true;
    }

    /// `text` typed into the field that has the keys.
    fn type_text(&mut self, text: &str) -> Typed {
        if !self.keys {
            return Typed::Nothing;
        }
        if let Some((typed, all)) = &mut self.address {
            if std::mem::take(all) {
                typed.clear();
            }
            typed.push_str(text);
            self.error = None;
            return Typed::Nothing;
        }
        match &mut self.find {
            Some((query, found)) => {
                query.push_str(text);
                *found = None;
                Typed::Search(query.clone())
            }
            None => Typed::Nothing,
        }
    }

    /// A key (not text) in the field that has the keys: Escape, Enter
    /// (with Shift, back through the matches), Backspace.
    fn key(&mut self, key: &str, shift: bool) -> Typed {
        if !self.keys {
            return Typed::Nothing;
        }
        if let Some((typed, all)) = &mut self.address {
            return match key {
                "escape" => {
                    self.address = None;
                    self.error = None;
                    self.keys = false;
                    Typed::Close
                }
                "enter" => Typed::Go(typed.clone()),
                "backspace" => {
                    if std::mem::take(all) {
                        typed.clear();
                    } else {
                        typed.pop();
                    }
                    self.error = None;
                    Typed::Nothing
                }
                _ => Typed::Nothing,
            };
        }
        let Some((query, found)) = &mut self.find else {
            return Typed::Nothing;
        };
        match key {
            "escape" => {
                self.find = None;
                self.keys = false;
                Typed::Close
            }
            "enter" if !query.is_empty() => Typed::Step(!shift),
            "backspace" => {
                query.pop();
                *found = None;
                Typed::Search(query.clone())
            }
            _ => Typed::Nothing,
        }
    }

    /// The passing status line, if one shows.
    fn status_line(&self) -> Option<&str> {
        self.status.as_ref().map(|(s, _)| s.as_str())
    }
}

/// What the find field says after the query.
fn found_text(found: Option<(u32, u32)>, query: &str) -> String {
    match found {
        _ if query.is_empty() => String::new(),
        None => String::new(),
        Some((0, _)) => "No matches".into(),
        Some((n, current)) => format!("{current} of {n}"),
    }
}

/// A page on a remote host's loopback, reached through a forward
/// (`hosts.forward`): the host, the page's port there, and, once the
/// forward is up, the port on this machine's loopback that reaches it.
pub(crate) struct Forward {
    pub(crate) host: String,
    remote: u16,
    local: Option<u16>,
    _held: replica::forward::Held,
}

pub(crate) struct WebPane {
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) pane: u32,
    /// The page's address: for a web pane, the one tmux keeps (what the
    /// page last reported, or what it was told to load).
    pub(crate) url: String,
    pub(crate) focus: FocusHandle,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    workspace: WeakEntity<Workspace>,
    place: Place,
    /// Whether the page's view may be on screen (see [`decide`]). Hiding
    /// takes effect at once; showing, when the pane is next painted.
    live: bool,
    snapshot: Snapshot,
    /// The page's title and scroll position as the placeholder keeps them.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    kept: (String, i32, i32),
    /// Why the page is not drawn.
    pub(crate) problem: Option<String>,
    /// Waiting for Dreamer Web to be added: the pane offers it, and opens
    /// the page once it is there.
    wants_component: bool,
    /// The app whose page this is, for a `web` node of an app pane.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    app: Option<web_engine::AppPage>,
    /// The browser's chrome (a plain web pane only).
    pub(crate) bar: Bar,
    /// For a page on a remote host's loopback: its forward.
    pub(crate) forward: Option<Forward>,
    #[cfg(feature = "web")]
    pub(crate) view: Option<web_engine::WebView>,
    /// Waiting for Chromium to make or load the page's profile before the
    /// page opens.
    #[cfg(feature = "web")]
    profile_wait: bool,
    /// The page has the keys exactly while the pane has Dreamer's focus.
    _focus: [Subscription; 2],
}

impl WebPane {
    /// `kept`: the page's title and scroll position as the placeholder
    /// keeps them; the page opens scrolled there. `app`: the app whose
    /// page it is, for a `web` node in app pane `pane`.
    pub(crate) fn new(
        pane: u32,
        url: String,
        kept: (String, i32, i32),
        app: Option<web_engine::AppPage>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WebPane {
        // Chromium starts with the first web pane, here rather than while
        // the pane paints.
        #[cfg(feature = "web")]
        let (problem, wants_component) = if crate::webcomp::look_now() {
            (engine::ensure(cx).err(), false)
        } else {
            // An agent's or an app's page waits for it as a click would.
            if crate::webcomp::mode() == workspace_config::WebComponent::Auto {
                crate::webcomp::add(cx);
            }
            (None, true)
        };
        #[cfg(not(feature = "web"))]
        let (problem, wants_component) = (
            Some(
                "This build of Dreamer has no web engine (web panes are built on macOS with the `web` feature)."
                    .to_owned(),
            ),
            false,
        );
        // Whatever takes Dreamer's focus from the pane (another pane, the
        // palette, the find bar) takes the keys from the page too.
        let focus = cx.focus_handle();
        let _focus = [
            cx.on_focus(&focus, window, |w, _, _| w.take_focus()),
            cx.on_blur(&focus, window, |w, _, _| w.release_focus()),
        ];
        WebPane {
            pane,
            url,
            focus,
            workspace,
            place: Place::Shown,
            live: true,
            snapshot: Snapshot::None,
            kept,
            problem,
            wants_component,
            app,
            bar: Bar::default(),
            forward: None,
            #[cfg(feature = "web")]
            view: None,
            #[cfg(feature = "web")]
            profile_wait: false,
            _focus,
        }
    }

    /// Reach the page, on port `port` of remote host `host`'s loopback,
    /// through a forward from the daemon at `socket`. The page opens once
    /// the forward is up.
    pub(crate) fn forward_from(
        &mut self,
        host: String,
        port: u16,
        socket: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let (held, rx) = match replica::forward::hold(socket, host.clone(), port) {
            Ok(h) => h,
            Err(e) => {
                self.problem = Some(format!("No forward to port {port} on {host}: {e}"));
                return;
            }
        };
        self.forward = Some(Forward {
            host: host.clone(),
            remote: port,
            local: None,
            _held: held,
        });
        cx.spawn(async move |this, cx| {
            let got = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err("the forward ended".into()));
            let _ = this.update(cx, |w, cx| {
                match got {
                    Ok(local) => {
                        if let Some(f) = &mut w.forward {
                            f.local = Some(local);
                        }
                    }
                    Err(e) => {
                        w.problem = Some(format!("No forward to port {port} on {host}: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The address Chromium loads for `url`: through the forward when it
    /// is the forwarded page's port on the loopback.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn load_url(&self, url: &str) -> String {
        use workspace_config::browser::{loopback_port, on_port};
        match &self.forward {
            Some(Forward {
                remote,
                local: Some(local),
                ..
            }) if loopback_port(url) == Some(*remote) => {
                on_port(url, *local).unwrap_or_else(|| url.to_owned())
            }
            _ => url.to_owned(),
        }
    }

    /// The address as the host means it, for an address Chromium shows:
    /// the forward's port becomes the host's again.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn host_url(&self, url: &str) -> String {
        use workspace_config::browser::{loopback_port, on_port};
        match &self.forward {
            Some(Forward {
                remote,
                local: Some(local),
                ..
            }) if loopback_port(url) == Some(*local) => {
                on_port(url, *remote).unwrap_or_else(|| url.to_owned())
            }
            _ => url.to_owned(),
        }
    }

    /// The pane waits for Dreamer Web to be added, offering it.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn wants_component(&self) -> bool {
        self.wants_component
    }

    /// Why the page is not drawn, if it cannot be now: a problem, or
    /// Dreamer Web not added yet (for agents and smoke runs, which cannot
    /// click the pane's button).
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn not_drawn(&self) -> Option<String> {
        self.problem.clone().or_else(|| {
            self.wants_component.then(|| {
                let dir = web_component::installed_dir();
                web_component::check(&dir)
                    .problem(&dir)
                    .unwrap_or_else(|| "Dreamer Web is being added".into())
            })
        })
    }

    /// Whether this is the browser: a plain web pane, not an app's page.
    fn browser(&self) -> bool {
        self.app.is_none()
    }

    /// The workspace laid the pane out at `place`. Returns whether an
    /// overlay may show over it: false while its live page waits for its
    /// snapshot.
    pub(crate) fn set_place(
        &mut self,
        place: Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        #[cfg(feature = "web")]
        let on_screen = self.view.as_ref().is_some_and(web_engine::WebView::visible);
        #[cfg(not(feature = "web"))]
        let on_screen = false;
        let (asked, taken) = match self.snapshot {
            Snapshot::None => (false, false),
            Snapshot::Asked => (true, false),
            Snapshot::Taken(_) => (true, true),
        };
        let (live, ask) = decide(place, on_screen, asked, taken);
        if place != Place::Covered {
            if let Snapshot::Taken(Some(image)) =
                std::mem::replace(&mut self.snapshot, Snapshot::None)
            {
                let _ = window.drop_image(image);
            }
            self.snapshot = Snapshot::None;
        }
        #[cfg(feature = "web")]
        {
            if ask {
                self.ask_snapshot(cx);
            }
            // Leaving the screen: keep where the page is.
            if self.place == Place::Shown && place != Place::Shown {
                self.keep_state(cx);
            }
            if !live && let Some(v) = &self.view {
                v.set_visible(false);
            }
        }
        #[cfg(not(feature = "web"))]
        let _ = ask;
        if (place, live) != (self.place, self.live) {
            cx.notify();
        }
        self.place = place;
        self.live = live;
        !(live && place == Place::Covered)
    }

    /// Keyboard focus moved to this pane: the page takes the keys, unless
    /// the browser's address bar or find field has them.
    pub(crate) fn take_focus(&self) {
        #[cfg(feature = "web")]
        if !self.bar.keys
            && let Some(v) = &self.view
        {
            v.focus();
        }
    }

    /// Whether the page has the window's keyboard focus now (a click in
    /// it), as opposed to Chromium reporting focus it took by itself.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn page_has_focus(&self) -> bool {
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            return v.has_focus();
        }
        false
    }

    /// Keyboard focus moved elsewhere: the window's own view takes it back.
    pub(crate) fn release_focus(&self) {
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            v.release_focus();
        }
    }

    /// Navigations of an app's page refused so far (smoke evidence).
    #[cfg(feature = "web")]
    pub(crate) fn blocked(&self) -> Vec<String> {
        self.view
            .as_ref()
            .map(web_engine::WebView::blocked)
            .unwrap_or_default()
    }

    /// One of Chromium's edit commands in the page, for the desktop's edit
    /// shortcuts; while the browser's bar has the keys, paste and copy
    /// are its field's.
    fn edit(&mut self, edit: Edit, cx: &mut Context<Self>) {
        if self.bar.keys {
            match edit {
                Edit::Paste => {
                    let text = crate::clip::read(cx).unwrap_or_default();
                    let line: String = text.lines().next().unwrap_or("").trim().to_owned();
                    self.typed(&line, cx);
                }
                Edit::Copy => {
                    if let Some((typed, _)) = &self.bar.address {
                        crate::clip::write(cx, typed.clone());
                    }
                }
                Edit::SelectAll => {
                    if let Some((_, all)) = &mut self.bar.address {
                        *all = true;
                        cx.notify();
                    }
                }
                Edit::Cut => {}
            }
        } else {
            #[cfg(feature = "web")]
            if let Some(v) = &self.view {
                v.edit(edit);
            }
        }
    }

    /// Cmd+L: the address bar, pulled down with the page's address
    /// selected; the page lets go of the keys.
    pub(crate) fn open_address(&mut self, cx: &mut Context<Self>) {
        if !self.browser() {
            return;
        }
        #[cfg(feature = "web")]
        let now = self.view.as_ref().map(web_engine::WebView::url);
        #[cfg(not(feature = "web"))]
        let now: Option<String> = None;
        let url = now
            .filter(|u| !u.is_empty())
            .map_or_else(|| self.url.clone(), |u| self.host_url(&u));
        self.bar.open_address(&url);
        self.release_focus();
        cx.notify();
    }

    /// Find: the find field, with the keys; again, it takes the keys
    /// back from the page.
    fn open_find(&mut self, cx: &mut Context<Self>) {
        if !self.browser() {
            return;
        }
        self.bar.address = None;
        self.bar.error = None;
        self.bar.open_find();
        self.release_focus();
        cx.notify();
    }

    /// Text typed while the bar has the keys (the smoke types an address
    /// this way, as the keys would).
    pub(crate) fn typed(&mut self, text: &str, cx: &mut Context<Self>) {
        let t = self.bar.type_text(text);
        self.apply(t, cx);
    }

    /// Keys while the bar has them; nothing reaches the page.
    fn bar_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.bar.keys {
            return;
        }
        let k = &ev.keystroke;
        let m = &k.modifiers;
        let typed = match k.key.as_str() {
            key @ ("escape" | "enter" | "backspace") => self.bar.key(key, m.shift),
            _ if m.control || m.platform || m.alt => return,
            _ => match &k.key_char {
                Some(text) => self.bar.type_text(text),
                None => Typed::Nothing,
            },
        };
        self.apply(typed, cx);
        cx.stop_propagation();
    }

    /// Do what the bar asked for.
    fn apply(&mut self, typed: Typed, cx: &mut Context<Self>) {
        match typed {
            Typed::Nothing => {}
            Typed::Close => {
                #[cfg(feature = "web")]
                if let Some(v) = &self.view {
                    v.stop_finding();
                }
                self.take_focus();
            }
            Typed::Go(text) => match workspace_config::browser::web_address(&text) {
                Ok(url) => {
                    self.bar.address = None;
                    self.bar.error = None;
                    self.bar.keys = false;
                    #[cfg(feature = "web")]
                    if let Some(v) = &self.view {
                        v.load(&self.load_url(&url));
                    }
                    #[cfg(not(feature = "web"))]
                    let _ = url;
                    self.take_focus();
                }
                Err(why) => self.bar.error = Some(why),
            },
            Typed::Search(query) => {
                #[cfg(feature = "web")]
                if let Some(v) = &self.view {
                    if query.is_empty() {
                        v.stop_finding();
                    } else {
                        v.find(&query, true, false);
                    }
                }
                #[cfg(not(feature = "web"))]
                let _ = query;
            }
            Typed::Step(forward) => {
                #[cfg(feature = "web")]
                if let (Some(v), Some((query, _))) = (&self.view, &self.bar.find) {
                    v.find(query, forward, true);
                }
                #[cfg(not(feature = "web"))]
                let _ = forward;
            }
        }
        cx.notify();
    }

    /// Back, forward, reload, or zoom, in the browser.
    fn go(&self, how: Go) {
        if !self.browser() {
            return;
        }
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            match how {
                Go::Back => v.back(),
                Go::Forward => v.forward(),
                Go::Reload => v.reload(),
                Go::Zoom(step) => v.zoom(step),
            }
        }
        #[cfg(not(feature = "web"))]
        let _ = how;
    }

    /// What the page reported.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn page_event(&mut self, e: web_engine::Event, cx: &mut Context<Self>) {
        match e {
            // A click in the page: it has the keys; the address bar goes.
            web_engine::Event::Focus => {
                self.bar.keys = false;
                if self.bar.address.take().is_some() {
                    self.bar.error = None;
                }
            }
            web_engine::Event::LoadFailed { url, error } if self.browser() => {
                // The address bar shows, with the error, to fix it.
                let error = if error.is_empty() {
                    "The page did not load".to_owned()
                } else {
                    error
                };
                self.bar.address = Some((self.host_url(&url), false));
                self.bar.error = Some(error);
                self.bar.keys = self.focus_is_ours(cx);
                if self.bar.keys {
                    self.release_focus();
                }
            }
            web_engine::Event::LoadFailed { .. } => {}
            web_engine::Event::Status(line) => self.status(line, cx),
            web_engine::Event::DownloadAsked { id, name, size } => {
                self.bar.downloads.push(Download {
                    id,
                    name: workspace_config::downloads::file_name(&name),
                    size,
                    state: Saving::Asked,
                });
            }
            web_engine::Event::DownloadProgress {
                id,
                received: now,
                total,
            } => {
                if let Some(d) = self.bar.downloads.iter_mut().find(|d| d.id == id) {
                    if let Saving::Writing { received, .. } = &mut d.state {
                        *received = now;
                    }
                    if total.is_some() {
                        d.size = total;
                    }
                }
            }
            web_engine::Event::DownloadDone { id, path } => self.download_done(id, path, cx),
            web_engine::Event::DownloadEnded { id, error, partial } => {
                self.download_ended(id, error, partial, cx);
            }
            web_engine::Event::Found { count, current, .. } => {
                if let Some((_, found)) = &mut self.bar.find {
                    *found = Some((count, current));
                }
            }
        }
        cx.notify();
    }

    /// Show `line` in the pane's status for a few seconds.
    fn status(&mut self, line: String, cx: &mut Context<Self>) {
        let at = Instant::now();
        self.bar.status = Some((line, at));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STATUS_FOR).await;
            let _ = this.update(cx, |w, cx| {
                if w.bar.status.as_ref().is_some_and(|(_, t)| *t == at) {
                    w.bar.status = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Save (`save`) or Cancel for download `id`: the row's buttons (and
    /// the smoke). Save reserves the file in the downloads folder, off
    /// this thread, and then lets Chromium write it; Cancel stops it, and
    /// what it wrote is removed.
    pub(crate) fn answer_download(&mut self, id: u32, save: bool, cx: &mut Context<Self>) {
        let Some(at) = self.bar.downloads.iter().position(|d| d.id == id) else {
            return;
        };
        if !save {
            #[cfg(feature = "web")]
            if let Some(v) = &self.view {
                v.cancel_download(id);
            }
            let d = &mut self.bar.downloads[at];
            match &d.state {
                Saving::Writing { part, .. } | Saving::Canceling { part } => {
                    d.state = Saving::Canceling { part: part.clone() };
                    // Chromium reports the end; if it does not, the
                    // partial file goes anyway.
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(CANCEL_WAIT).await;
                        let _ = this.update(cx, |w, cx| w.download_ended(id, None, None, cx));
                    })
                    .detach();
                }
                // Nothing written yet (a reservation still coming is
                // removed when it comes).
                Saving::Asked | Saving::Reserving => {
                    let name = self.bar.downloads.remove(at).name;
                    self.status(format!("Download canceled: {name}"), cx);
                }
            }
            cx.notify();
            return;
        }
        let d = &mut self.bar.downloads[at];
        if d.state != Saving::Asked {
            return;
        }
        d.state = Saving::Reserving;
        let name = d.name.clone();
        let configured = crate::live::with(|l| l.downloads.clone()).flatten();
        cx.spawn(async move |this, cx| {
            let reserved = cx
                .background_executor()
                .spawn(async move {
                    let folder = workspace_config::downloads::folder(configured.as_deref())
                        .ok_or_else(|| "no home directory to save it in".to_owned())?;
                    workspace_config::downloads::reserve(&folder, &name)
                        .map_err(|e| format!("{}: {e}", tilde(&folder)))
                })
                .await;
            let part = reserved.as_ref().ok().map(|(_, p)| p.clone());
            // The pane closed meanwhile: its reservation goes.
            if this
                .update(cx, |w, cx| w.reserved(id, reserved, cx))
                .is_err()
                && let Some(part) = part
            {
                cx.background_executor()
                    .spawn(async move {
                        let _ = std::fs::remove_file(part);
                    })
                    .detach();
            }
        })
        .detach();
        cx.notify();
    }

    /// Download `id`'s file was reserved (its path and its `.part`'s), or
    /// could not be: Chromium writes it now, unless it was canceled
    /// meanwhile.
    fn reserved(
        &mut self,
        id: u32,
        reserved: Result<(PathBuf, PathBuf), String>,
        cx: &mut Context<Self>,
    ) {
        let at = self
            .bar
            .downloads
            .iter()
            .position(|d| d.id == id && d.state == Saving::Reserving);
        let (at, (file, part)) = match (at, reserved) {
            (Some(at), Ok(r)) => (at, r),
            (Some(at), Err(e)) => {
                #[cfg(feature = "web")]
                if let Some(v) = &self.view {
                    v.cancel_download(id);
                }
                let name = self.bar.downloads.remove(at).name;
                self.status(format!("Download not saved: {name}: {e}"), cx);
                return;
            }
            // Canceled while it was reserved.
            (None, Ok((_, part))) => {
                discard(part, None, cx);
                return;
            }
            (None, Err(_)) => return,
        };
        #[cfg(feature = "web")]
        let started = match &self.view {
            Some(v) => v.save_download(id, &part),
            None => Err("the page has closed".to_owned()),
        };
        #[cfg(not(feature = "web"))]
        let started: Result<(), String> = Err("no web engine".to_owned());
        match started {
            Ok(()) => {
                self.bar.downloads[at].state = Saving::Writing {
                    file,
                    part,
                    received: 0,
                };
                cx.notify();
            }
            Err(e) => {
                let name = self.bar.downloads.remove(at).name;
                discard(part, None, cx);
                self.status(format!("Download not saved: {name}: {e}"), cx);
            }
        }
    }

    /// Download `id` is complete at `path`: rename it to its file, off
    /// this thread, and say where it is.
    fn download_done(&mut self, id: u32, path: PathBuf, cx: &mut Context<Self>) {
        let Some(at) = self.bar.downloads.iter().position(|d| d.id == id) else {
            return;
        };
        let d = self.bar.downloads.remove(at);
        let (file, part) = match d.state {
            Saving::Writing { file, part, .. } => (file, part),
            // Completed as it was canceled: the user said no.
            Saving::Canceling { part } => {
                discard(part, Some(path), cx);
                self.status(format!("Download canceled: {}", d.name), cx);
                return;
            }
            Saving::Asked | Saving::Reserving => return,
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    // Chromium wrote where it was told; if it chose another
                    // name, the reservation goes.
                    let from = if path.as_os_str().is_empty() {
                        part.clone()
                    } else {
                        path
                    };
                    if from != part {
                        let _ = std::fs::remove_file(&part);
                    }
                    workspace_config::downloads::finish(&from, &file)
                })
                .await;
            let line = match done {
                Ok(f) => format!(
                    "Saved {} to {}",
                    f.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default(),
                    f.parent().map(tilde).unwrap_or_default()
                ),
                Err(e) => format!("Download not saved: {}: {e}", d.name),
            };
            let _ = this.update(cx, |w, cx| w.status(line, cx));
        })
        .detach();
    }

    /// Download `id` ended unfinished (canceled, or failed with `error`):
    /// what it wrote is removed, off this thread.
    fn download_ended(
        &mut self,
        id: u32,
        error: Option<String>,
        partial: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = self.bar.downloads.iter().position(|d| d.id == id) else {
            return;
        };
        let d = self.bar.downloads.remove(at);
        if let Saving::Writing { part, .. } | Saving::Canceling { part } = d.state {
            discard(part, partial, cx);
        }
        let line = match error {
            Some(e) => format!("Download failed: {}: {e}", d.name),
            None => format!("Download canceled: {}", d.name),
        };
        self.status(line, cx);
    }

    /// Whether this pane is the workspace's focused pane.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn focus_is_ours(&self, cx: &Context<Self>) -> bool {
        self.workspace
            .read_with(cx, |ws, _| ws.focused_pane() == Some(self.pane))
            .unwrap_or(false)
    }

    /// The address tmux keeps for the pane changed to `url`: unless that
    /// is where the page already is (its own report coming back), load it.
    pub(crate) fn follow(&mut self, url: &str) {
        if url == self.url {
            return;
        }
        self.url = url.to_owned();
        self.kept = (String::new(), 0, 0);
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            v.load(&self.load_url(url));
        }
    }

    /// Whether the pane shows the page's snapshot now (smoke evidence).
    #[cfg(feature = "web")]
    pub(crate) fn shows_snapshot(&self) -> bool {
        !self.live && matches!(self.snapshot, Snapshot::Taken(Some(_)))
    }

    /// Ask Chromium for the page as drawn now. The page stays live until
    /// the image comes (decoded off this thread), or at most
    /// [`SNAPSHOT_WAIT`], and the workspace lays the pane out again then.
    ///
    /// With `web.color: display` the page is drawn in the screen's color
    /// space, and so is its capture: it is converted to sRGB, as GPUI
    /// draws images, so the snapshot has the live page's colors.
    #[cfg(feature = "web")]
    fn ask_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(v) = &self.view else { return };
        self.snapshot = Snapshot::Asked;
        let screen = (web_engine::color() == web_engine::Color::Display)
            .then(|| v.screen_color_space())
            .flatten();
        let (tx, rx) = async_channel::bounded(1);
        v.capture(move |reply| {
            let _ = tx.try_send(reply);
        });
        cx.spawn(async move |this, cx| {
            let ex = cx.background_executor().clone();
            let decoded = ex.spawn(async move {
                let reply = rx.recv().await.ok()?.ok()?;
                decode_snapshot(&reply, screen.as_ref()).ok()
            });
            let timer = ex.timer(SNAPSHOT_WAIT);
            let image = futures_lite::future::or(decoded, async {
                timer.await;
                None
            })
            .await;
            let _ = this.update(cx, |w, cx| {
                if matches!(w.snapshot, Snapshot::Asked) {
                    w.snapshot = Snapshot::Taken(image);
                    w.relayout(cx);
                }
            });
        })
        .detach();
    }

    /// Lay the panes out again: what this pane may show changed.
    #[cfg(feature = "web")]
    fn relayout(&self, cx: &mut Context<Self>) {
        cx.notify();
        let _ = self.workspace.update(cx, |_, cx| cx.notify());
    }

    /// Ask the page where it is scrolled to, and keep that, its title, and
    /// its address in the placeholder if they changed.
    #[cfg(feature = "web")]
    pub(crate) fn keep_state(&self, cx: &mut Context<Self>) {
        let Some(v) = &self.view else { return };
        // An app's page has no placeholder to keep it in.
        if self.app.is_some() || v.load_status().is_none() {
            return;
        }
        let title = v.title();
        let url = Some(self.host_url(&v.url()))
            .filter(|u| *u != self.url && workspace_config::browser::web_url_allowed(u));
        let (tx, rx) = async_channel::bounded(1);
        v.scroll_position(move |at| {
            let _ = tx.try_send(at);
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((x, y))) = rx.recv().await else {
                return;
            };
            let _ = this.update(cx, |w, cx| {
                let kept = (title, x, y);
                if w.kept == kept && url.is_none() {
                    return;
                }
                let state = api::WebState {
                    pane: w.pane,
                    title: kept.0.clone(),
                    scroll_x: x,
                    scroll_y: y,
                    url: url.clone(),
                    ..api::WebState::default()
                };
                w.kept = kept;
                // Its own report coming back is not a navigation.
                if let Some(u) = url {
                    w.url = u;
                }
                let _ = w.workspace.update(cx, |ws, _| ws.keep_web_state(state));
            });
        })
        .detach();
    }

    /// Whether the page's browser profile is ready for it to open. If not,
    /// Chromium is making (or loading) it, and the pane paints again once
    /// it is.
    #[cfg(feature = "web")]
    fn profile_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.profile_wait {
            return false;
        }
        let (tx, rx) = async_channel::bounded::<()>(1);
        let app = self.app.as_ref().map(|a| a.app.as_str());
        match web_engine::profile_ready(app, move || {
            let _ = tx.try_send(());
        }) {
            Ok(true) => true,
            Ok(false) => {
                self.profile_wait = true;
                cx.spawn(async move |this, cx| {
                    let _ = rx.recv().await;
                    let _ = this.update(cx, |w, cx| {
                        w.profile_wait = false;
                        cx.notify();
                    });
                })
                .detach();
                false
            }
            Err(e) => {
                self.problem = Some(e);
                cx.notify();
                false
            }
        }
    }

    /// Painted at `bounds`: open the page on first paint, then keep its
    /// view on the pane.
    #[cfg(feature = "web")]
    fn painted(
        &mut self,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.problem.is_some() || self.wants_component {
            return;
        }
        let rect = web_engine::Rect {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        };
        // A page through a forward opens once the forward is up.
        let forwarding = self.forward.as_ref().is_some_and(|f| f.local.is_none());
        if self.view.is_none() && (forwarding || !self.profile_ready(cx)) {
            return;
        }
        if self.view.is_none() {
            // What the page reports comes here, on the main thread; a click
            // in the page focuses this pane, as a click on a terminal pane
            // does.
            let (tx, rx) = async_channel::unbounded::<web_engine::Event>();
            let any = window.window_handle();
            let (ws, pane) = (self.workspace.clone(), self.pane);
            cx.spawn(async move |this, cx| {
                use gpui::AppContext as _;
                while let Ok(e) = rx.recv().await {
                    if e == web_engine::Event::Focus {
                        let _ = cx.update_window(any, |_, window, cx| {
                            let _ = ws.update(cx, |ws, cx| ws.page_focused(pane, window, cx));
                        });
                    }
                    if this.update(cx, |w, cx| w.page_event(e, cx)).is_err() {
                        return;
                    }
                }
            })
            .detach();
            let app = self.app.clone();
            let url = self.load_url(&self.url);
            match web_engine::WebView::open(window, rect, &url, app, move |e| {
                let _ = tx.try_send(e);
            }) {
                Ok(v) => {
                    engine::opened(cx.entity().downgrade());
                    // The pane was focused before its page existed.
                    if self.focus.is_focused(window) && !self.bar.keys {
                        v.focus();
                    }
                    let (_, x, y) = self.kept;
                    if (x, y) != (0, 0) {
                        v.restore_scroll(x, y);
                    }
                    self.view = Some(v);
                    // While the page is on screen, its state is kept every
                    // few seconds.
                    cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor().timer(KEEP_EVERY).await;
                            let alive = this.update(cx, |w, cx| {
                                if w.place == Place::Shown {
                                    w.keep_state(cx);
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                    })
                    .detach();
                }
                Err(e) => {
                    self.problem = Some(e);
                    cx.notify();
                    return;
                }
            }
        }
        if let Some(v) = &self.view {
            v.place(rect);
            v.set_visible(self.live);
        }
    }

    /// The browser's strip: the address bar or the find field, and the
    /// status line.
    fn strip(&self, window: &Window) -> Option<gpui::Div> {
        if !self.bar.shown() {
            return None;
        }
        let t = crate::theme::for_window(window);
        let field = |label: &'static str, text: &str, all: bool| {
            let caret = div().text_color(c(t.accent)).child("▏");
            let shown = if text.is_empty() {
                div().text_color(c(t.faint)).child(label)
            } else if all {
                div().bg(c(t.selected)).child(text.to_owned())
            } else {
                div().child(text.to_owned())
            };
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .overflow_hidden()
                .px_2()
                .py(px(2.0))
                .rounded_md()
                .bg(c(t.background))
                .border_1()
                .border_color(c(if self.bar.keys { t.focus } else { t.divider }))
                .text_color(c(t.text))
                .child(shown)
                .when(self.bar.keys, |d| d.child(caret))
        };
        let (left, note): (Option<gpui::Div>, Option<(String, u32)>) =
            match (&self.bar.address, &self.bar.find) {
                (Some((typed, all)), _) => (
                    Some(field("Address", typed, *all)),
                    self.bar.error.clone().map(|e| (e, t.danger)),
                ),
                (None, Some((query, found))) => (
                    Some(field("Find in page", query, false)),
                    Some((found_text(*found, query), t.muted)),
                ),
                (None, None) => (None, None),
            };
        let note = note
            .filter(|(s, _)| !s.is_empty())
            .or_else(|| self.bar.status_line().map(|s| (s.to_owned(), t.muted)));
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .h(px(STRIP))
                .px_2()
                .bg(c(t.raised))
                .border_b_1()
                .border_color(c(t.divider))
                .text_size(px(12.0))
                .children(left)
                .children(note.map(|(s, color)| {
                    div()
                        .flex_none()
                        .max_w(px(360.0))
                        .overflow_hidden()
                        .text_color(c(color))
                        .child(s)
                })),
        )
    }
}

/// Remove what an unfinished download wrote (its `.part`, and Chromium's
/// own partial file beside it), off the UI thread.
fn discard(part: PathBuf, partial: Option<PathBuf>, cx: &mut Context<WebPane>) {
    gpui::AppContext::background_spawn(cx, async move {
        workspace_config::downloads::discard(&part, partial.as_deref());
    })
    .detach();
}

impl WebPane {
    /// The oldest download's row, under the strip: what it is, and Save
    /// and Cancel (Cancel only, once saving).
    fn download_row(&self, window: &Window, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let d = self.bar.downloads.first()?;
        let t = crate::theme::for_window(window);
        let folder = crate::live::with(|l| l.downloads.clone())
            .flatten()
            .unwrap_or_else(|| "Downloads".to_owned());
        let id = d.id;
        let button = |label: &'static str, key: &'static str, primary: bool| {
            div()
                .id((key, id as usize))
                .flex_none()
                .px_2()
                .py(px(1.0))
                .rounded_md()
                .cursor_pointer()
                .bg(c(if primary { t.accent } else { t.selected }))
                .text_color(c(if primary { t.background } else { t.text }))
                .child(label)
        };
        let save = (d.state == Saving::Asked).then(|| {
            button("Save", "download-save", true)
                .on_click(cx.listener(move |w, _, _, cx| w.answer_download(id, true, cx)))
        });
        let cancel = (!matches!(d.state, Saving::Canceling { .. })).then(|| {
            button("Cancel", "download-cancel", false)
                .on_click(cx.listener(move |w, _, _, cx| w.answer_download(id, false, cx)))
        });
        let more = self.bar.downloads.len() - 1;
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .h(px(STRIP))
                .px_2()
                .bg(c(t.raised))
                .border_b_1()
                .border_color(c(t.divider))
                .text_size(px(12.0))
                .text_color(c(t.text))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(download_text(d, &folder)),
                )
                .children((more > 0).then(|| {
                    div()
                        .flex_none()
                        .text_color(c(t.muted))
                        .child(format!("+{more}"))
                }))
                .children(save)
                .children(cancel),
        )
    }
}

/// The strip's height.
const STRIP: f32 = 30.0;

/// Back, forward, reload, or zoom (in, out, or back to 100%).
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "web"), allow(dead_code))]
enum Go {
    Back,
    Forward,
    Reload,
    Zoom(Option<bool>),
}

/// How long a covered page waits for its snapshot (and the overlay with
/// it) before it is hidden without one.
#[cfg(feature = "web")]
const SNAPSHOT_WAIT: std::time::Duration = std::time::Duration::from_millis(400);

/// How often a page on screen has its title and scroll position kept.
#[cfg(feature = "web")]
const KEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// The image in Chromium's reply to a capture (`Page.captureScreenshot`:
/// a PNG, base64, in `data`). Slow: never on the UI thread.
///
/// `screen` is the screen's color space when Chromium draws in it
/// (`web.color: display`): the image is then converted to sRGB, from the
/// color space its PNG names (an ICC profile), or from `screen` when it
/// names none. A PNG marked sRGB, or any capture with `web.color: srgb`,
/// is drawn as it is.
#[cfg(feature = "web")]
fn decode_snapshot(
    reply: &[u8],
    screen: Option<&web_engine::ColorSpace>,
) -> Result<Arc<RenderImage>, String> {
    let v: serde_json::Value = serde_json::from_slice(reply).map_err(|e| e.to_string())?;
    let data = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or("no image in the reply")?;
    let png = api::json::decode_base64(data)?;
    if screen.is_none() {
        return terminal_render::decode_image(&png);
    }
    let (width, height, rgba) = snapshot_pixels(&png, screen)?;
    terminal_render::rgba_image(width, height, rgba).ok_or_else(|| "an image of no size".into())
}

/// A capture's size and RGBA pixels, in sRGB: converted from the color
/// space the PNG names, or from `screen` when it names none, unless it
/// is marked sRGB or `screen` is `None` (`web.color: srgb`).
#[cfg(feature = "web")]
fn snapshot_pixels(
    png: &[u8],
    screen: Option<&web_engine::ColorSpace>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let (width, height, mut rgba, space) = decode_png(png)?;
    let Some(screen) = screen else {
        return Ok((width, height, rgba));
    };
    let named = match space {
        PngSpace::Srgb => return Ok((width, height, rgba)),
        PngSpace::Icc(profile) => web_engine::ColorSpace::from_icc(&profile),
        PngSpace::Unnamed => None,
    };
    web_engine::capture_to_srgb(&mut rgba, width, height, named.as_ref().unwrap_or(screen))?;
    Ok((width, height, rgba))
}

/// The color space a PNG names.
#[cfg(feature = "web")]
pub(crate) enum PngSpace {
    /// An `sRGB` chunk.
    Srgb,
    /// An ICC profile (`iCCP`).
    Icc(Vec<u8>),
    /// Neither.
    Unnamed,
}

/// A PNG's size, its pixels as 8-bit RGBA, and the color space it names.
#[cfg(feature = "web")]
pub(crate) fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>, PngSpace), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let space = match (&reader.info().icc_profile, reader.info().srgb) {
        (Some(icc), _) => PngSpace::Icc(icc.to_vec()),
        (None, Some(_)) => PngSpace::Srgb,
        (None, None) => PngSpace::Unnamed,
    };
    let mut buf = vec![0; reader.output_buffer_size().ok_or("an image too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("an indexed PNG after expansion".into()),
    };
    Ok((info.width, info.height, rgba, space))
}

impl Render for WebPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::keymap as k;
        let t = crate::theme::for_window(window);
        // Dreamer Web was added: Chromium starts, and the page opens as
        // the pane paints.
        #[cfg(feature = "web")]
        if self.wants_component && crate::webcomp::status() == crate::webcomp::Status::Ready {
            self.wants_component = false;
            cx.defer_in(window, |w, _, cx| {
                w.problem = engine::ensure(cx).err();
                cx.notify();
            });
        }
        // What shows where the page is not drawn: under the page for a
        // moment as it starts, or instead of it.
        let notice = div()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(12.0))
            .text_color(c(t.muted))
            .child(match &self.forward {
                Some(f) => format!("web: {} on {}", shorten(&self.url, 200), f.host),
                None => format!("web: {}", shorten(&self.url, 200)),
            })
            .children(
                self.problem
                    .clone()
                    .map(|p| div().text_color(c(t.warning)).child(p)),
            )
            .when(self.wants_component, |d| {
                d.child(crate::webcomp::in_pane(&t))
            });
        // Under an overlay, the page as it was when the overlay opened.
        let snapshot = match &self.snapshot {
            Snapshot::Taken(Some(image)) if !self.live => Some(
                gpui::img(image.clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(gpui::ObjectFit::Fill),
            ),
            _ => None,
        };
        let page = div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(notice)
            .children(snapshot);
        #[cfg(feature = "web")]
        let page = {
            let entity = cx.entity();
            page.child(
                gpui::canvas(
                    |bounds, _, _| bounds,
                    move |_, bounds, window, cx| {
                        entity.update(cx, |w, cx| w.painted(bounds, window, cx));
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
        };
        let base = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(t.background))
            .track_focus(&self.focus)
            .key_context(if self.browser() {
                "WebPane Browser"
            } else {
                k::WEB_PANE
            })
            .on_action(cx.listener(|w, _: &k::Copy, _, cx| w.edit(Edit::Copy, cx)))
            .on_action(cx.listener(|w, _: &k::Cut, _, cx| w.edit(Edit::Cut, cx)))
            .on_action(cx.listener(|w, _: &k::Paste, _, cx| w.edit(Edit::Paste, cx)))
            .on_action(cx.listener(|w, _: &k::SelectAll, _, cx| w.edit(Edit::SelectAll, cx)))
            .on_key_down(cx.listener(|w, ev: &KeyDownEvent, _, cx| w.bar_key(ev, cx)));
        let base = if self.browser() {
            base.on_action(cx.listener(|w, _: &k::OpenLocation, _, cx| w.open_address(cx)))
                .on_action(cx.listener(|w, _: &k::Find, _, cx| w.open_find(cx)))
                .on_action(cx.listener(|w, _: &k::Back, _, _| w.go(Go::Back)))
                .on_action(cx.listener(|w, _: &k::Forward, _, _| w.go(Go::Forward)))
                .on_action(cx.listener(|w, _: &k::Reload, _, _| w.go(Go::Reload)))
                .on_action(cx.listener(|w, _: &k::FontBigger, _, _| w.go(Go::Zoom(Some(true)))))
                .on_action(cx.listener(|w, _: &k::FontSmaller, _, _| w.go(Go::Zoom(Some(false)))))
                .on_action(cx.listener(|w, _: &k::FontReset, _, _| w.go(Go::Zoom(None))))
        } else {
            base
        };
        base.children(self.strip(window))
            .children(self.download_row(window, cx))
            .child(page)
    }
}

/// At most `max` characters of `s`, with an ellipsis when cut.
fn shorten(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Bar, Download, Place, Saving, Typed, decide, download_text, found_text};

    /// A 1×1 PNG of `rgb`, naming `icc` as its color space, or sRGB.
    #[cfg(all(feature = "web", target_os = "macos"))]
    fn one_pixel(rgb: [u8; 3], icc: Option<Vec<u8>>, srgb: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut info = png::Info::with_size(1, 1);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        info.icc_profile = icc.map(Into::into);
        info.srgb = srgb.then_some(png::SrgbRenderingIntent::Perceptual);
        let mut w = png::Encoder::with_info(&mut out, info)
            .expect("info")
            .write_header()
            .expect("header");
        w.write_image_data(&rgb).expect("pixel");
        w.finish().expect("png");
        out
    }

    #[test]
    #[cfg(all(feature = "web", target_os = "macos"))]
    fn a_display_capture_is_converted_to_srgb_from_the_space_it_names_else_the_screens() {
        use web_engine::ColorSpace;
        let p3 = ColorSpace::display_p3().expect("P3");
        let srgb = ColorSpace::srgb().expect("sRGB");
        // sRGB's red, as Display P3 writes it.
        let mut red_in_p3 = [255, 0, 0, 255];
        web_engine::srgb_to(&mut red_in_p3, 1, 1, &p3).expect("to P3");
        let red_in_p3 = [red_in_p3[0], red_in_p3[1], red_in_p3[2]];
        assert!(red_in_p3[1] > 30, "{red_in_p3:?}");
        let is_red = |px: &[u8]| px[0] > 245 && px[1] < 12 && px[2] < 12;
        let pixels = |png: &[u8], screen| super::snapshot_pixels(png, screen).expect("pixels").2;

        // The PNG names Display P3: converted from it, whatever the screen.
        let named = one_pixel(red_in_p3, p3.icc(), false);
        assert!(is_red(&pixels(&named, Some(&srgb))));
        // It names nothing: the screen's space (Display P3 here).
        let unnamed = one_pixel(red_in_p3, None, false);
        assert!(is_red(&pixels(&unnamed, Some(&p3))));
        // Marked sRGB, or `web.color: srgb` (no screen): as it is.
        let marked = one_pixel(red_in_p3, None, true);
        assert_eq!(pixels(&marked, Some(&p3))[..3], red_in_p3);
        assert_eq!(pixels(&named, None)[..3], red_in_p3);
    }

    #[test]
    fn a_page_shows_only_where_its_pane_is_drawn_and_uncovered() {
        // On another tab or zoomed away: never on screen, no snapshot.
        for live in [false, true] {
            assert_eq!(decide(Place::Hidden, live, false, false), (false, false));
        }
        // Drawn and uncovered: on screen (again).
        assert_eq!(decide(Place::Shown, false, true, true), (true, false));
    }

    #[test]
    fn a_covered_page_stays_until_its_snapshot_is_taken() {
        // The overlay opens: snapshot first, the page still live.
        assert_eq!(decide(Place::Covered, true, false, false), (true, true));
        // Asked once, waiting.
        assert_eq!(decide(Place::Covered, true, true, false), (true, false));
        // Taken (or given up on): off the screen, the overlay shows.
        assert_eq!(decide(Place::Covered, true, true, true), (false, false));
        // A page that was not on screen does not come back for an overlay.
        assert_eq!(decide(Place::Covered, false, false, false), (false, false));
    }

    #[test]
    fn the_address_bar_replaces_the_address_typing_over_it_and_goes_on_enter() {
        let mut bar = Bar::default();
        assert!(!bar.shown());
        // Nothing typed reaches a bar that does not have the keys.
        assert_eq!(bar.type_text("x"), Typed::Nothing);
        bar.open_address("https://example.com/a");
        assert!(bar.shown() && bar.keys);
        // The first key replaces the whole address; then it adds.
        bar.type_text("l");
        bar.type_text("ocalhost:300");
        assert_eq!(bar.key("backspace", false), Typed::Nothing);
        bar.type_text("01");
        assert_eq!(bar.key("enter", false), Typed::Go("localhost:3001".into()));
        // Backspace on the selected address clears it.
        bar.open_address("https://example.com/a");
        bar.key("backspace", false);
        assert_eq!(bar.address, Some((String::new(), false)));
        // Esc hides it and gives the keys back.
        assert_eq!(bar.key("escape", false), Typed::Close);
        assert!(!bar.shown() && !bar.keys);
    }

    #[test]
    fn find_searches_as_typed_and_steps_through_matches() {
        let mut bar = Bar::default();
        bar.open_find();
        assert_eq!(bar.type_text("fo"), Typed::Search("fo".into()));
        assert_eq!(bar.type_text("x"), Typed::Search("fox".into()));
        assert_eq!(bar.key("backspace", false), Typed::Search("fo".into()));
        assert_eq!(bar.key("enter", false), Typed::Step(true));
        assert_eq!(bar.key("enter", true), Typed::Step(false));
        assert_eq!(found_text(Some((12, 3)), "fo"), "3 of 12");
        assert_eq!(found_text(Some((0, 0)), "fo"), "No matches");
        assert_eq!(found_text(None, "fo"), "");
        // Closed, then opened again: the last query is gone with it.
        assert_eq!(bar.key("escape", false), Typed::Close);
        assert!(bar.find.is_none() && !bar.keys);
    }

    #[test]
    fn a_download_row_names_the_file_its_size_and_its_progress() {
        let mut d = Download {
            id: 1,
            name: "report.bin".into(),
            size: Some(2_500_000),
            state: Saving::Asked,
        };
        assert_eq!(
            download_text(&d, "~/Downloads"),
            "Save report.bin (2.5 MB) to ~/Downloads?"
        );
        d.state = Saving::Writing {
            file: "/d/report.bin".into(),
            part: "/d/report.bin.part".into(),
            received: 1_000_000,
        };
        assert_eq!(
            download_text(&d, "~/Downloads"),
            "Saving report.bin: 1.0 MB of 2.5 MB (40%)"
        );
        d.size = None;
        d.state = Saving::Writing {
            file: "/d/report.bin".into(),
            part: "/d/report.bin.part".into(),
            received: 12,
        };
        assert_eq!(download_text(&d, "x"), "Saving report.bin: 12 B");
    }

    #[test]
    fn web_download_is_the_grant_entry_and_the_manifest_capability() {
        assert_eq!(web_engine::browser::DOWNLOAD_CAPABILITY, api::DOWNLOAD_WEB);
    }

    #[test]
    fn the_browser_and_the_cli_name_the_same_profile() {
        assert_eq!(
            web_engine::browser::PROFILE,
            workspace_config::browser::PROFILE
        );
    }
}

#[cfg(feature = "web")]
mod engine {
    //! Chromium for the whole process: started with the first web pane,
    //! its work pumped from a GPUI foreground task, shut down at quit.

    use std::cell::RefCell;
    use std::time::Duration;

    use gpui::WeakEntity;

    use super::WebPane;

    thread_local! {
        /// Every web pane that opened a page, to close them all at quit.
        static PANES: RefCell<Vec<WeakEntity<WebPane>>> = const { RefCell::new(Vec::new()) };
    }

    /// The longest CEF waits between pumps while it runs (cefclient's
    /// external pump uses the same bound).
    const MAX_PUMP_DELAY: Duration = Duration::from_millis(33);

    pub(super) fn opened(pane: WeakEntity<WebPane>) {
        PANES.with(|p| p.borrow_mut().push(pane));
    }

    /// Start Chromium in this process unless it runs.
    pub(super) fn ensure(cx: &mut gpui::App) -> Result<(), String> {
        if web_engine::started() {
            return Ok(());
        }
        let profile =
            workspace_config::browser::web_dir().ok_or("no state directory for the web profile")?;
        // `dream browser clear` asked while Chromium had the profile open.
        if let Err(e) = workspace_config::browser::clear_if_asked(&profile) {
            eprintln!("dreamer: the browser's profile was not emptied: {e}");
        }
        // `web.color` as it is now: Chromium keeps it until it next starts.
        let color = match crate::live::with(|l| l.web.color).unwrap_or_default() {
            workspace_config::WebColor::Srgb => web_engine::Color::Srgb,
            workspace_config::WebColor::Display => web_engine::Color::Display,
        };
        let (tx, rx) = async_channel::unbounded::<i64>();
        web_engine::start(&profile, color, move |delay| {
            let _ = tx.try_send(delay);
        })?;
        // CEF asks for its work (with a delay) from any thread; it runs
        // here, on the main thread, between GPUI's own events.
        cx.spawn(async move |cx| {
            let ex = cx.background_executor().clone();
            loop {
                let timer = ex.timer(MAX_PUMP_DELAY);
                let asked = futures_lite::future::or(async { rx.recv().await.ok() }, async {
                    timer.await;
                    None
                })
                .await;
                if let Some(d) = asked.filter(|d| *d > 0) {
                    ex.timer(Duration::from_millis(d as u64).min(MAX_PUMP_DELAY))
                        .await;
                }
                cx.update(|_| web_engine::do_work());
                if !web_engine::started() {
                    return;
                }
            }
        })
        .detach();
        cx.on_app_quit(|cx| {
            for pane in PANES.with(|p| std::mem::take(&mut *p.borrow_mut())) {
                let _ = pane.update(cx, |w, _| w.view = None);
            }
            if let Err(e) = web_engine::shutdown(Duration::from_secs(2)) {
                eprintln!("dreamer: {e}");
            }
            async {}
        })
        .detach();
        Ok(())
    }
}
