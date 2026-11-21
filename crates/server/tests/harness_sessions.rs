//! Agent sessions over Claude Code, Codex, and an ACP agent, through a real
//! daemon, with scripted stand-ins for each harness
//! (`crates/providers/testdata/fake-claude`, `fake-codex`, `fake-acp`) so no
//! model or login is needed. For each: a multi-turn conversation, a tool
//! call with its approval allowed and then denied, a cancelled turn, the
//! harness resumed on its own session after a daemon restart, no API key
//! in its environment, and closing that ends it.

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

fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../providers/testdata/{name}"))
}

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Option<Child>,
}

impl Setup {
    /// A daemon whose default harness is `yaml` (the `agents:` block).
    fn new(agents: &str) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-hs-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cfg")).expect("root");
        std::fs::create_dir_all(root.join("work")).expect("work");
        std::fs::write(root.join("cfg/dreamer.yaml"), agents).expect("settings");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let mut s = Setup {
            root,
            tmux,
            daemon: None,
        };
        s.start();
        s
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
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
                // Stands in for `dream`: only its path reaches the harness.
                .env("DREAMER_DREAM", "/bin/echo")
                // Must never reach the harness.
                .env("OPENAI_API_KEY", "sk-test-must-not-arrive")
                .env("ANTHROPIC_API_KEY", "sk-test-must-not-arrive")
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

    async fn restart(&mut self) {
        let pid = self.daemon_pid().expect("daemon pid");
        let c = self.client().await;
        let _ = c.call(Request::DaemonShutdown(api::Empty {})).await;
        drop(c);
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

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.work().join(format!("{name}.log"))).unwrap_or_default()
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

async fn send(c: &Client, id: &str, text: &str) {
    c.call(Request::AgentSessionsSend(api::AgentSessionSend {
        session: id.into(),
        text: text.into(),
    }))
    .await
    .expect("send");
}

struct Follower {
    _client: Client,
    pushed: tokio::sync::mpsc::Receiver<Pushed>,
    last: u64,
}

impl Follower {
    async fn new(setup: &Setup, id: &str, after: u64) -> Follower {
        let mut c = setup.client().await;
        let pushed = c.pushed().expect("pushed");
        let r = c
            .call(Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                session: id.into(),
                follow: true,
                after: Some(after),
                ..api::AgentSessionEventsQuery::default()
            }))
            .await
            .expect("follow");
        let Response::AgentSessionsEvents(r) = r else {
            panic!("unexpected reply");
        };
        assert!(r.subscription.is_some());
        let last = r.events.last().map_or(after, |e| e.seq);
        Follower {
            _client: c,
            pushed,
            last,
        }
    }

    async fn until(&mut self, stop: impl Fn(&K) -> bool) -> Vec<K> {
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
                if e.seq <= self.last {
                    continue;
                }
                assert_eq!(e.seq, self.last + 1, "events arrive in order, none missing");
                self.last = e.seq;
                let k = e.kind.expect("kind");
                let done = stop(&k);
                out.push(k);
                if done {
                    return out;
                }
            }
        }
    }
}

fn turn_end(k: &K) -> bool {
    matches!(k, K::TurnEnd(_))
}

fn stop_reason(k: &[K]) -> api::AgentStopReason {
    match k.last() {
        Some(K::TurnEnd(t)) => api::AgentStopReason::try_from(t.stop_reason).expect("reason"),
        other => panic!("no turn end: {other:?}"),
    }
}

