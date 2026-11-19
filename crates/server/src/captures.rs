//! Images of app panes, made by the user's desktop (`dream app preview
//! --png`).
//!
//! The daemon draws nothing. A desktop process that can capture its
//! windows waits for work with `apps.capture_wait` on a connection of its
//! own, and answers each request with `apps.capture_done`. `apps.capture`
//! hands a request to every desktop waiting now, and returns the first
//! image one of them wrote; a desktop that does not show the pane answers
//! with why, and the caller gets those reasons when no desktop wrote one.
//! With no desktop connected, it is refused at once.
//!
//! The `web.*` methods that read and drive a page (`web.read`,
//! `web.screenshot`, `web.click`, `web.type`) travel the same way: the
//! request names the method and carries its parameters, and the desktop
//! showing the pane's page runs it there and answers with its result.
//!
//! A desktop also reports on that connection whether it can draw web pages
//! (`web.component`: Dreamer Web ready, missing, being added, failed, or
//! turned off), kept until the connection closes. A `web.open` that finds
//! it missing waits on these reports ([`Captures::web_verdict`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use api::{RpcError, code};
use tokio::sync::{oneshot, watch};

/// What `apps.capture` says when no desktop answers captures.
pub const NO_DESKTOP: &str = "no Dreamer window is running; --png needs one";
/// What the `web.*` methods say when no desktop answers them.
pub const NO_PAGE: &str =
    "no Dreamer window is running: a page is read and driven in the desktop showing it";
/// How long a desktop counts as present after its last wait ended: it asks
/// again at once, so this only covers the gap between two waits.
const PRESENT: Duration = Duration::from_secs(5);
const WAIT_MS: u32 = 20_000;
const CAPTURE_MS: u32 = 10_000;
const MAX_MS: u32 = 60_000;

#[derive(Debug, Default)]
struct Server {
    queue: VecDeque<api::AppCaptureRequest>,
    waiting: u32,
    seen: Option<Instant>,
    /// What it last reported of Dreamer Web.
    web: Option<api::WebComponentState>,
}

/// What the desktops that report Dreamer Web say of it, for a `web.open`
/// that would wait until a page can load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebVerdict {
    /// A desktop has it, or none says it is missing (none running, or
    /// none that reports): go on, as the page will load where it can.
    Go,
    /// It is being added, or offered to be: wait for it.
    Wait,
    /// Every desktop that reports it says it will not be there, and why.
    No(String),
}

impl Server {
    fn present(&self) -> bool {
        self.waiting > 0 || self.seen.is_some_and(|s| s.elapsed() < PRESENT)
    }
}

#[derive(Debug)]
struct Pending {
    asked: HashSet<u64>,
    errors: Vec<String>,
    tx: oneshot::Sender<Result<api::AppCaptureDone, String>>,
}

#[derive(Debug, Default)]
struct Inner {
    next: u64,
    /// Desktops by connection.
    servers: HashMap<u64, Server>,
    pending: HashMap<u64, Pending>,
}

#[derive(Debug)]
pub struct Captures {
    inner: Mutex<Inner>,
    changed: watch::Sender<u64>,
}

impl Default for Captures {
    fn default() -> Self {
        Captures {
            inner: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }
}

fn poisoned() -> RpcError {
    RpcError::new(code::INTERNAL, "capture registry poisoned")
}

fn absolute(path: &str) -> Result<(), RpcError> {
    if std::path::Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(RpcError::new(
            code::INVALID_PARAMS,
            "the image's path must be absolute",
        ))
    }
}

/// The answer once every desktop asked said why it wrote nothing.
fn refused(errors: &[String]) -> String {
    let mut seen = Vec::new();
    for e in errors {
        if !seen.contains(e) {
            seen.push(e.clone());
        }
    }
    if seen.is_empty() {
        "the desktop closed before it answered".to_owned()
    } else {
        seen.join("; ")
    }
}

