//! Filters through a real daemon (spec 12.8): a derived stream of matched
//! lines, output a plain tmux client sees unchanged, and a self-triggering
//! rule stopped by the circuit breaker.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-flt-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.root.join("rt/dreamerd.sock"), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn tmux(&self, args: &[&str]) {
        self.tmux.run_checked(args).expect("tmux command");
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(c: &Client, r: Request) -> Result<Response, client::ClientError> {
    c.call(r).await
}

async fn pane0(c: &Client) -> u32 {
    let Ok(Response::TmuxTopology(t)) =
        call(c, Request::TmuxTopology(api::TopologyParams::default())).await
    else {
        panic!("topology")
    };
    t.panes[0].id
}

fn grant(methods: &[&str], panes: &[u32]) -> Option<api::ScriptGrant> {
    Some(api::ScriptGrant {
        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        panes: panes.to_vec(),
        all_panes: false,
    })
}

async fn status(c: &Client, name: &str) -> api::FilterStatus {
    let Ok(Response::FiltersList(l)) = call(c, Request::FiltersList(api::Empty {})).await else {
        panic!("list")
    };
    l.filters
        .into_iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no filter {name}"))
}

#[tokio::test]
async fn a_filter_publishes_matches_and_leaves_the_output_alone() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    let pane = pane0(&c).await;
    call(
        &c,
        Request::MessagesSubscribe(api::MessageSubscribe {
            topics: vec!["filter.*".into()],
        }),
    )
    .await
    .expect("subscribe");
    // Its input trigger must stay inside its grant.
    let outside = api::FilterSpec {
        name: "nope".into(),
        panes: vec![pane],
        pattern: "x".into(),
        input: Some(api::FilterInput {
            pane: pane + 100,
            text: "y".into(),
        }),
        grant: grant(&["taps.subscribe", "terminal.input"], &[pane]),
        ..api::FilterSpec::default()
    };
    let e = call(&c, Request::FiltersInstall(outside))
        .await
        .expect_err("outside");
    assert!(e.to_string().contains("outside the filter's grant"), "{e}");

    call(
        &c,
        Request::FiltersInstall(api::FilterSpec {
            name: "errs".into(),
            panes: vec![pane],
            pattern: "^ERR ".into(),
            topic: Some("filter.errs".into()),
            grant: grant(&["taps.subscribe", "messages.publish"], &[pane]),
            ..api::FilterSpec::default()
        }),
    )
    .await
    .expect("install");
    s.tmux(&[
        "send-keys",
        "-t",
        &format!("%{pane}"),
        "printf 'ok\\nERR one\\nERR two\\n'",
        "Enter",
    ]);
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while got.len() < 2 {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "messages.deliver" => {
                let e = api::MessageDelivery::decode(payload.as_slice())
                    .expect("delivery")
                    .envelope
                    .expect("envelope");
                assert_eq!(e.sender.expect("sender").instance, "filter:errs");
                got.push(String::from_utf8_lossy(&e.payload).into_owned());
            }
            Ok(Some(_)) => {}
            other => panic!("no derived stream: {other:?} after {got:?}"),
        }
    }
    assert_eq!(got, ["ERR one", "ERR two"]);
    // A plain tmux client sees exactly what the program wrote.
    let screen = s
        .tmux
        .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
        .expect("capture");
    assert!(screen.lines().any(|l| l == "ERR one"), "{screen}");
    assert!(!screen.contains('\u{1b}'));
    let st = status(&c, "errs").await;
    assert_eq!((st.matched, st.triggered), (2, 2), "{st:?}");

    // Pause, resume, remove.
    call(
        &c,
        Request::FiltersPause(api::FilterRef {
            name: "errs".into(),
        }),
    )
    .await
    .expect("pause");
    assert_eq!(
        status(&c, "errs").await.state,
        api::FilterState::Paused as i32
    );
    call(
        &c,
        Request::FiltersResume(api::FilterRef {
            name: "errs".into(),
        }),
    )
    .await
    .expect("resume");
    assert_eq!(
        status(&c, "errs").await.state,
        api::FilterState::Running as i32
    );
    call(
        &c,
        Request::FiltersRemove(api::FilterRef {
            name: "errs".into(),
        }),
    )
    .await
    .expect("remove");
    let Ok(Response::FiltersList(l)) = call(&c, Request::FiltersList(api::Empty {})).await else {
        panic!("list")
    };
    assert!(l.filters.is_empty());
}

#[tokio::test]
async fn a_self_triggering_filter_is_tripped() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    let pane = pane0(&c).await;
    call(
        &c,
        Request::EventsSubscribe(api::EventSubscribe {
            kinds: vec!["filters".into()],
            ..api::EventSubscribe::default()
        }),
    )
    .await
    .expect("events");
    // Each `pong` it sees makes it type a command that prints `pong`.
    call(
        &c,
        Request::FiltersInstall(api::FilterSpec {
            name: "echoer".into(),
            panes: vec![pane],
            pattern: "^pong$".into(),
            input: Some(api::FilterInput {
                pane,
                text: "echo pong\n".into(),
            }),
            breaker: Some(5),
            per_second: Some(100),
            grant: grant(&["taps.subscribe", "terminal.input"], &[pane]),
            ..api::FilterSpec::default()
        }),
    )
    .await
    .expect("install");
    s.tmux(&["send-keys", "-t", &format!("%{pane}"), "echo pong", "Enter"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let tripped = loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "events.batch" => {
                let b = api::EventBatch::decode(payload.as_slice()).expect("batch");
                if let Some(e) = b.events.into_iter().find(|e| e.kind == "filters.tripped") {
                    break e;
                }
            }
            Ok(Some(_)) => {}
            other => panic!("the loop was never stopped: {other:?}"),
        }
    };
    assert_eq!(tripped.subject.as_deref(), Some("echoer"));
    let st = status(&c, "echoer").await;
    assert_eq!(st.state, api::FilterState::Tripped as i32, "{st:?}");
    assert!(st.reason.clone().unwrap_or_default().contains("a loop?"));
    assert_eq!(
        st.triggered, 6,
        "the breaker stops it right after its limit"
    );
    assert!(
        st.skipped_own >= 1,
        "its own typed commands are not matched: {st:?}"
    );
    // And the pane stops filling with pongs.
    let count = || {
        s.tmux
            .run_checked(&[
                "capture-pane",
                "-p",
                "-S",
                "-200",
                "-t",
                &format!("%{pane}"),
            ])
            .expect("capture")
            .lines()
            .filter(|l| *l == "pong")
            .count()
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let settled = count();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(count(), settled, "no more triggers after the trip");
    assert!(settled <= 8, "{settled} pongs");
}

