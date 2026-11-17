//! Launching pane apps and registering them (spec 12.5).
//!
//! `apps.launch` (owner only) starts a program in a new tmux window. The
//! daemon first writes a random single-use ticket to an owner-only file in
//! its private runtime directory, then gives the program only that file's
//! path and the launch id, as environment. The ticket itself never appears
//! in argv, the pane, tmux options, or logs.
//!
//! The program registers on its own connection with `apps.register`. The
//! daemon checks the ticket (once, before it expires), and checks that the
//! connecting process descends from the pane's process. Then the connection
//! holds the app's narrow grant until it closes.
//!
//! Every process of the user can open the owner-only socket and act as the
//! user: grants confine apps that run under Dreamer's launch, not processes
//! the user starts by other means (spec 12.2: process separation is not an
//! OS sandbox).
//!
//! A registered app's host is its pane's program. It publishes its current
//! view (`apps.publish`); the daemon keeps the newest per pane, marks the
//! pane in the topology, and hands the view to the user's desktops
//! (`apps.view`), which draw it in the pane's rectangle. Keys typed there
//! go back to the host (`apps.input`) as events on its connection. When
//! the host's connection closes, the last view stays, marked not live,
//! until the pane is gone.
//!
//! A host also publishes its view as a keyed tree (`apps.publish_tree`):
//! the whole tree, then the changes. The daemon keeps the newest version
//! and the recent changes, and answers a reader (`apps.tree`) with the
//! changes since the version it holds, or the whole tree when it holds
//! none or one too old.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use api::{RpcError, code};
use tokio::sync::Mutex;

use crate::log;

