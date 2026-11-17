//! A `transcript` and a `composer` bound to an agent session, in a real
//! app pane, with the scripted stand-in for pi
//! (`crates/providers/testdata/fake-pi`) as the harness: no model, no
//! login. The app creates its session; a prompt typed into the composer
//! (through `apps.input`, as the desktop sends it) streams a long answer
//! that the pane draws as text; a tool call shows as a card and its
//! approval as an approval card, answered from the transcript. A desktop
//! reads the same session for the app (`for_pane`), and is refused a
//! session the app may not see.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use view_tree::{Bind, Kind};

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

/// The example chat app: a transcript and a composer bound to the session
/// it creates with Ctrl-n.
const MANIFEST: &str = include_str!("../../../examples/apps/chat/app.toml");

fn fake_pi() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/testdata/fake-pi")
}

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg(self.daemon.id().to_string())
            .status();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An isolated tmux server and daemon whose harness is the fake pi.
fn setup() -> Setup {
    let root = PathBuf::from(format!("/tmp/drm-chat-{}", std::process::id()));
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
    let tmux = TmuxServer::start(100, 40, "/bin/sh").expect("tmux");
    let daemon = PathBuf::from(APP)
        .parent()
        .expect("target dir")
        .join("dreamerd");
    assert!(
        daemon.exists(),
        "{} is missing; build dreamer-server",
        daemon.display()
    );
    let daemon = Command::new(daemon)
        .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
        .arg(tmux_program())
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_DATA_DIR", root.join("data"))
        .env("DREAMER_CONFIG_DIR", root.join("cfg"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dreamerd");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("rt/dreamerd.sock").exists() {
        assert!(Instant::now() < deadline, "daemon never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    Setup { root, tmux, daemon }
}

async fn call(c: &client::Client, r: Request) -> Response {
    c.call(r).await.expect("call")
}

fn input(pane: u32) -> api::AppInput {
    api::AppInput {
        pane,
        ..api::AppInput::default()
    }
}

async fn key(c: &client::Client, pane: u32, key: &str) {
    call(
        c,
        Request::AppsInput(api::AppInput {
            key: key.into(),
            ..input(pane)
        }),
    )
    .await;
}

async fn text(c: &client::Client, pane: u32, text: &str) {
    call(
        c,
        Request::AppsInput(api::AppInput {
            text: Some(text.into()),
            ..input(pane)
        }),
    )
    .await;
}

/// The pane as a plain tmux client sees it, once every one of `want` is
/// on it.
async fn screen_until(s: &Setup, pane: u32, want: &[&str]) -> String {
    let target = format!("%{pane}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let screen = s
            .tmux
            .run_checked(&["capture-pane", "-p", "-t", &target])
            .unwrap_or_default();
        if want.iter().all(|w| screen.contains(w)) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "the pane never showed {want:?}:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The session the app's transcript is bound to, once it is.
async fn bound_session(c: &client::Client, pane: u32) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut mirror = view_tree::Mirror::default();
    loop {
        let Response::AppsTree(t) = call(
            c,
            Request::AppsTree(api::AppTreeQuery {
                pane,
                after: 0,
                seq: mirror.seq,
                wait_ms: Some(500),
            }),
        )
        .await
        else {
            panic!("not a tree");
        };
        mirror.take(t.seq, &t.tree, &t.patches).expect("tree");
        let chat = mirror.frame.as_ref().and_then(|f| f.root.find("chat"));
        if let Some(n) = chat {
            assert!(matches!(n.kind, Kind::Transcript { .. }));
            if let Some(Bind::Session(s)) = &n.bind {
                return s.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "the transcript was never bound: {:?}",
            mirror.frame
        );
    }
}

#[test]
fn a_bound_transcript_streams_shows_cards_and_draws_as_text() {
    let s = setup();
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, MANIFEST).expect("manifest");
    let needs = app_model::manifest::Manifest::parse(MANIFEST)
        .expect("valid")
        .needs();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let socket = s.root.join("rt/dreamerd.sock");
        let c = client::Client::connect(&socket, "test")
            .await
            .expect("connect");
        let Response::TmuxTopology(t) = call(&c, Request::TmuxTopology(Default::default())).await
        else {
            panic!("not a topology");
        };
        let Response::AppsLaunch(launched) = call(
            &c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "dreamer.chat-example".into(),
                grant: Some(api::Grant {
                    methods: needs.methods.clone(),
                    all_panes: needs.all_panes,
                    ..api::Grant::default()
                }),
                argv: vec![APP.into(), manifest.display().to_string()],
                session: t.sessions[0].id,
                ..api::LaunchApp::default()
            }),
        )
        .await
        else {
            panic!("not launched");
        };
        let pane = launched.pane;
        screen_until(&s, pane, &["no agent session bound"]).await;

        // The app creates its session; both nodes bind to it.
        key(&c, pane, "Ctrl-n").await;
        let session = bound_session(&c, pane).await;
        screen_until(&s, pane, &["idle", "Ask the agent"]).await;

        // A prompt typed into the composer streams a long answer.
        key(&c, pane, "Tab").await;
        text(&c, pane, "write at length").await;
        key(&c, pane, "Enter").await;
        let screen = screen_until(
            &s,
            pane,
            &[
                "Paragraph 60 of the answer",
                "The end of the long answer.",
                "idle",
            ],
        )
        .await;
        assert!(
            !screen.contains("write at length▏"),
            "the composer was cleared:\n{screen}"
        );

        // A desktop reads the same session for the app.
        let Response::AgentSessionsEvents(read) = call(
            &c,
            Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                session: session.clone(),
                for_pane: Some(pane),
                ..api::AgentSessionEventsQuery::default()
            }),
        )
        .await
        else {
            panic!("not events");
        };
        let deltas = read
            .events
            .iter()
            .filter(|e| matches!(e.kind, Some(api::agent_session_event::Kind::TextDelta(_))))
            .count();
        assert!(deltas > 100, "a long answer in many deltas: {deltas}");

        // A tool call and its approval show as cards.
        text(&c, pane, "use a tool").await;
        key(&c, pane, "Enter").await;
        screen_until(
            &s,
            pane,
            &[
                "? Allow bash?",
                "[y] allow",
                "[n] deny",
                "bash $ ls",
                "waiting for approval",
            ],
        )
        .await;
        // Answered from the transcript, on the app's own connection.
        key(&c, pane, "Tab").await;
        key(&c, pane, "y").await;
        screen_until(
            &s,
            pane,
            &["allowed by", "✓ bash $ ls", "file-a", "Done.", "idle"],
        )
        .await;

        // `@%N` in a prompt attaches that pane's text, read by the app.
        let shell = t.panes[0].id;
        s.tmux
            .run_checked(&[
                "send-keys",
                "-t",
                &format!("%{shell}"),
                "echo attach-me",
                "Enter",
            ])
            .expect("type into the shell");
        key(&c, pane, "Tab").await;
        text(&c, pane, &format!("what is in @%{shell}?")).await;
        key(&c, pane, "Enter").await;
        screen_until(&s, pane, &["Hello from fake pi."]).await;
        let Response::AgentSessionsEvents(read) = call(
            &c,
            Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                session: session.clone(),
                for_pane: Some(pane),
                ..api::AgentSessionEventsQuery::default()
            }),
        )
        .await
        else {
            panic!("not events");
        };
        let sent = read
            .events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Some(api::agent_session_event::Kind::UserMessage(t)) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            sent.starts_with(&format!(
                "what is in @%{shell}?\n\nPane %{shell}:\n```text\n"
            )) && sent.contains("attach-me"),
            "{sent}"
        );

        // A session the app did not create is not the app's to read, and
        // a desktop reading for the app is refused it.
        let Response::AgentSessionsCreate(other) = call(
            &c,
            Request::AgentSessionsCreate(api::AgentSessionCreate {
                cwd: Some("/tmp".into()),
                ..api::AgentSessionCreate::default()
            }),
        )
        .await
        else {
            panic!("not created");
        };
        let refused = c
            .call(Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                session: other.id.clone(),
                for_pane: Some(pane),
                ..api::AgentSessionEventsQuery::default()
            }))
            .await
            .expect_err("not the app's session");
        assert!(
            refused
                .to_string()
                .contains(&format!("for the app in pane %{pane}")),
            "{refused}"
        );
        // Nor may a desktop tap a pane for an app whose grant has no taps.
        let refused = c
            .call(Request::TapsSubscribe(api::TapSubscribe {
                level: api::TapLevel::Text as i32,
                panes: vec![pane],
                for_pane: Some(pane),
                ..api::TapSubscribe::default()
            }))
            .await
            .expect_err("no taps in the app's grant");
        assert!(refused.to_string().contains("taps.subscribe"), "{refused}");
        key(&c, pane, "Ctrl-c").await;
    });
}
