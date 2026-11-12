//! The tmux object model and its reconciliation (spec 7.4, 15.1).
//!
//! [`Inventory::snapshot`] reads sessions, windows (one row per session
//! link, so linked windows are recognized), panes, and the server identity
//! in one non-yielding batch. [`InventoryTracker`] keeps the current model,
//! refreshes it when a notification says the structure may have changed,
//! and reports the differences as domain [`BackendEvent`]s.
//!
//! Notifications are hints. The model only ever changes from a complete
//! snapshot, so a missed or reordered notification cannot leave it wrong;
//! it can only make it late.

use std::collections::{BTreeMap, BTreeSet};

use domain::{
    BackendEvent, PaneGeneration, PaneInstance, PaneKey, ServerIncarnation, ServerKey, SessionKey,
    TmuxPaneId, TmuxServerRef, TmuxSessionId, TmuxWindowId, WindowKey, WindowLayout,
};
use tmux_protocol::{Notification, Response};

use crate::command::{Batch, TmuxCommand};
use crate::connection::{CommandError, Connection};
use crate::layout::parse_layout;

/// Snapshot layout. tmux prints control characters in command output as
/// `_`, so no control byte can serve as a field separator, and free text
/// (names, titles, commands) may contain any printable character. Numeric
/// fields therefore share one space-separated row, and every free-text field
/// is read by its own `list-*` command in the same non-yielding batch; rows
/// are matched by position. Output can never contain a newline, so each
/// object is exactly one line in every command.
const SEP: char = ' ';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    /// Number of clients attached.
    pub attached: u32,
    /// Windows by index.
    pub windows: BTreeMap<u32, TmuxWindowId>,
    pub active_window: Option<TmuxWindowId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub name: String,
    pub layout: WindowLayout,
    /// Every session the window is linked into.
    pub sessions: BTreeSet<TmuxSessionId>,
    pub active_pane: Option<TmuxPaneId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub window: TmuxWindowId,
    pub index: u32,
    pub cols: u16,
    pub rows: u16,
    pub dead: bool,
    pub pid: u32,
    pub command: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub server: TmuxServerRef,
    pub sessions: BTreeMap<TmuxSessionId, SessionInfo>,
    pub windows: BTreeMap<TmuxWindowId, WindowInfo>,
    pub panes: BTreeMap<TmuxPaneId, PaneInfo>,
}

#[derive(Debug)]
pub enum InventoryError {
    Command(CommandError),
    /// A snapshot row did not have the expected shape.
    Malformed(String),
    /// The tmux server restarted (new incarnation) under this connection.
    ServerChanged,
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command(e) => write!(f, "{e}"),
            Self::Malformed(m) => write!(f, "malformed tmux snapshot: {m}"),
            Self::ServerChanged => write!(f, "the tmux server restarted"),
        }
    }
}

impl std::error::Error for InventoryError {}

impl From<CommandError> for InventoryError {
    fn from(e: CommandError) -> Self {
        Self::Command(e)
    }
}

