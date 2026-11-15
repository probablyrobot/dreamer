//! Interaction leases (spec 12.4): automation that types into a pane holds
//! an expiring lease, and every automated input carries it. The lease is
//! checked when the input is about to be sent, under the pane's input
//! lane, so input queued behind a person's keystroke fails rather than
//! landing after it. A person typing into the pane revokes the lease's
//! generation; bytes already sent cannot be taken back.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use api::RpcError;
use api::code;

/// Who is calling: the connection, and whether it is the user's own
/// client (the owner grant) rather than an app, extension, or agent.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub conn: u64,
    pub owner: bool,
}

tokio::task_local! {
    /// Set by the connection around each request it answers.
    pub static CALLER: Caller;
}

/// The caller of the request being answered, if a connection set one.
pub fn caller() -> Option<Caller> {
    CALLER.try_with(|c| *c).ok()
}

const DEFAULT_TTL: Duration = Duration::from_secs(30);
const MAX_TTL: Duration = Duration::from_secs(600);
/// Ended leases remembered for a precise refusal.
const REMEMBERED: usize = 256;

#[derive(Debug)]
struct Held {
    id: String,
    conn: u64,
    generation: u64,
    ttl: Duration,
    expires: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    held: HashMap<u32, Held>,
    generations: HashMap<u32, u64>,
    /// Why a lease ended, by id, oldest first.
    ended: Vec<(String, &'static str)>,
}

impl Inner {
    fn end(&mut self, id: String, why: &'static str) {
        if self.ended.len() >= REMEMBERED {
            self.ended.remove(0);
        }
        self.ended.push((id, why));
    }

    /// The pane's lease, dropping it if it has expired.
    fn live(&mut self, pane: u32, now: Instant) -> Option<&mut Held> {
        if self.held.get(&pane).is_some_and(|h| h.expires <= now) {
            let h = self.held.remove(&pane).expect("held");
            self.end(h.id, "it expired");
        }
        self.held.get_mut(&pane)
    }
}

#[derive(Debug, Default)]
pub struct Leases {
    inner: Mutex<Inner>,
}

fn unix_ms(at: Instant) -> u64 {
    let left = at.saturating_duration_since(Instant::now());
    std::time::SystemTime::now()
        .checked_add(left)
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn precondition(why: String) -> RpcError {
    RpcError::new(code::PRECONDITION, why)
}

impl Leases {
    pub fn acquire(
        &self,
        pane: u32,
        conn: u64,
        ttl_ms: Option<u32>,
    ) -> Result<api::Lease, RpcError> {
        let ttl = ttl_ms
            .map(|ms| Duration::from_millis(u64::from(ms)))
            .unwrap_or(DEFAULT_TTL);
        if ttl.is_zero() || ttl > MAX_TTL {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "a lease lasts between 1 ms and 600000 ms",
            ));
        }
        let now = Instant::now();
        let mut g = self.inner.lock().map_err(|_| poisoned())?;
        if let Some(h) = g.live(pane, now) {
            if h.conn != conn {
                return Err(precondition(format!(
                    "pane %{pane} is leased to other automation for up to {} ms more",
                    h.expires.saturating_duration_since(now).as_millis()
                )));
            }
            // The holder asking again just extends its lease.
            h.ttl = ttl;
            h.expires = now + ttl;
            return Ok(api::Lease {
                id: h.id.clone(),
                pane,
                generation: h.generation,
                expires_unix_ms: unix_ms(h.expires),
            });
        }
        let generation = {
            let n = g.generations.entry(pane).or_insert(0);
            *n += 1;
            *n
        };
        let id = format!("{pane}.{generation}");
        g.held.insert(
            pane,
            Held {
                id: id.clone(),
                conn,
                generation,
                ttl,
                expires: now + ttl,
            },
        );
        Ok(api::Lease {
            id,
            pane,
            generation,
            expires_unix_ms: unix_ms(now + ttl),
        })
    }

    pub fn release(&self, id: &str, conn: u64) -> Result<(), RpcError> {
        let mut g = self.inner.lock().map_err(|_| poisoned())?;
        let pane = g
            .held
            .iter()
            .find(|(_, h)| h.id == id && h.conn == conn)
            .map(|(p, _)| *p);
        match pane {
            Some(p) => {
                g.held.remove(&p);
                g.end(id.to_owned(), "it was released");
                Ok(())
            }
            None => Err(RpcError::new(
                code::NOT_FOUND,
                format!("this connection holds no lease {id}"),
            )),
        }
    }

    /// A connection closed: its leases end with it.
    pub fn drop_conn(&self, conn: u64) {
        if let Ok(mut g) = self.inner.lock() {
            let gone: Vec<u32> = g
                .held
                .iter()
                .filter(|(_, h)| h.conn == conn)
                .map(|(p, _)| *p)
                .collect();
            for p in gone {
                if let Some(h) = g.held.remove(&p) {
                    g.end(h.id, "its connection closed");
                }
            }
        }
    }

