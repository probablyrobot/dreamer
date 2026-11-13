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
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> Result<String, RpcError> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| RpcError::new(code::INTERNAL, format!("random source: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compare without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
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
fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut p = pid;
    for _ in 0..64 {
        if p == ancestor {
            return true;
        }
        match parent_of(p) {
            Some(parent) if parent > 1 && parent != p => p = parent,
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

#[cfg(test)]
mod tests {
    use super::*;

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
