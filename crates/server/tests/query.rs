//! `terminal.query` through a real daemon (spec 12.8): matching panes are
//! found, every bound holds and is reported, bad patterns are refused, and
//! queries never hold up input.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
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
            "/tmp/drm-q-{}-{}",
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
        self.client_with(1024).await
    }

    async fn client_with(&self, pushed: usize) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect_with(&self.root.join("rt/dreamerd.sock"), "test", pushed).await {
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

fn query(pattern: &str) -> api::TerminalQuery {
    api::TerminalQuery {
        pattern: pattern.into(),
        ..api::TerminalQuery::default()
    }
}

async fn ask(c: &Client, q: api::TerminalQuery) -> Result<api::QueryResult, client::ClientError> {
    match c.call(Request::TerminalQuery(q)).await? {
        Response::TerminalQuery(r) => Ok(r),
        other => panic!("{other:?}"),
    }
}

/// Wait until a pane shows anything (its shell's prompt).
async fn prompted(c: &Client, pane: u32) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Response::TerminalRead(t)) = c
            .call(Request::TerminalRead(api::ReadParams {
                pane,
                history: Some(10),
                endpoint: None,
            }))
            .await
            && t.lines.iter().any(|l| !l.trim().is_empty())
        {
            return;
        }
        assert!(Instant::now() < deadline, "%{pane} never showed a prompt");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until every pane shows its marker. Shells in many panes start
/// slowly under emulation (the arm64 gate stage), so the wait is generous.
async fn shown(c: &Client, panes: &[(u32, &str)]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    for (pane, marker) in panes {
        loop {
            if let Ok(Response::TerminalRead(t)) = c
                .call(Request::TerminalRead(api::ReadParams {
                    pane: *pane,
                    history: Some(100),
                    endpoint: None,
                }))
                .await
                && t.lines.iter().any(|l| l == marker)
            {
                break;
            }
            assert!(Instant::now() < deadline, "%{pane} never showed {marker}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test]
async fn queries_find_panes_within_bounds_and_never_hold_up_input() {
    let s = Setup::new();
    let c = s.client().await;
    s.tmux(&["split-window", "-t", "main", "/bin/sh"]);
    s.tmux(&["split-window", "-t", "main", "/bin/sh"]);
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let panes: Vec<u32> = t.panes.iter().map(|p| p.id).collect();
    assert_eq!(panes.len(), 3);
    // A shell that is still starting can drop what is typed ahead of it:
    // wait for each prompt first.
    for pane in &panes {
        prompted(&c, *pane).await;
    }
    let say =
        |pane: u32, cmd: &str| s.tmux(&["send-keys", "-t", &format!("%{pane}"), cmd, "Enter"]);
    say(panes[0], "printf 'build FAILED: x\\nok\\n'; echo m0");
    say(panes[1], "printf 'all good\\n'; echo m1");
    say(
        panes[2],
        "for i in 1 2 3 4 5; do echo error $i; done; echo m2",
    );
    shown(&c, &[(panes[0], "m0"), (panes[1], "m1"), (panes[2], "m2")]).await;

    // Matching lines, per pane, with offsets.
    let r = ask(&c, query(r"FAILED|^error [0-9]$"))
        .await
        .expect("query");
    assert_eq!(r.panes_searched, 3);
    assert!(!r.truncated, "{r:?}");
    let in_pane = |p: u32| r.matches.iter().filter(|m| m.pane == p).count();
    // Pane 0's command line contains FAILED too.
    assert_eq!(
        (in_pane(panes[0]), in_pane(panes[1]), in_pane(panes[2])),
        (2, 0, 5)
    );
    let failed = r
        .matches
        .iter()
        .find(|m| m.pane == panes[0] && m.text == "build FAILED: x")
        .expect("match");
    assert_eq!(failed.text, "build FAILED: x");
    assert_eq!((failed.match_start, failed.match_end), (6, 12));

    // Bounds: matches, and the window of lines.
    let mut q = query("^error");
    q.max_matches = Some(2);
    let r = ask(&c, q).await.expect("query");
    assert_eq!(r.matches.len(), 2);
    assert!(
        r.truncated && r.notes.iter().any(|n| n.contains("max_matches")),
        "{r:?}"
    );
    let mut q = query("^error 1$");
    q.panes = vec![panes[2]];
    q.lines = Some(3);
    let r = ask(&c, q).await.expect("query");
    assert!(
        r.matches.is_empty(),
        "error 1 is outside the last 3 lines: {r:?}"
    );

    // Literal sets, case, and refused patterns.
    let mut q = query("");
    q.literals = vec!["ALL GOOD".into()];
    q.ignore_case = true;
    let r = ask(&c, q).await.expect("query");
    // The output line and the command that printed it.
    assert_eq!(r.matches.len(), 2, "{r:?}");
    assert!(r.matches.iter().all(|m| m.pane == panes[1]));
    assert!(r.matches.iter().any(|m| m.text == "all good"));
    let e = ask(&c, query("(a{1000}){1000}"))
        .await
        .expect_err("too complex");
    assert!(e.to_string().contains("too complex"), "{e}");
    assert!(ask(&c, query("(unclosed")).await.is_err());

    // Input keeps flowing while queries run back to back.
    let busy = {
        let c = s.client().await;
        tokio::spawn(async move {
            for _ in 0..40 {
                let mut q = query(".");
                q.lines = Some(2000);
                let _ = ask(&c, q).await;
            }
        })
    };
    for i in 0..10 {
        let started = Instant::now();
        c.call(Request::TerminalInput(api::TerminalInput {
            pane: panes[1],
            event: Some(api::terminal_input::Event::Text(api::TextInput {
                text: format!("echo in{i}\n"),
            })),
            ..api::TerminalInput::default()
        }))
        .await
        .expect("input");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "input took {:?} during queries",
            started.elapsed()
        );
    }
    shown(&c, &[(panes[1], "in9")]).await;
    busy.await.expect("queries");
}
