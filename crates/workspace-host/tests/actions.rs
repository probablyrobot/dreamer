//! Typed actions against real tmux (baseline 3.8) on an isolated server.

use std::time::Duration;

use api::{
    Action, ActionRecord, ActionState, KillPane, KillWindow, RenameSession, RenameWindow,
    ResizePane, SelectPane, SelectWindow, SendInput, SendKeys, SplitDirection, SplitPane,
    SubmitAction, ZoomPane, action::Kind,
};
use domain::ServerKey;
use tmux_backend::server::tmux_program;
use tmux_backend::{
    Connection, ConnectionConfig, InventoryTracker, Socket, TmuxCommand, TmuxServer,
};
use workspace_host::{Actions, Endpoint, Hooks, Ledger};

struct Harness {
    server: TmuxServer,
    /// The connection ends when nobody reads its events.
    _events: tokio::sync::mpsc::Receiver<tmux_backend::ConnEvent>,
    ep: Endpoint,
    dir: std::path::PathBuf,
}

fn config(server: &TmuxServer) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(
        tmux_program(),
        Socket::Name(server.socket().to_owned()),
        "main",
    );
    c.config_file = Some("/dev/null".into());
    c.env = Some(vec![
        (
            "PATH".into(),
            "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin".into(),
        ),
        ("HOME".into(), std::env::temp_dir().display().to_string()),
        ("TERM".into(), "xterm-256color".into()),
        ("SHELL".into(), "/bin/sh".into()),
    ]);
    c
}

