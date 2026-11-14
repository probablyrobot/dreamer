//! The example extension under the real supervisor: it answers the
//! daemon's handshake and heartbeats, receives events, and its summaries
//! reach a native subscriber with its stamped identity.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use client::{Client, Pushed};
use prost::Message;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

/// `target/<profile>`, from this test executable in `target/<profile>/deps`.
fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("exe");
    exe.parent()
        .and_then(|deps| deps.parent())
        .expect("target dir")
        .to_path_buf()
}

#[tokio::test]
async fn the_example_extension_runs_under_the_supervisor() {
    let example = target_dir().join("examples/event-summary");
    let daemon = target_dir().join("dreamerd");
    assert!(example.exists(), "{} is missing", example.display());
    assert!(
        daemon.exists(),
        "{} is missing; build it with `cargo build -p dreamer-server`",
        daemon.display()
    );
    let root = PathBuf::from(format!("/tmp/drm-sdk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).expect("config");
    std::fs::write(
        root.join("config/extensions.yaml"),
        format!(
            "extensions:\n  - name: summary\n    argv: [{}]\n    grant:\n      methods: [events.subscribe, messages.publish]\n      all_panes: true\n",
            example.display()
        ),
    )
    .expect("extensions.yaml");
    let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
    let mut d = Command::new(&daemon)
        .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
        .arg(tmux_program())
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .env("DREAMER_TEST_EXTENSION_HEARTBEAT_MS", "200")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dreamerd");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut c = loop {
        match Client::connect(&root.join("rt/dreamerd.sock"), "test").await {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("daemon: {e}"),
        }
    };
    let mut pushed = c.pushed().expect("pushed");
    // Running, after several heartbeats.
    let running = loop {
        let Ok(api::Response::ExtensionsList(l)) =
            c.call(api::Request::ExtensionsList(api::Empty {})).await
        else {
            panic!("list")
        };
        let x = l.extensions[0].clone();
        if x.state == api::ExtensionState::Running as i32 {
            break x;
        }
        assert!(Instant::now() < deadline, "never running: {x:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(running.version.as_deref(), Some("event-summary 0.1"));
    tokio::time::sleep(Duration::from_millis(700)).await;
    c.call(api::Request::MessagesSubscribe(api::MessageSubscribe {
        topics: vec!["summary.*".into()],
    }))
    .await
    .expect("subscribe");
    // Reaching tmux and changing it makes topology events.
    c.call(api::Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology");
    tmux.run_checked(&["new-window", "-d", "-t", "main", "/bin/sh"])
        .expect("tmux");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let envelope = loop {
        match tokio::time::timeout_at(deadline, pushed.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "messages.deliver" => {
                let e = api::MessageDelivery::decode(payload.as_slice())
                    .expect("delivery")
                    .envelope
                    .expect("envelope");
                let text = String::from_utf8_lossy(&e.payload).into_owned();
                if text.contains("tmux.topology") {
                    break e;
                }
            }
            Ok(Some(_)) => {}
            other => panic!("no summary: {other:?}"),
        }
    };
    let sender = envelope.sender.expect("sender");
    assert_eq!(sender.instance, "extension:summary");
    assert!(!sender.owner);
    let Ok(api::Response::ExtensionsList(l)) =
        c.call(api::Request::ExtensionsList(api::Empty {})).await
    else {
        panic!("list")
    };
    assert_eq!(
        l.extensions[0].restarts, 0,
        "heartbeats were answered: {l:?}"
    );
    assert_eq!(l.extensions[0].state, api::ExtensionState::Running as i32);

    let _ = c.call(api::Request::DaemonShutdown(api::Empty {})).await;
    let _ = d.wait();
    let log = std::fs::read_to_string(root.join("st/extensions/summary.log")).unwrap_or_default();
    assert!(log.contains("event-summary: stopping"), "{log}");
    let _ = std::fs::remove_dir_all(&root);
}
