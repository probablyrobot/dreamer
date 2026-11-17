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
    /// The host's connection.
    conn: u64,
    live: bool,
    view: String,
    revision: u64,
    /// Keys for the host, while it is connected.
    keys: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

/// The longest key name `apps.input` takes.
const MAX_KEY: usize = 32;
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

    /// The registered app `instance` hosts pane `pane` on connection
    /// `conn`. Returns the keys `apps.input` sends it.
    pub fn host(
        &self,
        pane: u32,
        instance: &str,
        app_id: &str,
        conn: u64,
    ) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut h) = self.hosted.lock() {
            let revision = self.bump();
            h.insert(
                pane,
                Hosted {
                    instance: instance.to_owned(),
                    app_id: app_id.to_owned(),
                    conn,
                    live: true,
                    view: String::new(),
                    revision,
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

    /// Send a key to the pane's host.
    pub fn input(&self, i: &api::AppInput) -> Result<(), RpcError> {
        if i.key.is_empty() || i.key.len() > MAX_KEY || i.key.chars().any(char::is_control) {
            return Err(RpcError::new(code::INVALID_PARAMS, "not a key name"));
        }
        let h = self
            .hosted
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "app registry poisoned"))?;
        let sent = h
            .get(&i.pane)
            .and_then(|e| e.keys.as_ref())
            .is_some_and(|k| k.send(i.key.clone()).is_ok());
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
        let mut keys = apps.host(7, "local.x-1", "local.x", 3);
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
        })
        .expect("input");
        assert_eq!(keys.recv().await.as_deref(), Some("Enter"));

        // The host's connection closes: the last view stays, not live, and
        // keys reach nothing.
        assert!(apps.closed(3));
        let v = apps.view(&q(0)).await.expect("view");
        assert!(!v.live);
        assert_eq!(v.view, r#"{"rows":[2]}"#);
        let key = api::AppInput {
            pane: 7,
            key: "a".into(),
        };
        assert!(apps.input(&key).is_err());
        let mut t = topology(&[7]);
        apps.annotate(&mut t);
        assert_eq!(t.panes[0].app.as_ref().map(|a| a.live), Some(false));

        // Its pane closes: the app is forgotten.
        apps.annotate(&mut topology(&[8]));
        assert!(apps.view(&q(0)).await.is_err());
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
