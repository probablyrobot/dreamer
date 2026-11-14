//! The message broker (spec 12.3): structured messages between
//! connections, never typed into a pane.
//!
//! Every connection has a broker identity: a registered app's instance,
//! or `owner:<n>` for the user's own clients. The broker stamps it on every
//! envelope it delivers, with the grant context, a sequence, and the causal
//! origin and depth. Whatever a client says about the sender is kept only
//! as `claimed_sender`. Reserved topics belong to the daemon; no client
//! may publish on them.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use api::{MessageEnvelope, MessageSender, RpcError, code};
use tokio::sync::{mpsc, oneshot};

/// Topics only the daemon publishes: broker lifecycle, human input,
/// policy, and approvals.
pub const RESERVED: [&str; 4] = ["dreamer", "human", "policy", "approval"];
/// The largest payload accepted.
pub const MAX_PAYLOAD: usize = 64 << 10;
/// How many causal steps a message may be from its origin.
pub const MAX_DEPTH: u32 = 8;
/// Messages kept per subscription before newer ones are dropped and
/// counted as missed.
const INBOX: usize = 256;
/// Messages remembered for causal depth.
const RECENT: usize = 4096;
const DEFAULT_TIMEOUT_MS: u32 = 10_000;
const MAX_TIMEOUT_MS: u32 = 60_000;

/// A connection's rate: a burst, then this many messages a second.
fn rate() -> (f64, f64) {
    if std::env::var_os("DREAMER_TEST_SMALL_QUEUES").is_some() {
        (20.0, 10.0)
    } else {
        (200.0, 100.0)
    }
}

/// A connection's identity as a sender, when it is not the user: set by
/// `apps.register`, or by the daemon for a supervised extension.
#[derive(Debug, Clone)]
pub struct Identity {
    pub instance: String,
    pub app_id: Option<String>,
    pub pane: Option<u32>,
}

/// Who is sending: decided by the daemon from the connection.
#[derive(Debug, Clone)]
pub struct Who {
    /// The connection, for the rate limit and so a topic message does not
    /// come back to the connection that sent it.
    pub conn: u64,
    pub instance: String,
    pub app_id: Option<String>,
    pub pane: Option<u32>,
    pub owner: bool,
}

impl Who {
    fn stamp(&self) -> MessageSender {
        MessageSender {
            instance: self.instance.clone(),
            app_id: self.app_id.clone(),
            pane: self.pane,
            owner: self.owner,
        }
    }
}

/// A connection's token bucket.
#[derive(Debug)]
pub struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Default for Bucket {
    fn default() -> Self {
        Bucket {
            tokens: rate().0,
            at: Instant::now(),
        }
    }
}

