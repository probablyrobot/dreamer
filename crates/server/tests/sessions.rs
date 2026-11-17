//! Agent sessions through a real daemon, with the scripted stand-in for
//! `pi --mode rpc` (`crates/providers/testdata/fake-pi`) as the harness, so
//! no model or login is needed: a turn with a tool call and an approval, a
//! cancelled turn, closing that ends the harness, the transcript replayed
//! after a daemon restart and the harness resumed on its own session, and
//! the session's grant kept within its creator's.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::agent_session_event::Kind as K;
use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message as _;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

fn fake_pi() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/testdata/fake-pi")
}

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Option<Child>,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-ss-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cfg")).expect("root");
        std::fs::write(
            root.join("cfg/dreamer.yaml"),
            format!(
                "agents:\n  default: pi\n  pi:\n    command: [/bin/sh, {}]\n",
                fake_pi().display()
            ),
        )
        .expect("settings");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let mut s = Setup {
            root,
            tmux,
            daemon: None,
        };
        s.start();
        s
    }

    fn start(&mut self) {
        let root = &self.root;
        self.daemon = Some(
            Command::new(EXE)
                .args([
                    "--background",
                    "--tmux-socket",
                    self.tmux.socket(),
                    "--tmux",
                ])
                .arg(tmux_program())
                .env("DREAMER_RUNTIME_DIR", root.join("rt"))
                .env("DREAMER_STATE_DIR", root.join("st"))
                .env("DREAMER_DATA_DIR", root.join("data"))
                .env("DREAMER_CONFIG_DIR", root.join("cfg"))
                .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
                // Must never reach the harness.
                .env("OPENAI_API_KEY", "sk-test-must-not-arrive")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("dreamerd"),
        );
    }

    fn daemon_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
    }

    /// Stop the daemon and start a new one on the same directories.
    async fn restart(&mut self) {
        let pid = self.daemon_pid().expect("daemon pid");
        let c = self.client().await;
        let _ = c.call(Request::DaemonShutdown(api::Empty {})).await;
        drop(c);
        // The daemon may be this test's own child (a zombie once it ends).
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let reaped = self
                .daemon
                .as_mut()
                .is_some_and(|d| matches!(d.try_wait(), Ok(Some(_))));
            if reaped || !platform::process_alive(pid) {
                break;
            }
            assert!(Instant::now() < deadline, "the daemon did not stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.daemon = None;
        self.start();
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect_with(&self.root.join("rt/dreamerd.sock"), "test", 1024).await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn harness_log(&self) -> String {
        std::fs::read_to_string(self.root.join("data/pi-profile/fake-pi.log")).unwrap_or_default()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn session_of(r: Response) -> api::AgentSession {
    match r {
        Response::AgentSessionsCreate(s)
        | Response::AgentSessionsSend(s)
        | Response::AgentSessionsCancel(s)
        | Response::AgentSessionsApprove(s)
        | Response::AgentSessionsDeny(s)
        | Response::AgentSessionsResume(s)
        | Response::AgentSessionsClose(s) => s,
        other => panic!("unexpected reply {other:?}"),
    }
}

async fn create(c: &Client, cwd: &Path) -> api::AgentSession {
    session_of(
        c.call(Request::AgentSessionsCreate(api::AgentSessionCreate {
            cwd: Some(cwd.display().to_string()),
            ..api::AgentSessionCreate::default()
        }))
        .await
        .expect("create"),
    )
}

async fn send(c: &Client, id: &str, text: &str) {
    c.call(Request::AgentSessionsSend(api::AgentSessionSend {
        session: id.into(),
        text: text.into(),
    }))
    .await
    .expect("send");
}

/// A connection following one session's events.
struct Follower {
    _client: Client,
    pushed: tokio::sync::mpsc::Receiver<Pushed>,
    seen: Vec<api::AgentSessionEvent>,
}

impl Follower {
    async fn new(setup: &Setup, id: &str) -> Follower {
        let mut c = setup.client().await;
        let pushed = c.pushed().expect("pushed");
        let r = c
            .call(Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                session: id.into(),
                follow: true,
                ..api::AgentSessionEventsQuery::default()
            }))
            .await
            .expect("follow");
        let Response::AgentSessionsEvents(r) = r else {
            panic!("unexpected reply");
        };
        assert!(r.subscription.is_some() && r.events.is_empty());
        Follower {
            _client: c,
            pushed,
            seen: Vec::new(),
        }
    }

    /// Events until one matches `stop` (inclusive).
    async fn until(&mut self, stop: impl Fn(&api::AgentSessionEvent) -> bool) -> Vec<K> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let p = tokio::time::timeout_at(deadline, self.pushed.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out; so far {out:?}"))
                .expect("connection");
            let Pushed::Event { topic, payload, .. } = p else {
                continue;
            };
            if topic != api::topic::AGENT_SESSIONS_EVENTS {
                continue;
            }
            let b = api::AgentSessionEvents::decode(payload.as_slice()).expect("decode");
            for e in b.events {
                if let Some(prev) = self.seen.last() {
                    assert_eq!(e.seq, prev.seq + 1, "events arrive in order, none missing");
                }
                self.seen.push(e.clone());
                out.push(e.kind.clone().expect("kind"));
                if stop(&e) {
                    return out;
                }
            }
        }
    }
}