async fn start(name: &str) -> Harness {
    let server = TmuxServer::start(120, 40, "/bin/sh").expect("tmux");
    server
        .run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
        .expect("shell");
    let (conn, events) = Connection::spawn(config(&server)).await.expect("connect");
    let (tracker, _) = InventoryTracker::start(conn.clone(), ServerKey("test".into()), "test")
        .await
        .expect("inventory");
    let dir = std::env::temp_dir().join(format!("dreamer-actions-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Harness {
        server,
        _events: events,
        ep: Endpoint {
            name: "test".into(),
            conn,
            tracker,
        },
        dir,
    }
}

impl Harness {
    fn ledger_path(&self) -> std::path::PathBuf {
        self.dir.join("actions.jsonl")
    }

    fn actions(&self, hooks: Hooks) -> Actions {
        let (ledger, _) = Ledger::open(&self.ledger_path()).expect("ledger");
        Actions::new(ledger, hooks)
    }

    fn first_pane(&self) -> u32 {
        self.ep
            .tracker
            .inventory()
            .panes
            .keys()
            .next()
            .expect("pane")
            .0
    }

    fn pane_count(&self) -> usize {
        self.ep.tracker.inventory().panes.len()
    }

    async fn capture(&self, pane: u32) -> String {
        let r = self
            .ep
            .conn
            .execute(
                TmuxCommand::new("capture-pane")
                    .flag("-p")
                    .opt("-t", format!("%{pane}")),
            )
            .await
            .expect("capture");
        r[0].body
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn wait_for_text(&self, pane: u32, text: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let shown = self.capture(pane).await;
            if shown.contains(text) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "pane %{pane} never showed {text:?}:\n{shown}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn submit(kind: Kind) -> SubmitAction {
    SubmitAction {
        action: Some(Action { kind: Some(kind) }),
        ..SubmitAction::default()
    }
}

fn completed(r: &ActionRecord) -> &str {
    assert_eq!(
        r.state,
        ActionState::Completed as i32,
        "expected completed: {r:?}"
    );
    r.observed.as_deref().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_action_reports_what_tmux_shows_afterwards() {
    let mut h = start("all").await;
    let mut actions = h.actions(Hooks::default());
    let first = h.first_pane();
    let window =
        h.ep.tracker
            .inventory()
            .windows
            .keys()
            .next()
            .expect("window")
            .0;
    let session =
        h.ep.tracker
            .inventory()
            .sessions
            .keys()
            .next()
            .expect("session")
            .0;

    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::SplitPane(SplitPane {
                pane: first,
                direction: SplitDirection::Horizontal as i32,
                percent: Some(30),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo split-argv-ok; exec cat".into(),
                ],
                ..SplitPane::default()
            })),
        )
        .await
        .expect("split");
    assert!(completed(&r).contains("created pane %"), "{r:?}");
    let second = r.new_pane.expect("new pane");
    assert_eq!(h.pane_count(), 2);
    h.wait_for_text(second, "split-argv-ok").await;

    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::ResizePane(ResizePane {
                pane: second,
                width: Some(50),
                height: None,
            })),
        )
        .await
        .expect("resize");
    assert!(
        completed(&r).contains(&format!("pane %{second} is 50x")),
        "{r:?}"
    );

    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::SelectPane(SelectPane { pane: first })),
        )
        .await
        .expect("select pane");
    assert_eq!(completed(&r), format!("pane %{first} is active"));

    let r = actions
        .submit(&mut h.ep, submit(Kind::ZoomPane(ZoomPane { pane: first })))
        .await
        .expect("zoom");
    assert!(completed(&r).contains("is zoomed onto"), "{r:?}");
    let r = actions
        .submit(&mut h.ep, submit(Kind::ZoomPane(ZoomPane { pane: first })))
        .await
        .expect("unzoom");
    assert!(completed(&r).contains("is not zoomed"), "{r:?}");

    for name in ["build; #{pane_id} $HOME", "plain"] {
        let r = actions
            .submit(
                &mut h.ep,
                submit(Kind::RenameWindow(RenameWindow {
                    window,
                    name: name.into(),
                })),
            )
            .await
            .expect("rename window");
        assert_eq!(completed(&r), format!("window @{window} is named {name:?}"));
    }
    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::RenameSession(RenameSession {
                session,
                name: "work #1 ~ $x".into(),
            })),
        )
        .await
        .expect("rename session");
    assert_eq!(
        completed(&r),
        format!("session ${session} is named \"work #1 ~ $x\"")
    );

    // Exact bytes, including a control character, reach the program.
    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::SendInput(SendInput {
                pane: second,
                data: b"bytes-\x01-ok\n".to_vec(),
                generation: None,
            })),
        )
        .await
        .expect("send input");
    assert!(completed(&r).contains("delivered 11 bytes"), "{r:?}");
    h.wait_for_text(second, "bytes-^A-ok").await;

    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::SendKeys(SendKeys {
                pane: first,
                keys: vec![
                    "e".into(),
                    "c".into(),
                    "h".into(),
                    "o".into(),
                    "Space".into(),
                    "k".into(),
                    "e".into(),
                    "y".into(),
                    "s".into(),
                    "Enter".into(),
                ],
                generation: None,
            })),
        )
        .await
        .expect("send keys");
    completed(&r);
    h.wait_for_text(first, "\nkeys").await;

    // A second window to select and kill.
    h.server
        .run_checked(&[
            "new-window",
            "-d",
            "-t",
            &format!("${session}:"),
            "sleep 600",
        ])
        .expect("window");
    h.ep.tracker.refresh().await.expect("refresh");
    let other = *h
        .ep
        .tracker
        .inventory()
        .windows
        .keys()
        .find(|w| w.0 != window)
        .expect("other");
    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::SelectWindow(SelectWindow { window: other.0 })),
        )
        .await
        .expect("select window");
    assert_eq!(completed(&r), format!("window @{} is active", other.0));
    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::KillWindow(KillWindow { window: other.0 })),
        )
        .await
        .expect("kill window");
    assert_eq!(completed(&r), format!("window @{} is gone", other.0));

    let r = actions
        .submit(&mut h.ep, submit(Kind::KillPane(KillPane { pane: second })))
        .await
        .expect("kill pane");
    assert_eq!(completed(&r), format!("pane %{second} is gone"));
    assert_eq!(h.pane_count(), 1);

    // Every transition is on disk, newest first in the listing.
    let listed = actions.list(100);
    assert_eq!(listed.len(), 13);
    assert!(
        listed
            .iter()
            .all(|r| r.state == ActionState::Completed as i32)
    );
    let lines = std::fs::read_to_string(h.ledger_path()).expect("ledger");
    assert_eq!(
        lines.lines().count(),
        13 * 4,
        "accepted, prepared, dispatching, completed"
    );
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_actions_send_nothing() {
    let mut h = start("refused").await;
    let mut actions = h.actions(Hooks::default());
    let first = h.first_pane();
    for (kind, why) in [
        (
            Kind::SelectPane(SelectPane { pane: 999 }),
            "pane %999 does not exist",
        ),
        (
            Kind::KillWindow(KillWindow { window: 999 }),
            "window @999 does not exist",
        ),
        (
            Kind::SplitPane(SplitPane {
                pane: first,
                ..SplitPane::default()
            }),
            "needs a direction",
        ),
        (
            Kind::SplitPane(SplitPane {
                pane: first,
                direction: SplitDirection::Vertical as i32,
                percent: Some(100),
                ..SplitPane::default()
            }),
            "between 1 and 99",
        ),
        (
            Kind::SendKeys(SendKeys {
                pane: first,
                keys: vec!["two words".into()],
                generation: None,
            }),
            "key names",
        ),
    ] {
        let r = actions
            .submit(&mut h.ep, submit(kind))
            .await
            .expect("record");
        assert_eq!(r.state, ActionState::Failed as i32, "{r:?}");
        assert!(
            r.reason.as_deref().unwrap_or_default().contains(why),
            "{r:?}"
        );
    }
    assert_eq!(h.pane_count(), 1);
    let err = actions
        .submit(&mut h.ep, SubmitAction::default())
        .await
        .expect_err("no kind");
    assert_eq!(err.code, api::code::INVALID_PARAMS);
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_for_a_replaced_process_is_refused() {
    let mut h = start("generation").await;
    let mut actions = h.actions(Hooks::default());
    let pane = h.first_pane();
    let before =
        h.ep.tracker
            .generation(domain::TmuxPaneId(pane))
            .expect("generation")
            .0;
    let ok = actions
        .submit(
            &mut h.ep,
            submit(Kind::SendInput(SendInput {
                pane,
                data: b"echo first\n".to_vec(),
                generation: Some(before),
            })),
        )
        .await
        .expect("send");
    completed(&ok);

    h.server
        .run_checked(&["respawn-pane", "-k", "-t", &format!("%{pane}"), "/bin/sh"])
        .expect("respawn");
    let r = actions
        .submit(
            &mut h.ep,
            submit(Kind::SendInput(SendInput {
                pane,
                data: b"echo must-not-run\n".to_vec(),
                generation: Some(before),
            })),
        )
        .await
        .expect("record");
    assert_eq!(r.state, ActionState::Failed as i32, "{r:?}");
    assert!(
        r.reason
            .as_deref()
            .unwrap_or_default()
            .contains("was replaced"),
        "{r:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!h.capture(pane).await.contains("must-not-run"));
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idempotency_key_runs_once_and_rejects_other_content() {
    let mut h = start("idempotent").await;
    let mut actions = h.actions(Hooks::default());
    let first = h.first_pane();
    let split = |percent| SubmitAction {
        idempotency_key: Some("split-once".into()),
        ..submit(Kind::SplitPane(SplitPane {
            pane: first,
            direction: SplitDirection::Vertical as i32,
            percent: Some(percent),
            ..SplitPane::default()
        }))
    };
    let a = actions.submit(&mut h.ep, split(40)).await.expect("first");
    let b = actions.submit(&mut h.ep, split(40)).await.expect("again");
    assert_eq!(a.id, b.id);
    h.ep.tracker.refresh().await.expect("refresh");
    assert_eq!(h.pane_count(), 2, "the repeat did not split again");
    let err = actions
        .submit(&mut h.ep, split(60))
        .await
        .expect_err("conflict");
    assert_eq!(err.code, api::code::CONFLICT);
    // The key survives a restart.
    drop(actions);
    let mut actions = h.actions(Hooks::default());
    let c = actions
        .submit(&mut h.ep, split(40))
        .await
        .expect("after restart");
    assert_eq!(c.id, a.id);
    h.ep.tracker.refresh().await.expect("refresh");
    assert_eq!(h.pane_count(), 2);
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_mid_action_is_outcome_unknown_and_never_replayed() {
    let mut h = start("crash").await;
    let first = h.first_pane();
    let mut crashing = h.actions(Hooks {
        hang_after_dispatch: true,
        ..Hooks::default()
    });
    let req = submit(Kind::SplitPane(SplitPane {
        pane: first,
        direction: SplitDirection::Horizontal as i32,
        ..SplitPane::default()
    }));
    // The command reaches tmux; the process "dies" before recording more.
    let cut = tokio::time::timeout(Duration::from_secs(3), crashing.submit(&mut h.ep, req)).await;
    assert!(cut.is_err(), "the hook holds the action after dispatch");
    drop(crashing);

    let (ledger, settled) = Ledger::open(&h.ledger_path()).expect("reopen");
    assert_eq!(settled.len(), 1);
    let r = &settled[0];
    assert_eq!(r.state, ActionState::OutcomeUnknown as i32, "{r:?}");
    assert!(
        r.reason
            .as_deref()
            .unwrap_or_default()
            .contains("will not be retried")
    );
    assert_eq!(
        ledger.get(&r.id).expect("stored").state,
        ActionState::OutcomeUnknown as i32
    );

    // The split happened exactly once; reopening did not send it again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.ep.tracker.refresh().await.expect("refresh");
    assert_eq!(h.pane_count(), 2);
    let (_again, settled_again) = Ledger::open(&h.ledger_path()).expect("reopen twice");
    assert!(settled_again.is_empty(), "settling happens once");
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[test]
fn a_torn_last_line_is_dropped_but_corruption_elsewhere_is_an_error() {
    let dir = std::env::temp_dir().join(format!("dreamer-ledger-torn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("actions.jsonl");
    let good = r#"{"id":"a1","state":"completed","endpoint":"e"}"#;
    std::fs::write(&path, format!("{good}\n{{\"id\":\"a2\",\"sta")).expect("write");
    let (ledger, settled) = Ledger::open(&path).expect("torn tail is tolerated");
    assert!(settled.is_empty());
    assert!(ledger.get("a1").is_some() && ledger.get("a2").is_none());
    drop(ledger);
    std::fs::write(&path, format!("not json\n{good}\n")).expect("write");
    assert!(Ledger::open(&path).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