fn text_rows(r: &Response) -> Vec<String> {
    r.body
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

fn aligned(ids: usize, text: &[String], what: &str) -> Result<(), InventoryError> {
    if ids == text.len() {
        Ok(())
    } else {
        Err(InventoryError::Malformed(format!(
            "{what}: {ids} rows but {} text rows",
            text.len()
        )))
    }
}

fn fields(format: &[&str]) -> String {
    format
        .iter()
        .map(|f| format!("#{{{f}}}"))
        .collect::<Vec<_>>()
        .join(&SEP.to_string())
}

fn rows(r: &Response, n: usize, what: &str) -> Result<Vec<Vec<String>>, InventoryError> {
    r.body
        .iter()
        .map(|line| {
            let text = String::from_utf8_lossy(line);
            let parts: Vec<String> = text.split(SEP).map(str::to_owned).collect();
            if parts.len() == n {
                Ok(parts)
            } else {
                Err(InventoryError::Malformed(format!(
                    "{what}: expected {n} fields, got {}",
                    parts.len()
                )))
            }
        })
        .collect()
}

fn id<T>(s: &str, sigil: char, make: fn(u32) -> T) -> Result<T, InventoryError> {
    s.strip_prefix(sigil)
        .and_then(|d| d.parse().ok())
        .map(make)
        .ok_or_else(|| InventoryError::Malformed(format!("bad id `{s}`")))
}

fn num<T: std::str::FromStr>(s: &str) -> Result<T, InventoryError> {
    s.parse()
        .map_err(|_| InventoryError::Malformed(format!("bad number `{s}`")))
}

impl Inventory {
    /// Read the whole model in one batch.
    pub async fn snapshot(
        conn: &Connection,
        server: ServerKey,
        endpoint: &str,
    ) -> Result<Self, InventoryError> {
        let list = |name: &'static str, all: bool, format: String| {
            let cmd = TmuxCommand::new(name);
            let cmd = if all { cmd.flag("-a") } else { cmd };
            cmd.opt_format("-F", format)
        };
        let batch = Batch::new()
            .push(
                TmuxCommand::new("display-message")
                    .flag("-p")
                    .format(fields(&["pid", "start_time"])),
            )
            .push(list(
                "list-sessions",
                false,
                fields(&["session_id", "session_attached"]),
            ))
            .push(list("list-sessions", false, "#{session_name}".into()))
            .push(list(
                "list-windows",
                true,
                fields(&["session_id", "window_id", "window_index", "window_active"]),
            ))
            .push(list("list-windows", true, "#{window_name}".into()))
            .push(list("list-windows", true, "#{window_layout}".into()))
            .push(list(
                "list-panes",
                true,
                fields(&[
                    "pane_id",
                    "window_id",
                    "pane_index",
                    "pane_width",
                    "pane_height",
                    "pane_active",
                    "pane_dead",
                    "pane_pid",
                ]),
            ))
            .push(list("list-panes", true, "#{pane_current_command}".into()))
            .push(list("list-panes", true, "#{pane_title}".into()));
        let r = conn.execute(batch).await?;

        let ident = rows(&r[0], 2, "server")?;
        let ident = ident
            .first()
            .ok_or_else(|| InventoryError::Malformed("no server identity".into()))?;
        let pid: u64 = num(&ident[0])?;
        let start: u64 = num(&ident[1])?;
        let tmux = TmuxServerRef {
            server,
            endpoint: endpoint.to_owned(),
            incarnation: ServerIncarnation((start << 24) ^ pid),
        };

        let session_rows = rows(&r[1], 2, "session")?;
        let session_names = text_rows(&r[2]);
        aligned(session_rows.len(), &session_names, "sessions")?;
        let mut sessions = BTreeMap::new();
        for (f, name) in session_rows.iter().zip(session_names) {
            sessions.insert(
                id(&f[0], '$', TmuxSessionId)?,
                SessionInfo {
                    name,
                    attached: num(&f[1])?,
                    windows: BTreeMap::new(),
                    active_window: None,
                },
            );
        }

        let window_rows = rows(&r[3], 4, "window")?;
        let window_names = text_rows(&r[4]);
        let window_layouts = text_rows(&r[5]);
        aligned(window_rows.len(), &window_names, "window names")?;
        aligned(window_rows.len(), &window_layouts, "window layouts")?;
        let mut windows: BTreeMap<TmuxWindowId, WindowInfo> = BTreeMap::new();
        for ((f, name), layout) in window_rows.iter().zip(window_names).zip(window_layouts) {
            let session = id(&f[0], '$', TmuxSessionId)?;
            let window = id(&f[1], '@', TmuxWindowId)?;
            let index: u32 = num(&f[2])?;
            if let Some(s) = sessions.get_mut(&session) {
                s.windows.insert(index, window);
                if f[3] == "1" {
                    s.active_window = Some(window);
                }
            }
            windows
                .entry(window)
                .or_insert_with(|| WindowInfo {
                    name,
                    layout: parse_layout(&layout),
                    sessions: BTreeSet::new(),
                    active_pane: None,
                })
                .sessions
                .insert(session);
        }

        let pane_rows = rows(&r[6], 8, "pane")?;
        let pane_commands = text_rows(&r[7]);
        let pane_titles = text_rows(&r[8]);
        aligned(pane_rows.len(), &pane_commands, "pane commands")?;
        aligned(pane_rows.len(), &pane_titles, "pane titles")?;
        let mut panes = BTreeMap::new();
        for ((f, command), title) in pane_rows.iter().zip(pane_commands).zip(pane_titles) {
            let pane = id(&f[0], '%', TmuxPaneId)?;
            let window = id(&f[1], '@', TmuxWindowId)?;
            if f[5] == "1"
                && let Some(w) = windows.get_mut(&window)
            {
                w.active_pane = Some(pane);
            }
            panes.insert(
                pane,
                PaneInfo {
                    window,
                    index: num(&f[2])?,
                    cols: num(&f[3])?,
                    rows: num(&f[4])?,
                    dead: f[6] == "1",
                    pid: num(&f[7]).unwrap_or(0),
                    command,
                    title,
                },
            );
        }

        Ok(Self {
            server: tmux,
            sessions,
            windows,
            panes,
        })
    }

    fn session_key(&self, id: TmuxSessionId) -> SessionKey {
        SessionKey {
            tmux: self.server.clone(),
            session: id,
        }
    }

    fn window_key(&self, id: TmuxWindowId) -> WindowKey {
        WindowKey {
            tmux: self.server.clone(),
            window: id,
        }
    }

    fn pane_key(&self, id: TmuxPaneId) -> PaneKey {
        PaneKey {
            tmux: self.server.clone(),
            pane: id,
        }
    }
}