/// Counts a desktop's wait while it lasts, whatever ends it.
struct Waiting<'a> {
    captures: &'a Captures,
    conn: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Ok(mut i) = self.captures.inner.lock()
            && let Some(s) = i.servers.get_mut(&self.conn)
        {
            s.waiting = s.waiting.saturating_sub(1);
            s.seen = Some(Instant::now());
        }
    }
}

impl Captures {
    /// Desktops connected now that answer captures.
    pub fn desktops(&self) -> u32 {
        self.inner
            .lock()
            .map(|i| i.servers.values().filter(|s| s.present()).count() as u32)
            .unwrap_or(0)
    }

    fn wake(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// Ask every desktop present for an image of `q.pane`; the first one
    /// written, or why none was.
    pub async fn capture(&self, q: &api::AppCapture) -> Result<api::AppCaptured, RpcError> {
        absolute(&q.path)?;
        let request = api::AppCaptureRequest {
            pane: q.pane,
            path: q.path.clone(),
            seq: q.seq,
            ..api::AppCaptureRequest::default()
        };
        let done = self.ask(request, q.wait_ms, NO_DESKTOP).await?;
        Ok(api::AppCaptured {
            path: q.path.clone(),
            width: done.width,
            height: done.height,
        })
    }

    /// Run web method `request` (one of [`api::INSPECT_METHODS`]) on the
    /// page of pane `pane`, in the desktop showing it; its result.
    pub async fn web(
        &self,
        pane: u32,
        request: &api::Request,
        wait_ms: Option<u32>,
    ) -> Result<api::Response, RpcError> {
        if let api::Request::WebScreenshot(s) = request {
            absolute(&s.path)?;
        }
        let method = request.method();
        let asked = api::AppCaptureRequest {
            pane,
            method: method.to_owned(),
            params: request.to_json().to_string(),
            ..api::AppCaptureRequest::default()
        };
        let done = self.ask(asked, wait_ms, NO_PAGE).await?;
        let bad = |e: String| {
            RpcError::new(
                code::INTERNAL,
                format!("the desktop's answer to {method}: {e}"),
            )
        };
        let value = serde_json::from_str(&done.result).map_err(|e| bad(e.to_string()))?;
        api::Response::from_json(method, value).map_err(|e| bad(e.to_string()))
    }

    /// Hand `request` to every desktop present; the first answer that is
    /// not a refusal, or the refusals. With no desktop present, `none`.
    async fn ask(
        &self,
        request: api::AppCaptureRequest,
        wait_ms: Option<u32>,
        none: &str,
    ) -> Result<api::AppCaptureDone, RpcError> {
        let (id, rx) = {
            let mut i = self.inner.lock().map_err(|_| poisoned())?;
            let asked: HashSet<u64> = i
                .servers
                .iter()
                .filter(|(_, s)| s.present())
                .map(|(c, _)| *c)
                .collect();
            if asked.is_empty() {
                return Err(RpcError::new(code::UNAVAILABLE, none));
            }
            i.next += 1;
            let id = i.next;
            let request = api::AppCaptureRequest { id, ..request };
            for c in &asked {
                if let Some(s) = i.servers.get_mut(c) {
                    s.queue.push_back(request.clone());
                }
            }
            let (tx, rx) = oneshot::channel();
            i.pending.insert(
                id,
                Pending {
                    asked,
                    errors: Vec::new(),
                    tx,
                },
            );
            (id, rx)
        };
        self.wake();
        let wait = Duration::from_millis(u64::from(wait_ms.unwrap_or(CAPTURE_MS).min(MAX_MS)));
        let answer = tokio::time::timeout(wait, rx).await;
        if let Ok(mut i) = self.inner.lock() {
            i.pending.remove(&id);
            for s in i.servers.values_mut() {
                s.queue.retain(|r| r.id != id);
            }
        }
        match answer {
            Ok(Ok(Ok(done))) => Ok(done),
            Ok(Ok(Err(why))) => Err(RpcError::new(code::PRECONDITION, why)),
            Ok(Err(_)) => Err(RpcError::new(
                code::UNAVAILABLE,
                "the desktop closed before it answered",
            )),
            Err(_) => Err(RpcError::new(
                code::UNAVAILABLE,
                format!("the desktop did not answer within {} s", wait.as_secs()),
            )),
        }
    }

    /// The desktop on connection `conn` waits for its next request: one
    /// queued for it, or id 0 when the wait runs out.
    pub async fn wait(
        &self,
        conn: u64,
        q: &api::AppCaptureWait,
    ) -> Result<api::AppCaptureRequest, RpcError> {
        let wait = Duration::from_millis(u64::from(q.wait_ms.unwrap_or(WAIT_MS).min(MAX_MS)));
        let deadline = tokio::time::Instant::now() + wait;
        let mut changes = self.changed.subscribe();
        {
            let mut i = self.inner.lock().map_err(|_| poisoned())?;
            i.servers.entry(conn).or_default().waiting += 1;
        }
        let _waiting = Waiting {
            captures: self,
            conn,
        };
        loop {
            changes.borrow_and_update();
            let next = {
                let mut i = self.inner.lock().map_err(|_| poisoned())?;
                i.servers.get_mut(&conn).and_then(|s| s.queue.pop_front())
            };
            if let Some(r) = next {
                return Ok(r);
            }
            if !matches!(
                tokio::time::timeout_at(deadline, changes.changed()).await,
                Ok(Ok(()))
            ) {
                return Ok(api::AppCaptureRequest::default());
            }
        }
    }

    /// The desktop on `conn` answers request `d.id`.
    pub fn done(&self, conn: u64, d: &api::AppCaptureDone) -> Result<(), RpcError> {
        let mut i = self.inner.lock().map_err(|_| poisoned())?;
        let Some(p) = i.pending.get_mut(&d.id) else {
            // Answered already, or the caller gave up.
            return Ok(());
        };
        if !p.asked.remove(&conn) {
            return Err(RpcError::new(
                code::FORBIDDEN,
                "this connection was not asked for that capture",
            ));
        }
        match &d.error {
            None => {
                if let Some(p) = i.pending.remove(&d.id) {
                    let _ = p.tx.send(Ok(d.clone()));
                }
            }
            Some(why) => {
                p.errors.push(why.clone());
                if p.asked.is_empty()
                    && let Some(p) = i.pending.remove(&d.id)
                {
                    let _ = p.tx.send(Err(refused(&p.errors)));
                }
            }
        }
        Ok(())
    }

    /// The desktop on `conn` reports Dreamer Web's status there.
    pub fn report_web(&self, conn: u64, state: api::WebComponentState) -> Result<(), RpcError> {
        {
            let mut i = self.inner.lock().map_err(|_| poisoned())?;
            i.servers.entry(conn).or_default().web = Some(state);
        }
        self.wake();
        Ok(())
    }

    /// What the desktops' reports of Dreamer Web come to (see
    /// [`WebVerdict`]).
    pub fn web_verdict(&self) -> WebVerdict {
        use api::WebComponentStatus as S;
        let Ok(i) = self.inner.lock() else {
            return WebVerdict::Go;
        };
        let mut wait = false;
        let mut why: Vec<String> = Vec::new();
        for s in i.servers.values().filter_map(|s| s.web.as_ref()) {
            match S::try_from(s.status).unwrap_or(S::Unspecified) {
                S::Ready => return WebVerdict::Go,
                S::Missing | S::Adding => wait = true,
                status @ (S::Failed | S::Off) => {
                    let said = s.detail.clone().unwrap_or_else(|| {
                        if status == S::Off {
                            "it is turned off (web.component: never)".into()
                        } else {
                            "it could not be added".into()
                        }
                    });
                    if !why.contains(&said) {
                        why.push(said);
                    }
                }
                S::Unspecified => {}
            }
        }
        match (wait, why.is_empty()) {
            (true, _) => WebVerdict::Wait,
            (false, true) => WebVerdict::Go,
            (false, false) => WebVerdict::No(why.join("; ")),
        }
    }

    /// Changes to the desktops and their reports, to wait on.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Connection `conn` closed: it answers no more captures.
    pub fn drop_conn(&self, conn: u64) {
        let Ok(mut i) = self.inner.lock() else {
            return;
        };
        let Some(gone) = i.servers.remove(&conn) else {
            return;
        };
        if gone.web.is_some() {
            // A web.open waiting on its report decides again.
            self.wake();
        }
        let ended: Vec<u64> = i
            .pending
            .iter_mut()
            .filter_map(|(id, p)| (p.asked.remove(&conn) && p.asked.is_empty()).then_some(*id))
            .collect();
        for id in ended {
            if let Some(p) = i.pending.remove(&id) {
                let _ = p.tx.send(Err(refused(&p.errors)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn capture(pane: u32) -> api::AppCapture {
        api::AppCapture {
            pane,
            path: "/tmp/preview.png".into(),
            seq: 3,
            wait_ms: Some(5_000),
        }
    }

    #[tokio::test]
    async fn with_no_desktop_a_capture_is_refused_at_once() {
        let c = Captures::default();
        assert_eq!(c.desktops(), 0);
        let e = c.capture(&capture(1)).await.expect_err("no desktop");
        assert_eq!(e.message, NO_DESKTOP);
        let mut relative = capture(1);
        relative.path = "preview.png".into();
        assert!(c.capture(&relative).await.is_err());
    }

    #[tokio::test]
    async fn a_waiting_desktop_gets_the_request_and_its_answer_returns() {
        let c = Arc::new(Captures::default());
        let desktop = {
            let c = c.clone();
            tokio::spawn(async move {
                let r = c
                    .wait(
                        7,
                        &api::AppCaptureWait {
                            wait_ms: Some(5_000),
                        },
                    )
                    .await
                    .expect("request");
                assert_eq!((r.pane, r.seq), (4, 3));
                c.done(
                    7,
                    &api::AppCaptureDone {
                        id: r.id,
                        error: None,
                        width: 640,
                        height: 400,
                        ..Default::default()
                    },
                )
                .expect("done");
            })
        };
        while c.desktops() == 0 {
            tokio::task::yield_now().await;
        }
        let got = c.capture(&capture(4)).await.expect("captured");
        assert_eq!((got.width, got.height), (640, 400));
        assert_eq!(got.path, "/tmp/preview.png");
        desktop.await.expect("desktop");
        // Another connection cannot answer for it.
        assert!(c.done(9, &api::AppCaptureDone::default()).is_ok());
    }

    #[tokio::test]
    async fn a_web_method_reaches_the_desktop_and_its_result_comes_back() {
        let c = Arc::new(Captures::default());
        let read = api::Request::WebRead(api::WebRead {
            pane: 6,
            tree: true,
            wait_ms: Some(5_000),
        });
        assert_eq!(
            c.web(6, &read, Some(5_000))
                .await
                .expect_err("none")
                .message,
            NO_PAGE
        );
        let desktop = {
            let c = c.clone();
            tokio::spawn(async move {
                let r = c
                    .wait(
                        7,
                        &api::AppCaptureWait {
                            wait_ms: Some(5_000),
                        },
                    )
                    .await
                    .expect("request");
                assert_eq!((r.pane, r.method.as_str()), (6, "web.read"));
                let asked = api::Request::from_json(
                    &r.method,
                    serde_json::from_str(&r.params).expect("json"),
                )
                .expect("params");
                assert!(matches!(asked, api::Request::WebRead(p) if p.tree));
                let page = api::Response::WebRead(api::WebPage {
                    title: "form".into(),
                    text: "Name".into(),
                    ..api::WebPage::default()
                });
                c.done(
                    7,
                    &api::AppCaptureDone {
                        id: r.id,
                        result: page.to_json().to_string(),
                        ..Default::default()
                    },
                )
                .expect("done");
            })
        };
        while c.desktops() == 0 {
            tokio::task::yield_now().await;
        }
        let got = c.web(6, &read, Some(5_000)).await.expect("read");
        let api::Response::WebRead(page) = got else {
            panic!("{got:?}");
        };
        assert_eq!((page.title.as_str(), page.text.as_str()), ("form", "Name"));
        desktop.await.expect("desktop");
        // A screenshot's path must be absolute before any desktop is asked.
        let relative = api::Request::WebScreenshot(api::WebScreenshot {
            pane: 6,
            path: "shot.png".into(),
            wait_ms: None,
        });
        assert!(c.web(6, &relative, None).await.is_err());
    }

    #[tokio::test]
    async fn when_every_desktop_declines_the_caller_hears_why() {
        let c = Arc::new(Captures::default());
        for conn in [1, 2] {
            let c = c.clone();
            tokio::spawn(async move {
                let r = c
                    .wait(
                        conn,
                        &api::AppCaptureWait {
                            wait_ms: Some(5_000),
                        },
                    )
                    .await
                    .expect("request");
                c.done(
                    conn,
                    &api::AppCaptureDone {
                        id: r.id,
                        error: Some(format!("pane %{} is not shown in this window", r.pane)),
                        ..Default::default()
                    },
                )
                .expect("done");
            });
        }
        while c.desktops() < 2 {
            tokio::task::yield_now().await;
        }
        let e = c.capture(&capture(5)).await.expect_err("declined");
        assert_eq!(e.message, "pane %5 is not shown in this window");
    }

    fn web(status: api::WebComponentStatus, detail: Option<&str>) -> api::WebComponentState {
        api::WebComponentState {
            status: status as i32,
            detail: detail.map(str::to_owned),
        }
    }

    #[test]
    fn web_open_waits_while_a_desktop_adds_dreamer_web_and_fails_only_when_none_will() {
        use api::WebComponentStatus as S;
        let c = Captures::default();
        // No desktop reports: the page loads wherever it can, as before.
        assert_eq!(c.web_verdict(), WebVerdict::Go);
        c.report_web(1, web(S::Missing, None)).expect("report");
        assert_eq!(c.web_verdict(), WebVerdict::Wait);
        c.report_web(1, web(S::Adding, None)).expect("report");
        assert_eq!(c.web_verdict(), WebVerdict::Wait);
        // Another desktop that will never add it does not end the wait.
        c.report_web(2, web(S::Off, None)).expect("report");
        assert_eq!(c.web_verdict(), WebVerdict::Wait);
        c.report_web(1, web(S::Failed, Some("the download failed")))
            .expect("report");
        let WebVerdict::No(why) = c.web_verdict() else {
            panic!("every desktop said no: {:?}", c.web_verdict());
        };
        assert!(why.contains("the download failed"), "{why}");
        assert!(why.contains("web.component: never"), "{why}");
        // One that has it is enough.
        c.report_web(1, web(S::Ready, None)).expect("report");
        assert_eq!(c.web_verdict(), WebVerdict::Go);
        // A desktop that goes takes its report with it.
        c.drop_conn(1);
        assert!(matches!(c.web_verdict(), WebVerdict::No(_)));
        c.drop_conn(2);
        assert_eq!(c.web_verdict(), WebVerdict::Go);
    }

    #[tokio::test]
    async fn a_report_wakes_whoever_waits_on_it() {
        let c = Captures::default();
        let mut changes = c.changes();
        changes.borrow_and_update();
        c.report_web(3, web(api::WebComponentStatus::Ready, None))
            .expect("report");
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("woken")
            .expect("open");
        changes.borrow_and_update();
        c.drop_conn(3);
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("woken by the close")
            .expect("open");
    }

    #[tokio::test]
    async fn a_desktop_that_closes_ends_the_capture_and_is_gone() {
        let c = Arc::new(Captures::default());
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move {
                c.wait(
                    3,
                    &api::AppCaptureWait {
                        wait_ms: Some(5_000),
                    },
                )
                .await
            })
        };
        while c.desktops() == 0 {
            tokio::task::yield_now().await;
        }
        let asked = {
            let c = c.clone();
            tokio::spawn(async move { c.capture(&capture(2)).await })
        };
        let r = waiter.await.expect("join").expect("request");
        assert_ne!(r.id, 0);
        c.drop_conn(3);
        let e = asked.await.expect("join").expect_err("closed");
        assert!(e.message.contains("closed"), "{}", e.message);
        assert_eq!(c.desktops(), 0);
    }
}