    /// May this input be sent to `pane` now? Call it under the pane's
    /// input lane, right before sending. `typed` is true for keys, text,
    /// and pastes (not focus or mouse).
    ///
    /// - With a lease: it must be this caller's live lease on this pane;
    ///   using it starts its idle time again.
    /// - Without one, from the user's own client: a person typing revokes
    ///   any lease on the pane.
    /// - Without one, from anyone else: refused while the pane is leased.
    pub fn admit(
        &self,
        pane: u32,
        lease: Option<&str>,
        caller: Option<Caller>,
        typed: bool,
    ) -> Result<(), RpcError> {
        let now = Instant::now();
        let mut g = self.inner.lock().map_err(|_| poisoned())?;
        match lease {
            Some(id) => {
                let conn = caller.map(|c| c.conn);
                match g.live(pane, now) {
                    Some(h) if h.id == id && Some(h.conn) == conn => {
                        h.expires = now + h.ttl;
                        Ok(())
                    }
                    _ => {
                        let why = g
                            .ended
                            .iter()
                            .rev()
                            .find(|(e, _)| e == id)
                            .map(|(_, why)| *why);
                        Err(precondition(match why {
                            Some(why) => format!(
                                "lease {id} on pane %{pane} has ended because {why}; the input was not sent"
                            ),
                            None => format!(
                                "lease {id} is not this connection's lease on pane %{pane}; the input was not sent"
                            ),
                        }))
                    }
                }
            }
            None if caller.is_none_or(|c| c.owner) => {
                if typed && let Some(h) = g.held.remove(&pane) {
                    g.end(h.id, "a person typed into the pane");
                }
                Ok(())
            }
            None => match g.live(pane, now) {
                Some(h) => Err(precondition(format!(
                    "pane %{pane} is leased to automation (lease {}); input without that lease is refused",
                    h.id
                ))),
                None => Ok(()),
            },
        }
    }
}

fn poisoned() -> RpcError {
    RpcError::new(code::INTERNAL, "lease table poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: Caller = Caller {
        conn: 7,
        owner: true,
    };
    const PERSON: Caller = Caller {
        conn: 1,
        owner: true,
    };
    const APP: Caller = Caller {
        conn: 9,
        owner: false,
    };

    #[test]
    fn a_person_typing_revokes_the_lease_and_later_automation_fails() {
        let l = Leases::default();
        let lease = l.acquire(3, AGENT.conn, None).expect("acquire");
        assert_eq!(lease.generation, 1);
        l.admit(3, Some(&lease.id), Some(AGENT), true)
            .expect("held");
        // An app without the lease is refused while it is held.
        let e = l.admit(3, None, Some(APP), true).expect_err("leased");
        assert!(e.message.contains("leased to automation"), "{e}");
        // Focus from the person's window is not typing.
        l.admit(3, None, Some(PERSON), false).expect("focus");
        l.admit(3, Some(&lease.id), Some(AGENT), true)
            .expect("still held");
        l.admit(3, None, Some(PERSON), true).expect("person");
        let e = l
            .admit(3, Some(&lease.id), Some(AGENT), true)
            .expect_err("revoked");
        assert!(e.message.contains("a person typed into the pane"), "{e}");
        // A new lease is a new generation; the old one stays dead.
        let again = l.acquire(3, AGENT.conn, None).expect("again");
        assert_eq!(again.generation, 2);
        assert!(l.admit(3, Some(&lease.id), Some(AGENT), true).is_err());
        l.admit(3, Some(&again.id), Some(AGENT), true).expect("new");
    }

    #[test]
    fn one_holder_at_a_time_and_leases_end_with_expiry_release_or_disconnect() {
        let l = Leases::default();
        let a = l.acquire(4, AGENT.conn, Some(30)).expect("acquire");
        assert!(l.acquire(4, APP.conn, None).is_err(), "held by another");
        assert!(
            l.admit(4, Some(&a.id), Some(APP), true).is_err(),
            "another connection cannot use it"
        );
        std::thread::sleep(Duration::from_millis(60));
        let e = l
            .admit(4, Some(&a.id), Some(AGENT), true)
            .expect_err("expired");
        assert!(e.message.contains("expired"), "{e}");

        let b = l.acquire(4, APP.conn, None).expect("free again");
        l.release(&b.id, APP.conn).expect("release");
        assert!(l.admit(4, Some(&b.id), Some(APP), true).is_err());

        let c = l.acquire(5, AGENT.conn, None).expect("acquire");
        l.drop_conn(AGENT.conn);
        let e = l
            .admit(5, Some(&c.id), Some(AGENT), true)
            .expect_err("gone");
        assert!(e.message.contains("connection closed"), "{e}");
        assert!(l.acquire(4, 0, Some(0)).is_err(), "zero ttl");
    }
}