fn texts(k: &[K]) -> String {
    k.iter()
        .filter_map(|k| match k {
            K::TextDelta(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool(k: &[K], status: api::AgentToolStatus) -> Option<api::AgentToolCall> {
    k.iter().find_map(|k| match k {
        K::ToolCall(t) if t.status == status as i32 => Some(t.clone()),
        _ => None,
    })
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

/// The whole conversation, the same for every harness.
async fn converse(harness: &str, fake_name: &str, resumed_marker: &str) {
    let yaml = match harness {
        "acp" => format!(
            "agents:\n  default: helper\n  helper:\n    protocol: acp\n    mode: default\n    command: [/bin/sh, {}]\n",
            fake(fake_name).display()
        ),
        other => format!(
            "agents:\n  default: {other}\n  {other}:\n    command: [/bin/sh, {}]\n",
            fake(fake_name).display()
        ),
    };
    let mut setup = Setup::new(&yaml);
    let c = setup.client().await;
    let s = session_of(
        c.call(Request::AgentSessionsCreate(api::AgentSessionCreate {
            cwd: Some(setup.work().display().to_string()),
            ..api::AgentSessionCreate::default()
        }))
        .await
        .expect("create"),
    );
    assert_eq!(s.state, api::AgentSessionState::Idle as i32, "{s:?}");
    if harness == "acp" {
        // The mode in the settings is set before any prompt.
        assert!(
            setup.log(fake_name).contains("set_mode:default"),
            "{}",
            setup.log(fake_name)
        );
    }
    let pid = s.pid.expect("a harness process");
    let mut f = Follower::new(&setup, &s.id, 0).await;

    // Two plain turns: a conversation.
    send(&c, &s.id, "hello").await;
    let k = f.until(turn_end).await;
    assert_eq!(stop_reason(&k), api::AgentStopReason::EndTurn, "{k:?}");
    assert!(texts(&k).contains("Hello from fake"), "{k:?}");
    assert!(k.iter().any(|k| matches!(k, K::ThinkingDelta(_))), "{k:?}");
    assert!(k.iter().any(|k| matches!(k, K::Usage(_))), "{k:?}");
    send(&c, &s.id, "hello again").await;
    let k = f.until(turn_end).await;
    assert_eq!(stop_reason(&k), api::AgentStopReason::EndTurn, "{k:?}");
    let own = {
        let Response::AgentSessionsList(l) = c
            .call(Request::AgentSessionsList(api::Empty {}))
            .await
            .expect("list")
        else {
            panic!()
        };
        l.sessions[0]
            .harness_session
            .clone()
            .expect("the harness's own session")
    };

    // A tool call, approved.
    send(&c, &s.id, "please use a tool").await;
    let k = f.until(|k| matches!(k, K::ApprovalRequest(_))).await;
    assert!(tool(&k, api::AgentToolStatus::Started).is_some(), "{k:?}");
    let Some(K::ApprovalRequest(a)) = k.last() else {
        unreachable!()
    };
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
    let k = f.until(turn_end).await;
    let done = tool(&k, api::AgentToolStatus::Finished).expect("finished");
    assert_eq!(done.output.as_deref(), Some("file-a"), "{k:?}");
    assert_eq!(stop_reason(&k), api::AgentStopReason::EndTurn);

    // The same, denied.
    send(&c, &s.id, "please use a tool").await;
    let k = f.until(|k| matches!(k, K::ApprovalRequest(_))).await;
    let Some(K::ApprovalRequest(a)) = k.last() else {
        unreachable!()
    };
    session_of(
        c.call(Request::AgentSessionsDeny(api::AgentSessionAnswer {
            session: s.id.clone(),
            approval: a.id.clone(),
            option: None,
            for_pane: None,
        }))
        .await
        .expect("deny"),
    );
    let k = f.until(turn_end).await;
    assert!(
        tool(&k, api::AgentToolStatus::Failed).is_some(),
        "a denied call fails: {k:?}"
    );

    // A turn cancelled.
    send(&c, &s.id, "work slowly").await;
    f.until(|k| matches!(k, K::TextDelta(t) if t.text == "Working"))
        .await;
    c.call(Request::AgentSessionsCancel(api::AgentSessionRef {
        session: s.id.clone(),
    }))
    .await
    .expect("cancel");
    let k = f.until(turn_end).await;
    assert_eq!(stop_reason(&k), api::AgentStopReason::Cancelled, "{k:?}");

    let log = setup.log(fake_name);
    assert!(
        log.contains("key:none") && !log.contains("key:seen"),
        "{log}"
    );
    assert!(log.contains(&format!("session-env:{}", s.id)), "{log}");
    assert!(
        log.contains("/bin/echo"),
        "Dreamer's tools reach the harness:\n{log}"
    );

    // After a restart the harness continues its own session.
    let last = f.last;
    drop(f);
    setup.restart().await;
    assert!(!alive_for_a_moment(pid).await, "the harness ended");
    let c = setup.client().await;
    let mut f = Follower::new(&setup, &s.id, last).await;
    send(&c, &s.id, "are you there").await;
    let k = f.until(turn_end).await;
    assert!(
        k.iter().any(
            |k| matches!(k, K::Status(st) if st.detail.as_deref() == Some("resumed")
            && st.harness_session.as_deref() == Some(own.as_str()))
        ),
        "{k:?}"
    );
    assert!(
        !texts(&k).contains("replayed"),
        "history is not repeated: {k:?}"
    );
    assert_eq!(stop_reason(&k), api::AgentStopReason::EndTurn);
    let log = setup.log(fake_name);
    assert!(
        log.contains(&format!("{resumed_marker}{own}")),
        "resumed on {own}:\n{log}"
    );

    let closed = session_of(
        c.call(Request::AgentSessionsClose(api::AgentSessionRef {
            session: s.id.clone(),
        }))
        .await
        .expect("close"),
    );
    assert_eq!(closed.state, api::AgentSessionState::Closed as i32);
    let pid = s.pid.unwrap_or_default();
    assert!(!alive_for_a_moment(pid).await, "closing ended the harness");
}

#[tokio::test]
async fn an_acp_agent_configured_only_in_settings_holds_a_conversation() {
    converse("acp", "fake-acp", "load:").await;
}

#[tokio::test]
async fn claude_code_sessions_are_long_lived_and_bidirectional() {
    converse("claude", "fake-claude", "--resume ").await;
}

#[tokio::test]
async fn codex_sessions_run_over_its_app_server() {
    converse("codex", "fake-codex", "thread/resume:").await;
}
