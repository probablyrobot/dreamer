//! Applying workspace files to real tmux (baseline 3.8).

use std::time::Duration;

use tmux_backend::server::tmux_program;
use tmux_backend::workspace::{ApplyError, apply};
use tmux_backend::{Connection, ConnectionConfig, Socket, TmuxCommand, TmuxServer};

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

fn lines(r: &tmux_protocol::Response) -> Vec<String> {
    r.body
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

async fn capture(conn: &Connection, pane: &str) -> String {
    let r = conn
        .execute(
            TmuxCommand::new("capture-pane")
                .flag("-p")
                .flag("-J")
                .opt("-S", "-")
                .opt("-t", pane),
        )
        .await
        .expect("capture");
    lines(&r[0]).join("\n")
}

async fn wait_for_text(conn: &Connection, pane: &str, text: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let shown = capture(conn, pane).await;
        if shown.contains(text) {
            return shown;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pane {pane} never showed `{text}`:\n{shown}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_file_becomes_the_declared_session() {
    let server = TmuxServer::start(100, 30, "sleep 600").expect("tmux");
    server
        .run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
        .expect("shell");
    let (conn, _events) = Connection::spawn(config(&server)).await.expect("connect");

    let dir = std::env::temp_dir().join(format!("dreamer-ws-apply-{}", std::process::id()));
    let sub = dir.join("sub dir; $x");
    std::fs::create_dir_all(&sub).expect("dirs");
    let yaml = r#"
version: 1
name: "dev ~ $HOME"
env:
  WS_SHARED: from-workspace
windows:
  - name: "code; #{pane_id}"
    layout:
      split: horizontal
      panes:
        - size: 60%
          title: "editor 'quoted'"
          keep: true
          shell: 'printf "left:%s:%s\n" "$WS_SHARED" "$(pwd)"'
        - split: vertical
          panes:
            - title: right-top
              cwd: "sub dir; $x"
              env: { WS_PANE: "pane value with spaces" }
              keep: true
              shell: 'printf "rt:%s:%s\n" "$WS_PANE" "$(pwd)"'
            - title: right-bottom
              focus: true
              run: 'echo typed-into-shell'
  - name: second
    focus: true
    panes:
      - exec: [sleep]
        keep: true
      - exec: [sh, -c, 'echo exec-argv-ok; sleep 600']
    arrange: even-horizontal
"#;
    let path = dir.join("dev.yaml");
    std::fs::write(&path, yaml).expect("write");
    let plan = workspace_config::load(&path).expect("valid workspace");

    let applied = apply(&conn, &plan).await.expect("apply");
    assert_eq!(applied.windows.len(), 2);
    assert_eq!(applied.windows[0].1.len(), 3);
    assert_eq!(applied.windows[1].1.len(), 2);

    // Session and window names survive hostile characters.
    let sessions = conn
        .execute(TmuxCommand::new("list-sessions").opt_format("-F", "#{session_name}"))
        .await
        .expect("sessions");
    assert!(lines(&sessions[0]).contains(&"dev ~ $HOME".to_owned()));
    let session = applied.session.to_string();
    let windows = conn
        .execute(
            TmuxCommand::new("list-windows")
                .opt("-t", &session)
                .opt_format("-F", "#{window_name}|#{window_active}"),
        )
        .await
        .expect("windows");
    assert_eq!(
        lines(&windows[0]),
        ["code; #{pane_id}|0".to_owned(), "second|1".to_owned()]
    );

    // Geometry: the editor takes 60% of the 100 columns.
    let (code_window, code_panes) = &applied.windows[0];
    let panes = conn
        .execute(
            TmuxCommand::new("list-panes")
                .opt("-t", code_window.to_string())
                .opt_format(
                    "-F",
                    "#{pane_id}|#{pane_width}|#{pane_title}|#{pane_active}",
                ),
        )
        .await
        .expect("panes");
    let rows = lines(&panes[0]);
    let row = |id: &domain::TmuxPaneId| {
        rows.iter()
            .find(|r| r.starts_with(&format!("{id}|")))
            .cloned()
            .unwrap_or_default()
    };
    let width: u32 = lines(
        &conn
            .execute(
                TmuxCommand::new("display-message")
                    .flag("-p")
                    .opt("-t", code_window.to_string())
                    .format("#{window_width}"),
            )
            .await
            .expect("width")[0],
    )[0]
    .parse()
    .expect("number");
    let editor = row(&code_panes[0]);
    assert!(
        editor.contains(&format!("|{}|editor 'quoted'|", width * 60 / 100)),
        "{editor} (window width {width})"
    );
    assert!(row(&code_panes[1]).contains("|right-top|"), "{rows:?}");
    assert!(row(&code_panes[2]).contains("|right-bottom|1"), "{rows:?}");

    // Environment and working directories reached the processes.
    let left = wait_for_text(&conn, &code_panes[0].to_string(), "left:").await;
    assert!(left.contains("left:from-workspace:"), "{left}");
    let rt = wait_for_text(&conn, &code_panes[1].to_string(), "rt:").await;
    assert!(rt.contains("rt:pane value with spaces:"), "{rt}");
    assert!(rt.contains("sub dir; $x"), "{rt}");

    // `run` typed a command into a live shell.
    wait_for_text(&conn, &code_panes[2].to_string(), "typed-into-shell").await;

    // `exec` with a multi-word argv ran without a shell of its own.
    let second = &applied.windows[1].1;
    wait_for_text(&conn, &second[1].to_string(), "exec-argv-ok").await;

    // Applying again is refused and changes nothing.
    match apply(&conn, &plan).await {
        Err(ApplyError::SessionExists(name)) => assert_eq!(name, "dev ~ $HOME"),
        other => panic!("expected SessionExists, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