const DEFAULT_TTL_MS: u32 = 60_000;
/// How long registration waits for the launch to learn its pane.
const PANE_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Pending {
    app_id: String,
    grant: api::Grant,
    ticket: String,
    path: PathBuf,
    expires_unix_ms: u64,
    pane: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Apps {
    pending: Mutex<HashMap<String, Pending>>,
    /// App hosts by pane, and a counter that grows with every change to
    /// them (the revisions `apps.view` waits on).
    hosted: std::sync::Mutex<HashMap<u32, Hosted>>,
    changed: tokio::sync::watch::Sender<u64>,
}

/// A pane whose program is a registered app host.
#[derive(Debug)]
struct Hosted {
    instance: String,
    app_id: String,
    /// The app's grant: what a desktop may read or answer for it
    /// (`for_pane`).
    grant: api::Grant,
    /// The host's connection.
    conn: u64,
    live: bool,
    view: String,
    revision: u64,
    /// The view tree, its versions' recent changes, and the revision of
    /// its last change (the revisions `apps.tree` waits on).
    tree: view_tree::Kept,
    tree_revision: u64,
    /// Keys for the host, while it is connected.
    keys: Option<tokio::sync::mpsc::UnboundedSender<api::AppInput>>,
}

/// The longest key name `apps.input` takes.
const MAX_KEY: usize = 32;
/// Longest view tree node key (the manifest allows 64).
const MAX_NODE: usize = 64;
/// Longest text one `apps.input` carries (a paste).
const MAX_TEXT: usize = 64 * 1024;
/// How long `apps.view` waits by default, and at most.
const VIEW_WAIT_MS: u32 = 20_000;
const VIEW_WAIT_MAX_MS: u32 = 60_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn random_hex(bytes: usize) -> Result<String, RpcError> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| RpcError::new(code::INTERNAL, format!("random source: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compare without stopping at the first difference.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The parent of a process, from `ps` (macOS and Linux alike).
fn parent_of(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Whether `pid` is `ancestor` or descends from it.
pub(crate) fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut p = pid;
    for _ in 0..64 {
        if p == ancestor {
            return true;
        }
        // Up to and including pid 1; the top of the tree has no parent.
        match parent_of(p) {
            Some(parent) if parent != 0 && parent != p => p = parent,
            _ => return false,
        }
    }
    false
}

fn forbidden(why: &str) -> RpcError {
    RpcError::new(code::FORBIDDEN, why.to_owned())
}

impl Apps {
    /// Prepare a launch: a ticket file and a pending record. Returns the
    /// launch id, the ticket file, and the environment for the program.
    pub async fn prepare(
        &self,
        p: &api::LaunchApp,
    ) -> Result<(String, Vec<(String, String)>, u64), RpcError> {
        if p.app_id.is_empty() || p.argv.is_empty() {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "an app needs an id and a program",
            ));
        }
        let id = random_hex(8)?;
        let ticket = random_hex(32)?;
        let dir = platform::runtime_dir().join("launch");
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| RpcError::new(code::INTERNAL, format!("{}: {e}", dir.display())))?;
            let path = dir.join(&id);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| RpcError::new(code::INTERNAL, format!("{}: {e}", path.display())))?;
            f.write_all(ticket.as_bytes())
                .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?;
        }
        let path = dir.join(&id);
        let expires = now_ms() + u64::from(p.ttl_ms.unwrap_or(DEFAULT_TTL_MS));
        let grant = p.grant.clone().unwrap_or_default();
        self.pending.lock().await.insert(
            id.clone(),
            Pending {
                app_id: p.app_id.clone(),
                grant: api::Grant {
                    owner: false,
                    ..grant
                },
                ticket,
                path: path.clone(),
                expires_unix_ms: expires,
                pane: None,
            },
        );
        let env = vec![
            // The app talks to this daemon, whatever the pane inherited.
            (
                "DREAMER_RUNTIME_DIR".to_owned(),
                platform::runtime_dir().display().to_string(),
            ),
            ("DREAMER_LAUNCH_ID".to_owned(), id.clone()),
            (
                "DREAMER_LAUNCH_TICKET".to_owned(),
                path.display().to_string(),
            ),
        ];
        Ok((id, env, expires))
    }

    /// The launch's pane is known: registration can check against it.
    pub async fn launched(&self, id: &str, pane: u32) {
        if let Some(p) = self.pending.lock().await.get_mut(id) {
            p.pane = Some(pane);
        }
    }

    /// The launch failed: forget it and its ticket.
    pub async fn abandon(&self, id: &str) {
        if let Some(p) = self.pending.lock().await.remove(id) {
            let _ = std::fs::remove_file(&p.path);
        }
    }

    /// Check a registration from the process `peer` and use up the ticket.
    /// `pane_pid` finds the pane's process. The returned grant is the app's
    /// from now on.
    pub async fn register<F, Fut>(
        &self,
        r: &api::RegisterApp,
        peer: Option<u32>,
        pane_pid: F,
    ) -> Result<api::Registration, RpcError>
    where
        F: FnOnce(u32) -> Fut,
        Fut: std::future::Future<Output = Option<u32>>,
    {
        // Wait (briefly) for the launch to learn its pane: a fast program
        // can register before `new-window` has answered.
        let deadline = tokio::time::Instant::now() + PANE_WAIT;
        loop {
            let pane = self.pending.lock().await.get(&r.launch_id).map(|p| p.pane);
            match pane {
                None => return Err(forbidden("no such launch, or its ticket was already used")),
                Some(Some(_)) => break,
                Some(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Some(None) => break,
            }
        }
        // One attempt per ticket: whatever happens next, it is used up.
        let Some(p) = self.pending.lock().await.remove(&r.launch_id) else {
            return Err(forbidden("no such launch, or its ticket was already used"));
        };
        let _ = std::fs::remove_file(&p.path);
        if now_ms() >= p.expires_unix_ms {
            log::warn(format!(
                "apps: launch {} refused: its ticket expired",
                r.launch_id
            ));
            return Err(forbidden("the launch ticket expired"));
        }
        if !same(&p.ticket, &r.ticket) {
            log::warn(format!(
                "apps: launch {} refused: wrong ticket",
                r.launch_id
            ));
            return Err(forbidden("wrong launch ticket"));
        }
        let Some(pane) = p.pane else {
            return Err(forbidden("the launch has no pane"));
        };
        let Some(peer) = peer else {
            return Err(forbidden("the connecting process cannot be identified"));
        };
        let Some(root) = pane_pid(pane).await else {
            return Err(forbidden("the launch's pane is gone"));
        };
        if !descends_from(peer, root) {
            log::warn(format!(
                "apps: launch {} refused: process {peer} is not in pane %{pane}",
                r.launch_id
            ));
            return Err(forbidden(&format!(
                "the registering process is not running in pane %{pane}"
            )));
        }
        let mut grant = p.grant;
        if grant.panes.is_empty() && !grant.all_panes {
            grant.panes = vec![pane];
        }
        let instance = format!("{}-{}", p.app_id, r.launch_id);
        log::info(format!(
            "apps: `{}` registered from pane %{pane} as {instance}",
            p.app_id
        ));
        Ok(api::Registration {
            instance,
            app_id: p.app_id,
            pane,
            grant: Some(grant),
        })
    }
}

