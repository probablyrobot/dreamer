//! The user's prefix and mode keys, read from tmux, and keys typed after
//! the prefix reaching tmux's own bindings (baseline 3.8). Every server
//! here is isolated; nothing touches the user's tmux.

use domain::TmuxPaneId;
use tmux_backend::ops::{self, KeyOptions};
use tmux_backend::server::tmux_program;
use tmux_backend::{Connection, ConnectionConfig, Socket, TmuxServer};

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

async fn connect(server: &TmuxServer) -> Connection {
    let (conn, mut events) = Connection::spawn(config(server)).await.expect("attach");
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    conn
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_prefix_and_mode_keys_are_read_and_left_alone() {
    let server = TmuxServer::start(80, 24, "sh").expect("tmux");
    server
        .run_checked(&["set-option", "-g", "prefix", "C-a"])
        .expect("prefix");
    server
        .run_checked(&["set-option", "-g", "prefix2", "F12"])
        .expect("prefix2");
    server
        .run_checked(&["set-option", "-gw", "mode-keys", "vi"])
        .expect("mode-keys");
    server
        .run_checked(&["set-option", "-gw", "mode-style", "bg=red,fg=white"])
        .expect("mode-style");
    let conn = connect(&server).await;
    let got = ops::key_options(&conn).await.expect("options");
    assert_eq!(
        got,
        KeyOptions {
            prefix: "C-a".into(),
            prefix2: "F12".into(),
            mode_keys: "vi".into(),
            mode_style: "bg=red,fg=white".into(),
        }
    );
    // Following a change: read again.
    server
        .run_checked(&["set-option", "-gw", "mode-keys", "emacs"])
        .expect("mode-keys");
    assert_eq!(
        ops::key_options(&conn).await.expect("options").mode_keys,
        "emacs"
    );
    assert_eq!(
        server
            .run_checked(&["show-options", "-gv", "prefix"])
            .expect("show")
            .trim(),
        "C-a"
    );
}

fn windows(server: &TmuxServer) -> Vec<String> {
    server
        .run_checked(&["list-windows", "-a", "-F", "#{session_name}:#{window_name}"])
        .expect("list")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_after_a_custom_prefix_runs_the_users_binding_on_that_pane() {
    let server = TmuxServer::start(80, 24, "sh").expect("tmux");
    server
        .run_checked(&["new-session", "-d", "-s", "other", "sh"])
        .expect("session");
    server
        .run_checked(&["set-option", "-g", "prefix", "C-a"])
        .expect("prefix");
    // The user's own binding for prefix x.
    server
        .run_checked(&["bind-key", "-T", "prefix", "x", "new-window", "-n", "mine"])
        .expect("bind");
    let other: u32 = server
        .run_checked(&["display-message", "-p", "-t", "other:", "#{pane_id}"])
        .expect("pane")
        .trim()
        .trim_start_matches('%')
        .parse()
        .expect("pane id");
    let conn = connect(&server).await;
    let out = ops::prefix_key(&config(&server), TmuxPaneId(other), "x")
        .await
        .expect("prefix key");
    assert_eq!(out.mode_cancelled, None);
    // The binding ran on the pane's session, not the session the daemon's
    // own client is attached to.
    let w = windows(&server);
    assert!(w.contains(&"other:mine".to_owned()), "{w:?}");
    assert!(!w.contains(&"main:mine".to_owned()), "{w:?}");
    // A default binding (prefix c) works the same way.
    ops::prefix_key(&config(&server), TmuxPaneId(other), "c")
        .await
        .expect("prefix c");
    assert_eq!(
        windows(&server)
            .iter()
            .filter(|w| w.starts_with("other:"))
            .count(),
        3
    );
    // The prefix is untouched, and the short-lived client is gone.
    assert_eq!(
        ops::key_options(&conn).await.expect("options").prefix,
        "C-a"
    );
    let clients = server
        .run_checked(&["list-clients", "-F", "#{client_flags}"])
        .expect("clients");
    assert_eq!(clients.lines().count(), 1, "{clients}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binding_that_opens_a_tmux_mode_is_cancelled_and_named() {
    let server = TmuxServer::start(80, 24, "sh").expect("tmux");
    let pane: u32 = server
        .run_checked(&["display-message", "-p", "-t", "main:", "#{pane_id}"])
        .expect("pane")
        .trim()
        .trim_start_matches('%')
        .parse()
        .expect("pane id");
    // prefix PageUp is `copy-mode -u` in tmux's defaults.
    let out = ops::prefix_key(&config(&server), TmuxPaneId(pane), "PPage")
        .await
        .expect("prefix key");
    assert_eq!(out.mode_cancelled.as_deref(), Some("copy-mode"));
    let in_mode = server
        .run_checked(&["display-message", "-p", "-t", "main:", "#{pane_in_mode}"])
        .expect("mode");
    assert_eq!(in_mode.trim(), "0");
}

#[tokio::test(flavor = "multi_thread")]
async fn paste_buffers_are_listed_newest_first() {
    let server = TmuxServer::start(80, 24, "sh").expect("tmux");
    server
        .run_checked(&["set-buffer", "first"])
        .expect("buffer");
    server
        .run_checked(&["set-buffer", "second one"])
        .expect("buffer");
    let conn = connect(&server).await;
    let got = ops::list_buffers(&conn).await.expect("list");
    let samples: Vec<(&str, u64)> = got.iter().map(|b| (b.sample.as_str(), b.size)).collect();
    assert_eq!(samples, vec![("second one", 10), ("first", 5)]);
}