/// Whether a notification may mean the object model changed.
pub fn affects_structure(n: &Notification) -> bool {
    !matches!(
        n,
        Notification::Output { .. }
            | Notification::ExtendedOutput { .. }
            | Notification::Pause { .. }
            | Notification::Continue { .. }
            | Notification::PasteBufferChanged { .. }
            | Notification::PasteBufferDeleted { .. }
            | Notification::SubscriptionChanged { .. }
            | Notification::ConfigError { .. }
            | Notification::ClientDetached { .. }
    )
}

/// Keeps an [`Inventory`] current and reports changes.
#[derive(Debug)]
pub struct InventoryTracker {
    conn: Connection,
    server: ServerKey,
    endpoint: String,
    current: Inventory,
    generations: BTreeMap<TmuxPaneId, (u32, PaneGeneration)>,
}

impl InventoryTracker {
    /// Switch the connection to v2 layouts (which include floating panes)
    /// and read the first snapshot.
    pub async fn start(
        conn: Connection,
        server: ServerKey,
        endpoint: impl Into<String>,
    ) -> Result<(Self, Vec<BackendEvent>), InventoryError> {
        conn.execute(TmuxCommand::new("refresh-client").opt("-f", "new-layouts"))
            .await?;
        let endpoint = endpoint.into();
        let current = Inventory::snapshot(&conn, server.clone(), &endpoint).await?;
        let empty = Inventory {
            server: current.server.clone(),
            sessions: BTreeMap::new(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
        };
        let mut tracker = Self {
            conn,
            server,
            endpoint,
            current: empty,
            generations: BTreeMap::new(),
        };
        let events = tracker.apply(current);
        Ok((tracker, events))
    }

    pub fn inventory(&self) -> &Inventory {
        &self.current
    }

    /// Generation of a pane's current process.
    pub fn generation(&self, pane: TmuxPaneId) -> Option<PaneGeneration> {
        self.generations.get(&pane).map(|(_, g)| *g)
    }

    /// Re-read tmux and report what changed.
    pub async fn refresh(&mut self) -> Result<Vec<BackendEvent>, InventoryError> {
        let next = Inventory::snapshot(&self.conn, self.server.clone(), &self.endpoint).await?;
        if next.server != self.current.server {
            return Err(InventoryError::ServerChanged);
        }
        Ok(self.apply(next))
    }

    fn apply(&mut self, next: Inventory) -> Vec<BackendEvent> {
        let events = diff(&self.current, &next, &mut self.generations);
        self.current = next;
        events
    }
}

/// Events that turn `old` into `new`, in a stable order: sessions, then
/// windows, then panes.
fn diff(
    old: &Inventory,
    new: &Inventory,
    generations: &mut BTreeMap<TmuxPaneId, (u32, PaneGeneration)>,
) -> Vec<BackendEvent> {
    let mut out = Vec::new();

    for (id, s) in &new.sessions {
        match old.sessions.get(id) {
            None => out.push(BackendEvent::SessionAdded {
                session: new.session_key(*id),
                name: s.name.clone(),
            }),
            Some(prev) if prev.name != s.name => out.push(BackendEvent::SessionRenamed {
                session: new.session_key(*id),
                name: s.name.clone(),
            }),
            Some(_) => {}
        }
    }
    for id in old
        .sessions
        .keys()
        .filter(|k| !new.sessions.contains_key(k))
    {
        out.push(BackendEvent::SessionClosed {
            session: old.session_key(*id),
        });
    }

    for (id, w) in &new.windows {
        let prev = old.windows.get(id);
        if prev.is_none_or(|p| p.sessions != w.sessions) {
            out.push(BackendEvent::WindowAdded {
                window: new.window_key(*id),
                sessions: w.sessions.iter().map(|s| new.session_key(*s)).collect(),
            });
        }
        if prev.is_some_and(|p| p.name != w.name) {
            out.push(BackendEvent::WindowRenamed {
                window: new.window_key(*id),
                name: w.name.clone(),
            });
        }
        if prev.is_none_or(|p| p.layout != w.layout) {
            out.push(BackendEvent::WindowLayout {
                window: new.window_key(*id),
                layout: w.layout.clone(),
            });
        }
        if let Some(active) = w.active_pane
            && prev.is_none_or(|p| p.active_pane != w.active_pane)
        {
            out.push(BackendEvent::ActivePane {
                window: new.window_key(*id),
                pane: active,
            });
        }
    }
    for id in old.windows.keys().filter(|k| !new.windows.contains_key(k)) {
        out.push(BackendEvent::WindowClosed {
            window: old.window_key(*id),
        });
    }
    for (id, s) in &new.sessions {
        if let Some(active) = s.active_window
            && old
                .sessions
                .get(id)
                .is_none_or(|p| p.active_window != s.active_window)
        {
            out.push(BackendEvent::ActiveWindow {
                session: new.session_key(*id),
                window: active,
            });
        }
    }

    for (id, p) in &new.panes {
        let entry = generations.entry(*id).or_insert((p.pid, PaneGeneration(1)));
        if entry.0 != p.pid && p.pid != 0 {
            entry.0 = p.pid;
            entry.1 = PaneGeneration(entry.1.0 + 1);
            out.push(BackendEvent::PaneRespawned {
                pane: PaneInstance {
                    pane: new.pane_key(*id),
                    generation: entry.1,
                },
            });
        }
        let was_dead = old.panes.get(id).is_some_and(|o| o.dead);
        if p.dead && !was_dead {
            out.push(BackendEvent::PaneExited {
                pane: PaneInstance {
                    pane: new.pane_key(*id),
                    generation: entry.1,
                },
                status: None,
            });
        }
    }
    generations.retain(|id, _| new.panes.contains_key(id));
    out
}