/// The app panes: what `apps.publish`, `apps.view`, and `apps.input` act
/// on, and what the topology shows.
impl Apps {
    /// The next revision; called with `hosted` locked, so revisions grow
    /// in the order changes are made.
    fn bump(&self) -> u64 {
        let mut next = 0;
        self.changed.send_modify(|n| {
            *n += 1;
            next = *n;
        });
        next
    }

    /// The registered app `instance`, holding `grant`, hosts pane `pane`
    /// on connection `conn`. Returns what `apps.input` sends it.
    pub fn host(
        &self,
        pane: u32,
        instance: &str,
        app_id: &str,
        grant: &api::Grant,
        conn: u64,
    ) -> tokio::sync::mpsc::UnboundedReceiver<api::AppInput> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut h) = self.hosted.lock() {
            let revision = self.bump();
            h.insert(
                pane,
                Hosted {
                    instance: instance.to_owned(),
                    app_id: app_id.to_owned(),
                    grant: grant.clone(),
                    conn,
                    live: true,
                    view: String::new(),
                    revision,
                    tree: view_tree::Kept::default(),
                    tree_revision: revision,
                    keys: Some(tx),
                },
            );
        }
        rx
    }

    /// The host on connection `conn` published `view` for its pane.
    pub fn publish(&self, conn: u64, view: String) -> Result<(), RpcError> {
        let mut h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        let Some(entry) = h.values_mut().find(|e| e.conn == conn && e.live) else {
            return Err(forbidden("only a registered app host publishes a view"));
        };
        if entry.view != view {
            entry.view = view;
            entry.revision = self.bump();
        }
        Ok(())
    }

    /// The host on connection `conn` published version `p.seq` of its
    /// pane's tree. A patch that does not follow the version kept is
    /// refused with a precondition error: the host sends the whole tree.
    pub fn publish_tree(&self, conn: u64, p: &api::AppTreePublish) -> Result<(), RpcError> {
        let mut h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        let Some(entry) = h.values_mut().find(|e| e.conn == conn && e.live) else {
            return Err(forbidden("only a registered app host publishes a view"));
        };
        let before = entry.tree.seq();
        let result = entry.tree.publish(p.seq, &p.tree, &p.patch);
        if entry.tree.seq() != before || result.is_ok() {
            entry.tree_revision = self.bump();
        }
        result.map_err(|r| match r {
            view_tree::Refused::Resync(why) => {
                RpcError::new(code::PRECONDITION, format!("send the whole tree: {why}"))
            }
            view_tree::Refused::Invalid(why) => RpcError::new(code::INVALID_PARAMS, why),
        })
    }

    fn tree_now(&self, q: &api::AppTreeQuery) -> Result<api::AppTree, RpcError> {
        let h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        let e = h.get(&q.pane).ok_or_else(|| {
            RpcError::new(
                code::NOT_FOUND,
                format!("pane %{} runs no registered app", q.pane),
            )
        })?;
        let since = e.tree.since(q.seq);
        Ok(api::AppTree {
            pane: q.pane,
            instance: e.instance.clone(),
            app_id: e.app_id.clone(),
            revision: e.tree_revision,
            live: e.live,
            seq: since.seq,
            tree: since.tree,
            patches: since.patches,
        })
    }

    /// The pane's tree for a reader at version `q.seq`, once its revision
    /// is newer than `q.after` or the wait runs out.
    pub async fn tree(&self, q: &api::AppTreeQuery) -> Result<api::AppTree, RpcError> {
        let wait = q.wait_ms.unwrap_or(VIEW_WAIT_MS).min(VIEW_WAIT_MAX_MS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(wait));
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let t = self.tree_now(q)?;
            if t.revision > q.after {
                return Ok(t);
            }
            if !matches!(
                tokio::time::timeout_at(deadline, changes.changed()).await,
                Ok(Ok(()))
            ) {
                return self.tree_now(q);
            }
        }
    }

    /// The app pane `pane`'s app instance and grant: what a desktop drawing
    /// its bound streams reads with (`for_pane`), besides its own grant.
    pub fn app_of(&self, pane: u32) -> Result<(String, api::Grant), RpcError> {
        let h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        h.get(&pane)
            .map(|e| (e.instance.clone(), e.grant.clone()))
            .ok_or_else(|| {
                RpcError::new(
                    code::NOT_FOUND,
                    format!("pane %{pane} runs no registered app"),
                )
            })
    }

    /// Connection `conn` closed: the panes it hosted keep their last view,
    /// no longer live. Whether anything changed.
    pub fn closed(&self, conn: u64) -> bool {
        let Ok(mut h) = self.hosted.lock() else {
            return false;
        };
        if !h.values().any(|e| e.conn == conn && e.live) {
            return false;
        }
        let revision = self.bump();
        for entry in h.values_mut().filter(|e| e.conn == conn && e.live) {
            entry.live = false;
            entry.keys = None;
            entry.revision = revision;
            entry.tree_revision = revision;
        }
        true
    }

    /// Mark the app panes in `t`, and forget stopped hosts whose pane is
    /// gone.
    pub fn annotate(&self, t: &mut api::Topology) {
        let Ok(mut h) = self.hosted.lock() else {
            return;
        };
        h.retain(|pane, e| e.live || t.panes.iter().any(|p| p.id == *pane));
        for p in &mut t.panes {
            p.app = h.get(&p.id).map(|e| api::PaneApp {
                instance: e.instance.clone(),
                app_id: e.app_id.clone(),
                live: e.live,
            });
        }
    }

    fn view_now(&self, pane: u32) -> Result<api::AppView, RpcError> {
        let h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        let e = h.get(&pane).ok_or_else(|| {
            RpcError::new(
                code::NOT_FOUND,
                format!("pane %{pane} runs no registered app"),
            )
        })?;
        Ok(api::AppView {
            pane,
            instance: e.instance.clone(),
            app_id: e.app_id.clone(),
            revision: e.revision,
            live: e.live,
            view: e.view.clone(),
        })
    }

    /// The pane's latest view, once its revision is newer than `after` or
    /// the wait runs out.
    pub async fn view(&self, q: &api::AppViewQuery) -> Result<api::AppView, RpcError> {
        let wait = q.wait_ms.unwrap_or(VIEW_WAIT_MS).min(VIEW_WAIT_MAX_MS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(wait));
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let v = self.view_now(q.pane)?;
            if v.revision > q.after {
                return Ok(v);
            }
            if !matches!(
                tokio::time::timeout_at(deadline, changes.changed()).await,
                Ok(Ok(()))
            ) {
                return self.view_now(q.pane);
            }
        }
    }

    /// Send a key, text, a click, or wheel steps to the pane's host.
    pub fn input(&self, i: &api::AppInput) -> Result<(), RpcError> {
        check_input(i)?;
        let h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        let sent = h
            .get(&i.pane)
            .and_then(|e| e.keys.as_ref())
            .is_some_and(|k| k.send(i.clone()).is_ok());
        if sent {
            Ok(())
        } else {
            Err(RpcError::new(
                code::PRECONDITION,
                format!("pane %{} has no running app host", i.pane),
            ))
        }
    }
}

