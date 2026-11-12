//! Inventory convergence against real tmux (baseline 3.8): changes made by
//! an ordinary tmux client reach the model through notifications and
//! snapshots, and are reported as typed events.

use std::time::Duration;

use domain::{BackendEvent, ServerKey, WindowLayout};
use tmux_backend::server::tmux_program;
use tmux_backend::{
    ConnEvent, Connection, ConnectionConfig, Inventory, InventoryTracker, Socket, TmuxServer,
    affects_structure,
};
use tokio::sync::mpsc::Receiver;

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
    ]);
    c
}

struct Harness {
    server: TmuxServer,
    conn: Connection,
    events: Receiver<ConnEvent>,
    tracker: InventoryTracker,
}

async fn start() -> Harness {
    let server = TmuxServer::start(80, 24, "sleep 600").expect("tmux");
    let (conn, events) = Connection::spawn(config(&server)).await.expect("connect");
    let (tracker, initial) =
        InventoryTracker::start(conn.clone(), ServerKey("test".into()), "isolated")
            .await
            .expect("start");
    assert!(
        initial
            .iter()
            .any(|e| matches!(e, BackendEvent::SessionAdded { .. }))
    );
    Harness {
        server,
        conn,
        events,
        tracker,
    }
}

impl Harness {
    /// Apply a change with an ordinary tmux client, then follow
    /// notifications until `done` holds; return the events reported.
    async fn change(
        &mut self,
        args: &[&str],
        done: impl Fn(&Inventory) -> bool,
    ) -> Vec<BackendEvent> {
        self.server
            .run_checked(args)
            .unwrap_or_else(|e| panic!("tmux {args:?}: {e}"));
        let mut reported = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !done(self.tracker.inventory()) {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "model did not converge after {args:?}: {:#?}",
                        self.tracker.inventory()
                    )
                })
                .expect("connection open");
            if let ConnEvent::Notification(n) = event
                && affects_structure(&n)
            {
                reported.extend(self.tracker.refresh().await.expect("refresh"));
            }
        }
        // The tracked model equals a fresh read of tmux.
        let fresh = Inventory::snapshot(&self.conn, ServerKey("test".into()), "isolated")
            .await
            .expect("snapshot");
        assert_eq!(
            self.tracker.inventory(),
            &fresh,
            "model differs from tmux after {args:?}"
        );
        reported
    }
}

fn pane_count(inv: &Inventory) -> usize {
    inv.panes.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_changes_converge() {
    let mut h = start().await;
    let first_window = *h.tracker.inventory().windows.keys().next().expect("window");

    // Split.
    let ev = h
        .change(&["split-window", "-h", "-t", "main", "sleep 600"], |i| {
            pane_count(i) == 2
        })
        .await;
    assert!(ev.iter().any(|e| matches!(e, BackendEvent::WindowLayout { layout: WindowLayout::Tiled(n), .. } if n.panes().len() == 2)));

    // Rename the window (hostile-ish name).
    let ev = h
        .change(&["rename-window", "-t", "main", "build; #1"], |i| {
            i.windows.values().any(|w| w.name == "build; #1")
        })
        .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, BackendEvent::WindowRenamed { name, .. } if name == "build; #1"))
    );

    // A second session.
    let ev = h
        .change(&["new-session", "-d", "-s", "second", "sleep 600"], |i| {
            i.sessions.len() == 2
        })
        .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, BackendEvent::SessionAdded { name, .. } if name == "second"))
    );

    // Link the first window into it: one window identity, two sessions.
    let ev = h
        .change(
            &[
                "link-window",
                "-s",
                &format!("@{}", first_window.0),
                "-t",
                "second:7",
            ],
            |i| {
                i.windows
                    .get(&first_window)
                    .is_some_and(|w| w.sessions.len() == 2)
            },
        )
        .await;
    assert!(ev.iter().any(|e| matches!(e, BackendEvent::WindowAdded { window, sessions } if window.window == first_window && sessions.len() == 2)));

    // Break the second pane out into its own window.
    let ev = h
        .change(&["break-pane", "-d", "-s", "main.1"], |i| {
            i.windows
                .values()
                .filter(|w| w.sessions.iter().any(|_| true))
                .count()
                >= 2
                && i.windows.get(&first_window).is_some_and(
                    |w| matches!(&w.layout, WindowLayout::Tiled(n) if n.panes().len() == 1),
                )
        })
        .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, BackendEvent::WindowLayout { .. }))
    );

    // Respawn a pane: same id, new process generation.
    let pane = *h.tracker.inventory().panes.keys().next().expect("pane");
    let before = h.tracker.generation(pane).expect("generation");
    let pid_before = h.tracker.inventory().panes[&pane].pid;
    let ev = h
        .change(
            &[
                "respawn-pane",
                "-k",
                "-t",
                &format!("%{}", pane.0),
                "sleep 600",
            ],
            |i| i.panes.get(&pane).is_some_and(|p| p.pid != pid_before),
        )
        .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, BackendEvent::PaneRespawned { .. }))
    );
    assert!(h.tracker.generation(pane).expect("generation") > before);

    // Rename and then kill the second session.
    let ev = h
        .change(&["rename-session", "-t", "second", "renamed"], |i| {
            i.sessions.values().any(|s| s.name == "renamed")
        })
        .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, BackendEvent::SessionRenamed { .. }))
    );
    let ev = h
        .change(&["kill-session", "-t", "renamed"], |i| {
            i.sessions.len() == 1
        })
        .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, BackendEvent::SessionClosed { .. }))
    );
    assert_eq!(
        h.tracker.inventory().windows[&first_window].sessions.len(),
        1,
        "the linked window lost its second session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn floating_panes_mark_the_window_unsupported() {
    let mut h = start().await;
    h.change(&["split-window", "-h", "-t", "main", "sleep 600"], |i| {
        pane_count(i) == 2
    })
    .await;
    let ev = h
        .change(
            &["break-pane", "-W", "-s", "main.1", "-x", "20", "-y", "6"],
            |i| {
                i.windows
                    .values()
                    .any(|w| matches!(w.layout, WindowLayout::Unsupported { .. }))
            },
        )
        .await;
    assert!(ev.iter().any(|e| matches!(
        e,
        BackendEvent::WindowLayout { layout: WindowLayout::Unsupported { reason, .. }, .. }
            if reason.contains("floating")
    )));
    // The floating pane is still in the model; nothing was dropped.
    assert_eq!(pane_count(h.tracker.inventory()), 2);
}
