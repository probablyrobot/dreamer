//! The async control connection against real tmux (baseline 3.8), on
//! isolated servers.

use std::time::Duration;

use tmux_backend::server::tmux_program;
use tmux_backend::{
    Batch, CommandError, ConnEvent, Connection, ConnectionConfig, LinkState, Socket, TmuxCommand,
    TmuxServer,
};
use tmux_protocol::Notification;

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
        ("LANG".into(), "C.UTF-8".into()),
    ]);
    c
}

fn body(r: &tmux_protocol::Response) -> String {
    r.body
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strings that would break or subvert a naively built command line.
const HOSTILE: &[&str] = &[
    "plain",
    "two words",
    "it's",
    "say \"hi\"",
    "semi;colon ; kill-server",
    "$HOME and ${USER}",
    "~ and ~root",
    "#{pane_id} #[fg=red] ##",
    "back\\slash \\n \\033",
    "brace { } }",
    "-starts-with-dash",
    "%percent %if",
    "tab\there",
    "new\nline\nkill-server",
    "carriage\rreturn",
    "esc \u{1b}[31m",
    "emoji 🚀 é 世界",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_strings_round_trip_exactly() {
    let server = TmuxServer::start(80, 24, "sleep 60").expect("tmux");
    let (conn, _events) = Connection::spawn(config(&server)).await.expect("connect");
    for (i, value) in HOSTILE.iter().enumerate() {
        let name = format!("t{i}");
        conn.execute(TmuxCommand::new("set-buffer").opt("-b", &name).arg(*value))
            .await
            .unwrap_or_else(|e| panic!("set-buffer {value:?}: {e}"));
        let shown = conn
            .execute(TmuxCommand::new("show-buffer").opt("-b", &name))
            .await
            .expect("show-buffer");
        assert_eq!(body(&shown[0]), *value, "buffer round trip for {value:?}");
    }
    // The server survived every "kill-server" smuggled into a value.
    let alive = conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .format("#{session_name}"),
        )
        .await
        .expect("alive");
    assert_eq!(body(&alive[0]), "main");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_window_names_round_trip() {
    let server = TmuxServer::start(80, 24, "sleep 60").expect("tmux");
    let (conn, _events) = Connection::spawn(config(&server)).await.expect("connect");
    for value in HOSTILE.iter().filter(|v| !v.chars().any(char::is_control)) {
        // rename-window expands formats in the name, so literal names go
        // through the format-literal encoding.
        conn.execute(
            TmuxCommand::new("rename-window")
                .opt("-t", "main")
                .literal_for_format(*value),
        )
        .await
        .unwrap_or_else(|e| panic!("rename {value:?}: {e}"));
        let shown = conn
            .execute(
                TmuxCommand::new("display-message")
                    .flag("-p")
                    .opt("-t", "main")
                    .format("#{window_name}"),
            )
            .await
            .expect("display");
        // tmux stores names in escaped form: backslashes doubled (it
        // passes names through strvis), and a literal `#[` (which would
        // otherwise start a style directive) as `##[`. The exact bytes reach
        // tmux; `hostile_strings_round_trip_exactly` proves that.
        let stored = value.replace('\\', "\\\\").replace("#[", "##[");
        assert_eq!(body(&shown[0]), stored, "window name {value:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_command_reports_its_index_and_the_stream_stays_aligned() {
    let server = TmuxServer::start(80, 24, "sleep 60").expect("tmux");
    let (conn, _events) = Connection::spawn(config(&server)).await.expect("connect");
    let batch = Batch::new()
        .push(TmuxCommand::new("display-message").flag("-p").arg("first"))
        .push(TmuxCommand::new("select-window").opt("-t", "@999"))
        .push(TmuxCommand::new("display-message").flag("-p").arg("never"));
    match conn.execute(batch).await {
        Err(CommandError::Failed { index, response }) => {
            assert_eq!(index, 1);
            assert!(body(&response).contains("@999"), "{}", body(&response));
        }
        other => panic!("expected failure, got {other:?}"),
    }
    let next = conn
        .execute(TmuxCommand::new("display-message").flag("-p").arg("next"))
        .await
        .expect("next");
    assert_eq!(body(&next[0]), "next");
    assert_eq!(conn.state(), LinkState::Ready);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_is_reported_once_never_retried_and_reconciled_late() {
    let server = TmuxServer::start(80, 24, "sleep 60").expect("tmux");
    let mut cfg = config(&server);
    cfg.command_timeout = Duration::from_millis(300);
    let (conn, mut events) = Connection::spawn(cfg).await.expect("connect");

    // tmux runs `run-shell` in the foreground of the command queue, so the
    // batch cannot finish before its deadline.
    let slow = Batch::new()
        .push(TmuxCommand::new("run-shell").arg("sleep 1.5"))
        .push(
            TmuxCommand::new("new-window")
                .flag("-d")
                .opt("-n", "made-once"),
        );
    let id = match conn.execute(slow).await {
        Err(CommandError::Timeout { batch_id }) => batch_id,
        other => panic!("expected timeout, got {other:?}"),
    };
    assert_eq!(conn.state(), LinkState::Recovering);

    // A command submitted now waits behind the drain instead of being
    // interleaved with the late responses.
    let conn2 = conn.clone();
    let waiting = tokio::spawn(async move {
        conn2
            .execute(TmuxCommand::new("display-message").flag("-p").arg("after"))
            .await
    });

    let late = loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("late outcome")
        {
            Some(ConnEvent::LateOutcome {
                batch_id,
                responses,
            }) => break (batch_id, responses),
            Some(_) => {}
            None => panic!("connection closed"),
        }
    };
    assert_eq!(late.0, id);
    assert_eq!(late.1.len(), 2);
    assert!(late.1.iter().all(|r| r.ok));

    let after = waiting.await.expect("join").expect("after");
    assert_eq!(body(&after[0]), "after");
    assert_eq!(conn.state(), LinkState::Ready);

    let names = conn
        .execute(
            TmuxCommand::new("list-windows")
                .opt("-t", "main")
                .opt_format("-F", "#{window_name}"),
        )
        .await
        .expect("list");
    let made = names[0]
        .body
        .iter()
        .filter(|l| l.as_slice() == b"made-once")
        .count();
    assert_eq!(made, 1, "the timed-out batch ran exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_arrive_in_order() {
    let server = TmuxServer::start(80, 24, "sleep 60").expect("tmux");
    let (conn, mut events) = Connection::spawn(config(&server)).await.expect("connect");
    conn.execute(TmuxCommand::new("new-window").flag("-d").opt("-n", "one"))
        .await
        .expect("new-window");
    conn.execute(
        TmuxCommand::new("rename-window")
            .opt("-t", "main:1")
            .arg("renamed"),
    )
    .await
    .expect("rename");
    let mut saw_add = None;
    let mut saw_rename = None;
    let mut index = 0;
    while saw_rename.is_none() {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event")
            .expect("open");
        if let ConnEvent::Notification(n) = event {
            match n {
                Notification::WindowAdd { .. } if saw_add.is_none() => saw_add = Some(index),
                Notification::WindowRenamed { name, .. } if name == "renamed" => {
                    saw_rename = Some(index)
                }
                _ => {}
            }
            index += 1;
        }
    }
    assert!(saw_add.expect("window-add") < saw_rename.expect("rename"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_whose_server_exits_is_failed_before_any_command() {
    let server = TmuxServer::start(80, 24, "sleep 60").expect("tmux");
    let (conn, mut events) = Connection::spawn(config(&server)).await.expect("connect");
    assert_eq!(conn.state(), LinkState::Ready);
    server.run_checked(&["kill-server"]).expect("kill");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event")
            .expect("open");
        if matches!(event, ConnEvent::Closed { .. }) {
            break;
        }
    }
    // A new server on the same socket would reuse the old ids: the owner
    // must see the link failed and reconnect, not send on it.
    assert_eq!(conn.state(), LinkState::Failed);
}