/// An app input is exactly one of a key name, text, a click, or wheel
/// steps, within bounds.
fn check_input(i: &api::AppInput) -> Result<(), RpcError> {
    let given = usize::from(!i.key.is_empty())
        + usize::from(i.text.is_some())
        + usize::from(i.select.is_some())
        + usize::from(i.scroll.is_some());
    if given != 1 {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            "give exactly one of key, text, select, or scroll",
        ));
    }
    if !i.key.is_empty() && (i.key.len() > MAX_KEY || i.key.chars().any(char::is_control)) {
        return Err(RpcError::new(code::INVALID_PARAMS, "not a key name"));
    }
    if i.text
        .as_ref()
        .is_some_and(|t| t.is_empty() || t.len() > MAX_TEXT)
    {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("text must be 1 to {MAX_TEXT} bytes"),
        ));
    }
    if let Some(n) = &i.node {
        if i.select.is_none() && i.scroll.is_none() {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "node goes with select or scroll",
            ));
        }
        if n.is_empty() || n.len() > MAX_NODE || n.chars().any(char::is_control) {
            return Err(RpcError::new(code::INVALID_PARAMS, "not a node key"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology(panes: &[u32]) -> api::Topology {
        api::Topology {
            panes: panes
                .iter()
                .map(|id| api::Pane {
                    id: *id,
                    ..api::Pane::default()
                })
                .collect(),
            ..api::Topology::default()
        }
    }

    #[tokio::test]
    async fn an_app_pane_keeps_its_last_view_when_its_host_stops() {
        let apps = Apps::default();
        let mut keys = apps.host(7, "local.x-1", "local.x", &api::Grant::default(), 3);
        let mut t = topology(&[7, 8]);
        apps.annotate(&mut t);
        assert_eq!(t.panes[0].app.as_ref().map(|a| a.live), Some(true));
        assert!(t.panes[1].app.is_none());

        // Only the host's own connection publishes.
        assert!(apps.publish(4, "{}".into()).is_err());
        apps.publish(3, r#"{"rows":[1]}"#.into()).expect("publish");
        let q = |after| api::AppViewQuery {
            pane: 7,
            after,
            wait_ms: Some(50),
        };
        let v = apps.view(&q(0)).await.expect("view");
        assert_eq!(v.view, r#"{"rows":[1]}"#);
        assert!(v.live);

        // A waiting reader wakes for the next view.
        let waiting = api::AppViewQuery {
            wait_ms: Some(5_000),
            ..q(v.revision)
        };
        let (next, ()) = tokio::join!(apps.view(&waiting), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            apps.publish(3, r#"{"rows":[2]}"#.into()).expect("publish");
        });
        assert_eq!(next.expect("view").view, r#"{"rows":[2]}"#);

        apps.input(&api::AppInput {
            pane: 7,
            key: "Enter".into(),
            ..api::AppInput::default()
        })
        .expect("input");
        assert_eq!(keys.recv().await.map(|i| i.key).as_deref(), Some("Enter"));
        // Pasted text goes as it is, one input; never with a key too.
        let text = api::AppInput {
            pane: 7,
            text: Some("two words".into()),
            ..api::AppInput::default()
        };
        apps.input(&text).expect("text");
        assert_eq!(keys.recv().await, Some(text));
        // A click on a row of a view tree's node names the node.
        let click = api::AppInput {
            pane: 7,
            select: Some(2),
            node: Some("names".into()),
            ..api::AppInput::default()
        };
        apps.input(&click).expect("click");
        assert_eq!(keys.recv().await, Some(click));
        for bad in [
            api::AppInput {
                pane: 7,
                ..api::AppInput::default()
            },
            api::AppInput {
                pane: 7,
                key: "a".into(),
                select: Some(1),
                ..api::AppInput::default()
            },
            api::AppInput {
                pane: 7,
                text: Some(String::new()),
                ..api::AppInput::default()
            },
            api::AppInput {
                pane: 7,
                key: "\u{1b}[A".into(),
                ..api::AppInput::default()
            },
            // A node key names where a click or wheel steps go, nothing else.
            api::AppInput {
                pane: 7,
                key: "a".into(),
                node: Some("left".into()),
                ..api::AppInput::default()
            },
        ] {
            assert!(apps.input(&bad).is_err(), "{bad:?}");
        }

        // The host's connection closes: the last view stays, not live, and
        // keys reach nothing.
        assert!(apps.closed(3));
        let v = apps.view(&q(0)).await.expect("view");
        assert!(!v.live);
        assert_eq!(v.view, r#"{"rows":[2]}"#);
        let key = api::AppInput {
            pane: 7,
            key: "a".into(),
            ..api::AppInput::default()
        };
        assert!(apps.input(&key).is_err());
        let mut t = topology(&[7]);
        apps.annotate(&mut t);
        assert_eq!(t.panes[0].app.as_ref().map(|a| a.live), Some(false));

        // Its pane closes: the app is forgotten.
        apps.annotate(&mut topology(&[8]));
        assert!(apps.view(&q(0)).await.is_err());
    }

    #[tokio::test]
    async fn a_reader_gets_the_tree_then_only_what_changed() {
        let apps = Apps::default();
        let _keys = apps.host(7, "local.x-1", "local.x", &api::Grant::default(), 3);
        let frame = |cmd: &str| {
            let mut root = view_tree::Node::new(
                "panes",
                view_tree::Kind::Table {
                    columns: vec!["cmd".into()],
                    selected: None,
                },
            );
            root.children = vec![view_tree::Node::new(
                "1",
                view_tree::Kind::Item {
                    cells: vec![cmd.into()],
                },
            )];
            view_tree::Frame {
                title: "X".into(),
                status: String::new(),
                focus: None,
                root,
            }
        };
        let full = |seq, f: &view_tree::Frame| api::AppTreePublish {
            seq,
            tree: serde_json::to_string(f).expect("json"),
            patch: String::new(),
        };
        let patch = |seq, ops: Vec<view_tree::Op>| api::AppTreePublish {
            seq,
            tree: String::new(),
            patch: serde_json::to_string(&ops).expect("json"),
        };
        // Only the host's own connection publishes, and a patch needs a tree.
        assert!(apps.publish_tree(4, &full(1, &frame("sh"))).is_err());
        let early = apps.publish_tree(3, &patch(1, Vec::new()));
        assert_eq!(early.expect_err("no tree").code, code::PRECONDITION);
        apps.publish_tree(3, &full(1, &frame("sh"))).expect("tree");
        let q = |seq, after| api::AppTreeQuery {
            pane: 7,
            after,
            seq,
            wait_ms: Some(50),
        };
        let first = apps.tree(&q(0, 0)).await.expect("tree");
        assert_eq!(first.seq, 1);
        assert!(first.tree.contains("\"sh\""), "{}", first.tree);
        let mut reader = view_tree::Mirror::default();
        reader
            .take(first.seq, &first.tree, &first.patches)
            .expect("take");

        let ops = view_tree::diff(&frame("sh"), &frame("vim"));
        apps.publish_tree(3, &patch(2, ops)).expect("patch");
        let next = apps
            .tree(&q(reader.seq, first.revision))
            .await
            .expect("tree");
        assert!(next.tree.is_empty(), "patches, not the tree");
        assert_eq!(next.patches.len(), 1);
        reader
            .take(next.seq, &next.tree, &next.patches)
            .expect("take");
        assert_eq!(reader.frame, Some(frame("vim")));

        // A patch that skips a version is refused.
        let skipped = apps.publish_tree(3, &patch(9, Vec::new()));
        assert_eq!(skipped.expect_err("skipped").code, code::PRECONDITION);
        // The host stops: the tree stays, not live.
        assert!(apps.closed(3));
        let last = apps.tree(&q(0, next.revision)).await.expect("tree");
        assert!(!last.live);
        assert!(last.tree.contains("vim"));
    }

    #[test]
    fn tickets_are_compared_in_full() {
        assert!(same("abcd", "abcd"));
        assert!(!same("abcd", "abce"));
        assert!(!same("abcd", "abc"));
    }

    #[test]
    fn a_process_descends_from_its_own_ancestors_only() {
        let me = std::process::id();
        let parent = parent_of(me).expect("parent");
        assert!(descends_from(me, me));
        assert!(descends_from(me, parent));
        assert!(!descends_from(parent, me));
    }
}