async fn decorations(c: &Client, pane: u32) -> Vec<api::Decoration> {
    match call(c, Request::DecorationsList(api::DecorationQuery { pane })).await {
        Ok(Response::DecorationsList(l)) => l.decorations,
        other => panic!("{other:?}"),
    }
}

async fn wait_marks(c: &Client, pane: u32, n: usize) -> Vec<api::Decoration> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let d = decorations(c, pane).await;
        if d.len() == n {
            return d;
        }
        assert!(Instant::now() < deadline, "wanted {n} marks, have {d:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn decorations_mark_matched_lines_until_a_reflow_or_respawn() {
    let s = Setup::new();
    let c = s.client().await;
    let pane = pane0(&c).await;
    let target = format!("%{pane}");
    call(
        &c,
        Request::FiltersInstall(api::FilterSpec {
            name: "marks".into(),
            panes: vec![pane],
            pattern: "^WARN ".into(),
            decorate: api::DecorationStyle::Badge as i32,
            badge: Some("!".into()),
            grant: grant(&["taps.subscribe"], &[pane]),
            ..api::FilterSpec::default()
        }),
    )
    .await
    .expect("install");
    s.tmux(&[
        "send-keys",
        "-t",
        &target,
        "printf 'a\\nWARN one\\nb\\n'",
        "Enter",
    ]);
    let marks = wait_marks(&c, pane, 1).await;
    let m = &marks[0];
    assert_eq!(
        (m.text.as_str(), m.badge.as_deref(), m.filter.as_str()),
        ("WARN one", Some("!"), "marks")
    );
    // The row names that very line, counted from the top of history.
    let Ok(Response::TerminalRead(t)) = call(
        &c,
        Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(100_000),
            endpoint: None,
        }),
    )
    .await
    else {
        panic!("read")
    };
    assert_eq!(t.lines[m.row as usize], "WARN one", "{t:?}");

    // A reflow drops it.
    s.tmux(&["resize-window", "-t", "main", "-x", "70"]);
    wait_marks(&c, pane, 0).await;
    // So does a respawn, after a new mark.
    s.tmux(&["send-keys", "-t", &target, "echo 'WARN two'", "Enter"]);
    wait_marks(&c, pane, 1).await;
    s.tmux(&["respawn-pane", "-k", "-t", &target, "/bin/sh"]);
    wait_marks(&c, pane, 0).await;
}

/// The desktop's path to a mark: the decorations feed lists a pane's marks
/// when they change, and a mark's row names the same line in a view's own
/// replica of the pane (built from its checkpoint), with history above it.
#[tokio::test]
async fn a_view_finds_a_mark_on_its_own_copy_of_the_line() {
    let s = Setup::new();
    let c = s.client().await;
    let pane = pane0(&c).await;
    let target = format!("%{pane}");
    let socket = s.root.join("rt/dreamerd.sock");
    // History above the line, so its row counts from well above the screen.
    s.tmux(&["send-keys", "-t", &target, "seq 1 80", "Enter"]);
    let marks = replica::decorations::DecorationsFeed::start(socket.clone()).expect("feed");
    marks.watch([pane]);
    call(
        &c,
        Request::FiltersInstall(api::FilterSpec {
            name: "view-marks".into(),
            panes: vec![pane],
            pattern: "^MARKED ".into(),
            decorate: api::DecorationStyle::Highlight as i32,
            grant: grant(&["taps.subscribe"], &[pane]),
            ..api::FilterSpec::default()
        }),
    )
    .await
    .expect("install");
    s.tmux(&[
        "send-keys",
        "-t",
        &target,
        "printf 'MARKED %s\\n' here",
        "Enter",
    ]);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while marks.marks(pane).is_empty() {
        tokio::time::timeout_at(deadline, marks.wake.recv())
            .await
            .expect("the feed never listed the mark")
            .expect("feed alive");
    }
    let m = marks.marks(pane).remove(0);
    assert_eq!(m.text, "MARKED here");

    let feed = replica::feed::Feed::start(socket, Some(pane), None).expect("pane feed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let frame = loop {
        tokio::time::timeout_at(deadline, feed.wake.recv())
            .await
            .expect("no frame with the line")
            .expect("feed alive");
        if let Some(f) = feed.take()
            && f.text_dump().contains("\nMARKED here")
        {
            break f;
        }
    };
    assert!(frame.view.offset > 0, "no history above: {:?}", frame.view);
    assert_eq!(m.cols, u32::from(frame.size.cols));
    let i = usize::try_from(m.row - frame.view.offset).expect("row");
    assert_eq!(frame.rows[i].text().trim_end(), "MARKED here", "{m:?}");

    // A pane no longer watched keeps no marks.
    marks.watch([]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marks.marks(pane).is_empty() {
        assert!(Instant::now() < deadline, "unwatched marks kept");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