impl Bucket {
    fn take(&mut self) -> bool {
        let (burst, per_second) = rate();
        let now = Instant::now();
        let refill = now.duration_since(self.at).as_secs_f64() * per_second;
        self.tokens = (self.tokens + refill).min(burst);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A delivery, with how many messages this subscription lost before it.
pub type Delivery = (MessageEnvelope, u64);

struct Sub {
    conn: u64,
    instance: String,
    topics: Vec<String>,
    tx: mpsc::Sender<Delivery>,
    missed: Arc<AtomicU64>,
}

struct Pending {
    requester: String,
    recipient: String,
    correlation: Option<String>,
    origin: u64,
    depth: u32,
    tx: oneshot::Sender<MessageEnvelope>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    subs: Vec<Sub>,
    /// id -> (origin, depth), for messages that name a cause.
    recent: HashMap<u64, (u64, u32)>,
    order: VecDeque<u64>,
    pending: HashMap<u64, Pending>,
}

#[derive(Default)]
pub struct Broker {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker").finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn invalid(why: impl Into<String>) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, why)
}

/// Whether `topic` is one of the daemon's.
pub fn reserved(topic: &str) -> bool {
    RESERVED.iter().any(|r| {
        topic == *r
            || topic
                .strip_prefix(r)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn topic_matches(filter: &str, topic: &str) -> bool {
    filter == topic
        || filter == "*"
        || filter
            .strip_suffix(".*")
            .is_some_and(|base| topic.strip_prefix(base).is_some_and(|r| r.starts_with('.')))
}

/// What a new message carries before the broker stamps it.
struct Draft {
    topic: Option<String>,
    recipient: Option<String>,
    schema: u32,
    correlation: Option<String>,
    expires_unix_ms: Option<u64>,
    payload: Vec<u8>,
    cause: Option<u64>,
    claimed_sender: Option<String>,
    request: bool,
}

impl Inner {
    fn lock(m: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&mut self, id: u64, origin: u64, depth: u32) {
        self.recent.insert(id, (origin, depth));
        self.order.push_back(id);
        while self.order.len() > RECENT {
            if let Some(old) = self.order.pop_front() {
                self.recent.remove(&old);
            }
        }
    }

    /// Stamp `d` as sent by `who`, after the checks every message passes.
    fn envelope(&mut self, who: &Who, d: Draft, now: u64) -> Result<MessageEnvelope, RpcError> {
        if d.payload.len() > MAX_PAYLOAD {
            return Err(invalid(format!(
                "the payload is {} bytes; the limit is {MAX_PAYLOAD}",
                d.payload.len()
            )));
        }
        if d.expires_unix_ms.is_some_and(|t| t <= now) {
            return Err(RpcError::new(
                code::PRECONDITION,
                "the message expired before it was sent",
            ));
        }
        let (origin, depth) = match d.cause {
            Some(c) => match self.recent.get(&c) {
                Some(&(origin, depth)) => (origin, depth + 1),
                // A cause the broker no longer remembers starts a chain
                // of its own from there.
                None => (c, 1),
            },
            None => (0, 0),
        };
        if depth > MAX_DEPTH {
            return Err(RpcError::new(
                code::LIMITED,
                format!(
                    "message {} is {depth} steps from its origin; the limit is {MAX_DEPTH}",
                    d.cause.unwrap_or_default()
                ),
            ));
        }
        self.next += 1;
        let id = self.next;
        let origin = if origin == 0 { id } else { origin };
        self.remember(id, origin, depth);
        Ok(MessageEnvelope {
            id,
            sender: Some(who.stamp()),
            topic: d.topic,
            recipient: d.recipient,
            schema: d.schema,
            correlation: d.correlation,
            expires_unix_ms: d.expires_unix_ms,
            payload: d.payload,
            cause: d.cause,
            origin,
            depth,
            claimed_sender: d.claimed_sender,
            time_unix_ms: now,
            request: d.request,
        })
    }

    /// Hand `e` to every subscription in `to`; full inboxes count a miss.
    fn deliver(&mut self, e: &MessageEnvelope, to: impl Fn(&Sub) -> bool) -> u32 {
        let mut delivered = 0;
        self.subs.retain(|s| !s.tx.is_closed());
        for s in self.subs.iter().filter(|s| to(s)) {
            let missed = s.missed.swap(0, Ordering::Relaxed);
            match s.tx.try_send((e.clone(), missed)) {
                Ok(()) => delivered += 1,
                Err(_) => {
                    s.missed.fetch_add(missed + 1, Ordering::Relaxed);
                }
            }
        }
        delivered
    }

    fn receiving(&self, instance: &str) -> bool {
        self.subs
            .iter()
            .any(|s| s.instance == instance && !s.tx.is_closed())
    }
}

impl Broker {
    /// Start receiving: direct messages to `who`, and `topics` (exact,
    /// `service.*`, or `*`).
    pub fn subscribe(&self, who: &Who, topics: Vec<String>) -> mpsc::Receiver<Delivery> {
        let (tx, rx) = mpsc::channel(INBOX);
        Inner::lock(&self.inner).subs.push(Sub {
            conn: who.conn,
            instance: who.instance.clone(),
            topics,
            tx,
            missed: Arc::default(),
        });
        rx
    }

    pub fn publish(
        &self,
        who: &Who,
        bucket: &Mutex<Bucket>,
        p: api::MessagePublish,
    ) -> Result<api::MessageSent, RpcError> {
        let topic = p.topic.filter(|t| !t.is_empty());
        let recipient = p.recipient.filter(|r| !r.is_empty());
        match (&topic, &recipient) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(invalid("give a topic or a recipient, not both"));
            }
            (Some(t), None) if reserved(t) => {
                return Err(RpcError::new(
                    code::FORBIDDEN,
                    format!("`{t}` is a reserved topic; only the daemon publishes on it"),
                ));
            }
            _ => {}
        }
        limit(bucket)?;
        let mut inner = Inner::lock(&self.inner);
        if let Some(r) = &recipient
            && !inner.receiving(r)
        {
            return Err(RpcError::new(
                code::NOT_FOUND,
                format!("no app instance `{r}` is receiving messages"),
            ));
        }
        let e = inner.envelope(
            who,
            Draft {
                topic: topic.clone(),
                recipient: recipient.clone(),
                schema: p.schema,
                correlation: p.correlation,
                expires_unix_ms: p.expires_unix_ms,
                payload: p.payload,
                cause: p.cause,
                claimed_sender: p.sender,
                request: false,
            },
            now_ms(),
        )?;
        let delivered = match (&topic, &recipient) {
            // Never back to the connection that published it.
            (Some(t), _) => inner.deliver(&e, |s| {
                s.conn != who.conn && s.topics.iter().any(|f| topic_matches(f, t))
            }),
            (_, Some(r)) => inner.deliver(&e, |s| s.instance == *r),
            _ => 0,
        };
        Ok(api::MessageSent {
            id: e.id,
            delivered,
            depth: e.depth,
        })
    }

    /// Send a request to one instance and wait for its reply.
    pub async fn request(
        self: &Arc<Self>,
        who: &Who,
        bucket: &Mutex<Bucket>,
        p: api::MessageRequest,
    ) -> Result<MessageEnvelope, RpcError> {
        if p.recipient.is_empty() {
            return Err(invalid("a request needs a recipient"));
        }
        limit(bucket)?;
        let timeout = p
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut inner = Inner::lock(&self.inner);
            if !inner.receiving(&p.recipient) {
                return Err(RpcError::new(
                    code::NOT_FOUND,
                    format!("no app instance `{}` is receiving messages", p.recipient),
                ));
            }
            let e = inner.envelope(
                who,
                Draft {
                    topic: None,
                    recipient: Some(p.recipient.clone()),
                    schema: p.schema,
                    correlation: p.correlation.clone(),
                    expires_unix_ms: Some(now_ms() + u64::from(timeout)),
                    payload: p.payload,
                    cause: p.cause,
                    claimed_sender: p.sender,
                    request: true,
                },
                now_ms(),
            )?;
            inner.pending.insert(
                e.id,
                Pending {
                    requester: who.instance.clone(),
                    recipient: p.recipient.clone(),
                    correlation: p.correlation,
                    origin: e.origin,
                    depth: e.depth,
                    tx,
                },
            );
            inner.deliver(&e, |s| s.instance == p.recipient);
            e.id
        };
        // Forget the request however this ends.
        let _forget = Forget(Arc::downgrade(self), id);
        match tokio::time::timeout(Duration::from_millis(u64::from(timeout)), rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) | Err(_) => Err(RpcError::new(
                code::UNAVAILABLE,
                format!(
                    "`{}` did not reply to message {id} within {timeout} ms",
                    p.recipient
                ),
            )),
        }
    }

    /// Answer a request addressed to `who`.
    pub fn reply(
        &self,
        who: &Who,
        bucket: &Mutex<Bucket>,
        p: api::MessageReply,
    ) -> Result<api::MessageSent, RpcError> {
        limit(bucket)?;
        let mut inner = Inner::lock(&self.inner);
        let Some(pending) = inner.pending.get(&p.request) else {
            return Err(RpcError::new(
                code::NOT_FOUND,
                format!("no request {} is waiting for a reply", p.request),
            ));
        };
        if pending.recipient != who.instance {
            return Err(RpcError::new(
                code::FORBIDDEN,
                format!(
                    "request {} was sent to `{}`; only it can reply",
                    p.request, pending.recipient
                ),
            ));
        }
        let Some(pending) = inner.pending.remove(&p.request) else {
            return Err(RpcError::new(code::INTERNAL, "request vanished"));
        };
        let depth = pending.depth + 1;
        if depth > MAX_DEPTH {
            return Err(RpcError::new(
                code::LIMITED,
                format!("a reply would be {depth} steps from its origin; the limit is {MAX_DEPTH}"),
            ));
        }
        if p.payload.len() > MAX_PAYLOAD {
            return Err(invalid(format!(
                "the payload is {} bytes; the limit is {MAX_PAYLOAD}",
                p.payload.len()
            )));
        }
        inner.next += 1;
        let id = inner.next;
        inner.remember(id, pending.origin, depth);
        let e = MessageEnvelope {
            id,
            sender: Some(who.stamp()),
            recipient: Some(pending.requester),
            schema: p.schema,
            correlation: pending.correlation,
            payload: p.payload,
            cause: Some(p.request),
            origin: pending.origin,
            depth,
            time_unix_ms: now_ms(),
            ..MessageEnvelope::default()
        };
        let delivered = u32::from(pending.tx.send(e).is_ok());
        Ok(api::MessageSent {
            id,
            delivered,
            depth,
        })
    }
}

/// Removes a request from the pending table when its waiter goes away.
struct Forget(Weak<Broker>, u64);

impl Drop for Forget {
    fn drop(&mut self) {
        if let Some(b) = self.0.upgrade() {
            Inner::lock(&b.inner).pending.remove(&self.1);
        }
    }
}

fn limit(bucket: &Mutex<Bucket>) -> Result<(), RpcError> {
    let ok = bucket
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if ok {
        Ok(())
    } else {
        let (burst, per_second) = rate();
        Err(RpcError::new(
            code::LIMITED,
            format!(
                "this connection sends messages faster than {per_second} a second (burst {burst})"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(conn: u64, instance: &str) -> Who {
        Who {
            conn,
            instance: instance.into(),
            app_id: Some("app".into()),
            pane: Some(conn as u32),
            owner: false,
        }
    }

    fn publish(topic: &str, payload: &str) -> api::MessagePublish {
        api::MessagePublish {
            topic: Some(topic.into()),
            payload: payload.as_bytes().to_vec(),
            ..api::MessagePublish::default()
        }
    }

    #[test]
    fn the_broker_stamps_the_sender_and_keeps_a_forged_one_as_a_claim() {
        let b = Broker::default();
        let bucket = Mutex::default();
        let (a, z) = (who(1, "app-a"), who(2, "app-z"));
        let mut rx = b.subscribe(&z, vec!["build.*".into()]);
        let mut p = publish("build.done", "ok");
        p.sender = Some("owner".into());
        let sent = b.publish(&a, &bucket, p).expect("publish");
        assert_eq!(sent.delivered, 1);
        let (e, missed) = rx.try_recv().expect("delivered");
        assert_eq!(missed, 0);
        let s = e.sender.expect("stamped");
        assert_eq!((s.instance.as_str(), s.owner), ("app-a", false));
        assert_eq!(e.claimed_sender.as_deref(), Some("owner"));
        assert_eq!((e.origin, e.depth), (e.id, 0));
    }

    #[test]
    fn reserved_topics_are_refused_and_a_topic_does_not_echo_back() {
        let b = Broker::default();
        let bucket = Mutex::default();
        let a = who(1, "app-a");
        for t in [
            "human.input",
            "approval.granted",
            "policy",
            "dreamer.lifecycle",
        ] {
            let e = b.publish(&a, &bucket, publish(t, "x")).expect_err(t);
            assert_eq!(e.code, code::FORBIDDEN, "{t}");
        }
        assert!(!reserved("humane.x"));
        let mut own = b.subscribe(&a, vec!["*".into()]);
        let sent = b.publish(&a, &bucket, publish("chat", "hi")).expect("chat");
        assert_eq!(sent.delivered, 0);
        assert!(own.try_recv().is_err());
    }

    #[test]
    fn a_chain_deeper_than_the_limit_is_stopped() {
        let b = Broker::default();
        let bucket = Mutex::new(Bucket {
            tokens: 1000.0,
            at: Instant::now(),
        });
        let a = who(1, "app-a");
        let mut cause = None;
        for depth in 0..=MAX_DEPTH {
            let mut p = publish("loop", "x");
            p.cause = cause;
            let sent = b.publish(&a, &bucket, p).expect("within the limit");
            assert_eq!(sent.depth, depth);
            cause = Some(sent.id);
        }
        let mut p = publish("loop", "x");
        p.cause = cause;
        let e = b.publish(&a, &bucket, p).expect_err("too deep");
        assert_eq!(e.code, code::LIMITED);
    }

    #[test]
    fn a_connection_over_its_rate_is_refused() {
        let b = Broker::default();
        let bucket = Mutex::default();
        let a = who(1, "app-a");
        let (burst, _) = rate();
        for _ in 0..burst as usize {
            b.publish(&a, &bucket, publish("t", "x")).expect("burst");
        }
        let e = b.publish(&a, &bucket, publish("t", "x")).expect_err("rate");
        assert_eq!(e.code, code::LIMITED);
    }

    #[tokio::test]
    async fn requests_are_answered_only_by_their_recipient() {
        let b = Arc::new(Broker::default());
        let bucket = Arc::new(Mutex::default());
        let (a, z, other) = (who(1, "app-a"), who(2, "app-z"), who(3, "app-o"));
        let mut inbox = b.subscribe(&z, Vec::new());
        let asker = {
            let (b, a, bucket) = (b.clone(), a.clone(), bucket.clone());
            tokio::spawn(async move {
                b.request(
                    &a,
                    &bucket,
                    api::MessageRequest {
                        recipient: "app-z".into(),
                        payload: b"ping".to_vec(),
                        correlation: Some("c1".into()),
                        ..api::MessageRequest::default()
                    },
                )
                .await
            })
        };
        let (req, _) = inbox.recv().await.expect("request delivered");
        assert!(req.request);
        let e = b
            .reply(
                &other,
                &bucket,
                api::MessageReply {
                    request: req.id,
                    payload: b"forged".to_vec(),
                    ..api::MessageReply::default()
                },
            )
            .expect_err("not the recipient");
        assert_eq!(e.code, code::FORBIDDEN);
        b.reply(
            &z,
            &bucket,
            api::MessageReply {
                request: req.id,
                payload: b"pong".to_vec(),
                ..api::MessageReply::default()
            },
        )
        .expect("reply");
        let reply = asker.await.expect("task").expect("reply arrives");
        assert_eq!(reply.payload, b"pong");
        assert_eq!(reply.sender.expect("sender").instance, "app-z");
        assert_eq!(reply.cause, Some(req.id));
        assert_eq!(reply.correlation.as_deref(), Some("c1"));
        assert_eq!(reply.depth, 1);
    }
}