fn turn_end(e: &api::AgentSessionEvent) -> bool {
    matches!(e.kind, Some(K::TurnEnd(_)))
}

fn stop_reason(k: &[K]) -> api::AgentStopReason {
    match k.last() {
        Some(K::TurnEnd(t)) => api::AgentStopReason::try_from(t.stop_reason).expect("reason"),
        other => panic!("no turn end: {other:?}"),
    }
}

async fn alive_for_a_moment(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while platform::process_alive(pid) {
        if Instant::now() > deadline {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn sessions_stream_turns_approvals_and_cancels_and_replay_after_a_restart() {
    let mut setup = Setup::new();
    let c = setup.client().await;
    let s = create(&c, &setup.root).await;
    assert_eq!(s.state, api::AgentSessionState::Idle as i32);
    assert_eq!(s.harness, "pi");
    let pid = s.pid.expect("a harness process");
    let pi_session = s.harness_session.clone().expect("pi's own session id");
    assert_eq!(
        s.grant.as_ref().map(|g| g.level),
        Some(api::ActLevel::Observe as i32),
        "the default grant observes"
    );

    // A turn with a tool call that waits for an approval.
    let mut f = Follower::new(&setup, &s.id).await;
    send(&c, &s.id, "please use a tool").await;
    let before = f
        .until(|e| matches!(e.kind, Some(K::ApprovalRequest(_))))
        .await;
    assert!(matches!(&before[0], K::Status(st) if st.state == api::AgentSessionState::Idle as i32));
    assert!(before.contains(&K::UserMessage(api::AgentText {
        text: "please use a tool".into()
    })));
    assert!(before.contains(&K::TextDelta(api::AgentText {
        text: "Let me look.".into()
    })));
    assert!(before.iter().any(|k| matches!(k, K::ToolCall(t)
        if t.status == api::AgentToolStatus::Started as i32 && t.name == "bash"
            && t.input.as_deref() == Some(r#"{"command":"ls"}"#))));
    let Some(K::ApprovalRequest(a)) = before.last() else {
        unreachable!()
    };
    let list = c
        .call(Request::AgentSessionsList(api::Empty {}))
        .await
        .expect("list");
    let Response::AgentSessionsList(list) = list else {
        panic!()
    };
    assert_eq!(
        list.sessions[0].state,
        api::AgentSessionState::Waiting as i32
    );
    session_of(
        c.call(Request::AgentSessionsApprove(api::AgentSessionAnswer {
            session: s.id.clone(),
            approval: a.id.clone(),
            option: None,
            for_pane: None,
        }))
        .await
        .expect("approve"),
    );
    let after = f.until(turn_end).await;
    assert!(matches!(&after[0], K::ApprovalAnswer(x) if x.allowed && x.by.starts_with("owner:")));
    assert!(after.iter().any(|k| matches!(k, K::ToolCall(t)
        if t.status == api::AgentToolStatus::Finished as i32 && t.output.as_deref() == Some("file-a"))));
    assert!(after.iter().any(
        |k| matches!(k, K::Usage(u) if u.output_tokens == 5 && u.cost_micro_usd == Some(125))
    ));
    assert_eq!(stop_reason(&after), api::AgentStopReason::EndTurn);
    // The approval is used up.
    assert!(
        c.call(Request::AgentSessionsDeny(api::AgentSessionAnswer {
            session: s.id.clone(),
            approval: a.id.clone(),
            option: None,
            for_pane: None,
        }))
        .await
        .is_err()
    );

    // Another session: a turn cancelled, then closed, which ends its harness.
    let s2 = create(&c, &setup.root).await;
    let pid2 = s2.pid.expect("pid");
    let mut f2 = Follower::new(&setup, &s2.id).await;
    send(&c, &s2.id, "work slowly").await;
    f2.until(|e| matches!(&e.kind, Some(K::TextDelta(t)) if t.text == "Working"))
        .await;
    c.call(Request::AgentSessionsCancel(api::AgentSessionRef {
        session: s2.id.clone(),
    }))
    .await
    .expect("cancel");
    let k = f2.until(turn_end).await;
    assert_eq!(stop_reason(&k), api::AgentStopReason::Cancelled);
    let closed = session_of(
        c.call(Request::AgentSessionsClose(api::AgentSessionRef {
            session: s2.id.clone(),
        }))
        .await
        .expect("close"),
    );
    assert_eq!(closed.state, api::AgentSessionState::Closed as i32);
    assert!(closed.pid.is_none());
    assert!(!alive_for_a_moment(pid2).await, "closing ended the harness");
    f2.until(|e| matches!(&e.kind, Some(K::Status(st)) if st.state == api::AgentSessionState::Closed as i32))
        .await;

    let log = setup.harness_log();
    assert!(log.contains("args:") && log.contains("--mode rpc"), "{log}");
    assert!(
        log.contains("key:none") && !log.contains("key:seen"),
        "{log}"
    );
    assert!(log.contains(&format!("session-env:{}", s.id)), "{log}");

    // Restart the daemon: the harness ends, the sessions stay.
    let transcript = f.seen.clone();
    drop(f);
    setup.restart().await;
    assert!(
        !alive_for_a_moment(pid).await,
        "stopping the daemon ended the harness"
    );
    let c = setup.client().await;
    let Response::AgentSessionsList(list) = c
        .call(Request::AgentSessionsList(api::Empty {}))
        .await
        .expect("list")
    else {
        panic!()
    };
    let state = |id: &str| {
        list.sessions
            .iter()
            .find(|x| x.id == id)
            .map(|x| api::AgentSessionState::try_from(x.state).expect("state"))
    };
    assert_eq!(state(&s.id), Some(api::AgentSessionState::Detached));
    assert_eq!(state(&s2.id), Some(api::AgentSessionState::Closed));

    // The transcript replays: everything seen live, then the detach.
    let Response::AgentSessionsEvents(replay) = c
        .call(Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
            session: s.id.clone(),
            ..api::AgentSessionEventsQuery::default()
        }))
        .await
        .expect("events")
    else {
        panic!()
    };
    assert_eq!(replay.events[..transcript.len()], transcript[..]);
    assert!(matches!(&replay.events.last().and_then(|e| e.kind.clone()),
        Some(K::Status(st)) if st.state == api::AgentSessionState::Detached as i32
            && st.detail.as_deref() == Some("the daemon stopped")));
    for (i, e) in replay.events.iter().enumerate() {
        assert_eq!(e.seq, i as u64 + 1);
    }

    // Sending resumes the harness on pi's own session.
    let mut f = Follower::new(&setup, &s.id).await;
    f.until(|e| e.seq == replay.events.len() as u64).await;
    send(&c, &s.id, "hello again").await;
    let k = f.until(turn_end).await;
    assert!(
        matches!(&k[0], K::Status(st) if st.detail.as_deref() == Some("resumed")
        && st.harness_session.as_deref() == Some(pi_session.as_str()))
    );
    assert!(k.contains(&K::TextDelta(api::AgentText {
        text: "Hello from fake pi.".into()
    })));
    assert!(
        setup.harness_log().contains("--session"),
        "resumed with --session"
    );
    session_of(
        c.call(Request::AgentSessionsClose(api::AgentSessionRef {
            session: s.id.clone(),
        }))
        .await
        .expect("close"),
    );
}

#[tokio::test]
async fn a_session_grant_stays_within_its_creators_and_binds_its_harness() {
    let setup = Setup::new();
    // A narrow caller: sessions and reading, no panes.
    let narrow = setup.client().await;
    narrow
        .call(Request::GrantsNarrow(api::Grant {
            methods: vec!["agent.sessions.*".into(), "terminal.read".into()],
            ..api::Grant::default()
        }))
        .await
        .expect("narrow");
    // The default grant (every pane) is beyond it.
    let e = narrow
        .call(Request::AgentSessionsCreate(api::AgentSessionCreate {
            cwd: Some(setup.root.display().to_string()),
            ..api::AgentSessionCreate::default()
        }))
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("the session's grant"), "{e}");
    // Within it, and stripped of sessions and approvals.
    let s = session_of(
        narrow
            .call(Request::AgentSessionsCreate(api::AgentSessionCreate {
                cwd: Some(setup.root.display().to_string()),
                grant: Some(api::Grant {
                    methods: vec!["terminal.read".into(), "agent.sessions.approve".into()],
                    ..api::Grant::default()
                }),
                ..api::AgentSessionCreate::default()
            }))
            .await
            .expect("create"),
    );
    assert_eq!(
        s.grant.as_ref().map(|g| g.methods.clone()),
        Some(vec!["terminal.read".to_owned()])
    );
    // Another narrow caller does not see it; the owner does.
    let other = setup.client().await;
    other
        .call(Request::GrantsNarrow(api::Grant {
            methods: vec!["agent.sessions.*".into()],
            ..api::Grant::default()
        }))
        .await
        .expect("narrow");
    let e = other
        .call(Request::AgentSessionsClose(api::AgentSessionRef {
            session: s.id.clone(),
        }))
        .await
        .expect_err("not its session");
    assert!(e.to_string().contains("no agent session"), "{e}");
    let owner = setup.client().await;
    let Response::AgentSessionsList(l) = owner
        .call(Request::AgentSessionsList(api::Empty {}))
        .await
        .expect("list")
    else {
        panic!()
    };
    assert!(l.sessions.iter().any(|x| x.id == s.id));

    // The session's ticket gives its grant only to a process its harness
    // started: this test process is not one.
    let ticket = std::fs::read_to_string(setup.root.join("rt/sessions").join(&s.id))
        .expect("the harness's ticket file");
    let e = owner
        .call(Request::AppsRegister(api::RegisterApp {
            launch_id: format!("session:{}", s.id),
            ticket,
        }))
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("harness"), "{e}");
    let e = owner
        .call(Request::AppsRegister(api::RegisterApp {
            launch_id: format!("session:{}", s.id),
            ticket: "0".repeat(64),
        }))
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("wrong session ticket"), "{e}");
    close_session(&owner, &s.id).await;
}

async fn close_session(c: &Client, id: &str) {
    session_of(
        c.call(Request::AgentSessionsClose(api::AgentSessionRef {
            session: id.into(),
        }))
        .await
        .expect("close"),
    );
}

#[tokio::test]
async fn no_prompt_reaches_pi_without_a_login_and_turns_stay_on_the_pinned_provider() {
    let setup = Setup::new();
    let agent_dir = setup.root.join("data/pi-profile");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    // pi reports no login for any provider.
    std::fs::write(agent_dir.join("fake-pi-logins"), "").expect("logins");
    let c = setup.client().await;

    // A session starts without a login, with no automatic network use.
    let s = create(&c, &setup.root).await;
    assert_eq!(s.state, api::AgentSessionState::Idle as i32);
    let log = setup.harness_log();
    assert!(log.contains("start:offline=1"), "{log}");
    assert!(
        log.contains("auth:openai-codex:not_ready:offline=1"),
        "{log}"
    );

    // A prompt is refused before pi is asked anything, saying how to log in.
    let r = c
        .call(Request::AgentSessionsSend(api::AgentSessionSend {
            session: s.id.clone(),
            text: "hello".into(),
        }))
        .await;
    match r {
        Err(client::ClientError::Rpc(e)) => {
            assert_eq!(e.code, api::code::PRECONDITION, "{e}");
            assert!(e.message.contains("auth login openai-codex"), "{e}");
        }
        other => panic!("a prompt without a login was not refused: {other:?}"),
    }
    let log = setup.harness_log();
    assert!(
        !log.contains("turn:"),
        "pi was asked for a model turn:\n{log}"
    );

    // With pi's own login reported, the prompt goes through, and the turn
    // is held to the pinned provider, not the one pi picked by itself.
    std::fs::write(agent_dir.join("fake-pi-logins"), "openai-codex\n").expect("logins");
    let mut f = Follower::new(&setup, &s.id).await;
    send(&c, &s.id, "hello").await;
    let k = f.until(turn_end).await;
    assert_eq!(stop_reason(&k), api::AgentStopReason::EndTurn);
    let log = setup.harness_log();
    assert!(
        log.contains("set_model:openai-codex/openai-codex-small"),
        "{log}"
    );
    assert!(log.contains("turn:openai-codex"), "{log}");
    assert!(
        !log.contains("turn:radius"),
        "a turn reached pi's own default:\n{log}"
    );
    close_session(&c, &s.id).await;
}
