//! Terminal tools for agents through a real daemon and an isolated tmux:
//! act levels (observe, own, elsewhere), approvals, interaction leases,
//! `terminal.run`, `terminal.watch`, and the bounds of `terminal.query`.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");
/// How long an approval waits in these tests.
const APPROVAL_MS: u64 = 3000;

/// Stands in for `dream job-run DIR -- PROGRAM [ARGS...]`, which lives in
/// the CLI crate: it records the wrapper's pid, the program's pid, and its
/// exit status in DIR, then prints the same last line.
const JOB_RUN: &str = r#"#!/bin/sh
dir=$2; shift 3
echo $$ > "$dir/wrapper"
sh -c 'echo $$ > "$0/pid"; exec "$@"' "$dir" "$@"
code=$?
printf '{"exit_code":%d,"ended_unix_ms":0}\n' "$code" > "$dir/.status.tmp"
mv "$dir/.status.tmp" "$dir/status.json"
echo "[job exited with $code]"
exit "$code"
"#;

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-at-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let wrapper = root.join("dream");
        std::fs::write(&wrapper, JOB_RUN).expect("wrapper");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DREAM", &wrapper)
            .env("DREAMER_TEST_APPROVAL_TIMEOUT_MS", APPROVAL_MS.to_string())
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
            match Client::connect_with(&self.root.join("rt/dreamerd.sock"), "test", 1024).await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    /// A connection holding an agent's grant at `level`: every pane, and
    /// the methods of every tool, so the level alone decides what acts.
    async fn agent(&self, level: api::ActLevel) -> Client {
        let c = self.client().await;
        let methods = [
            "tmux.topology",
            "terminal.list",
            "terminal.read",
            "terminal.query",
            "terminal.watch",
            "jobs.get",
            "terminal.input",
            "terminal.run",
            "actions.submit",
        ];
        c.call(Request::GrantsNarrow(api::Grant {
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            all_panes: true,
            level: level as i32,
            ..api::Grant::default()
        }))
        .await
        .expect("narrow");
        c
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

async fn first_pane(c: &Client) -> u32 {
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    t.panes[0].id
}

async fn read(c: &Client, pane: u32) -> Vec<String> {
    match c
        .call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(200),
            endpoint: None,
        }))
        .await
    {
        Ok(Response::TerminalRead(t)) => t.lines,
        other => panic!("{other:?}"),
    }
}

/// Wait until the pane shows a line equal to `line`.
async fn shown(c: &Client, pane: u32, line: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !read(c, pane).await.iter().any(|l| l == line) {
        assert!(Instant::now() < deadline, "%{pane} never showed {line}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn text(pane: u32, t: &str, lease: Option<&str>) -> Request {
    Request::TerminalInput(api::TerminalInput {
        pane,
        lease: lease.map(str::to_owned),
        event: Some(api::terminal_input::Event::Text(api::TextInput {
            text: t.into(),
        })),
        ..api::TerminalInput::default()
    })
}

fn forbidden(r: Result<Response, client::ClientError>, says: &str) {
    match r {
        Err(client::ClientError::Rpc(e)) => {
            assert_eq!(e.code, api::code::FORBIDDEN, "{e}");
            assert!(e.message.contains(says), "{e}");
        }
        other => panic!("expected a refusal saying {says:?}: {other:?}"),
    }
}

/// The one approval waiting, once it appears.
async fn waiting(owner: &Client) -> api::Approval {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Response::ApprovalsList(l)) =
            owner.call(Request::ApprovalsList(api::Empty {})).await
            && let Some(a) = l.approvals.into_iter().next()
        {
            return a;
        }
        assert!(Instant::now() < deadline, "no approval was asked for");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn answer(owner: &Client, id: &str, d: api::ApprovalDecision) {
    owner
        .call(Request::ApprovalsAnswer(api::ApprovalAnswer {
            id: id.into(),
            decision: d as i32,
        }))
        .await
        .expect("answer");
}

async fn lease(c: &Client, pane: u32) -> String {
    match c
        .call(Request::LeasesAcquire(api::LeaseAcquire {
            pane,
            ttl_ms: Some(60_000),
        }))
        .await
    {
        Ok(Response::LeasesAcquire(l)) => l.id,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn observing_needs_no_approval_and_never_acts() {
    let s = Setup::new();
    let owner = s.client().await;
    let pane = first_pane(&owner).await;
    owner
        .call(text(pane, "echo seen-$((6*7))\n", None))
        .await
        .expect("type");
    shown(&owner, pane, "seen-42").await;

    let agent = s.agent(api::ActLevel::Observe).await;
    let Ok(Response::TerminalList(l)) = agent
        .call(Request::TerminalList(api::TopologyParams::default()))
        .await
    else {
        panic!("list")
    };
    let t = l.terminals.iter().find(|t| t.pane == pane).expect("pane");
    assert!(!t.owned, "{t:?}");
    assert!(t.cols > 0 && t.rows > 0, "{t:?}");
    assert_eq!(t.session_name, "main");
    assert!(t.quiet_ms.is_some(), "{t:?}");

    // Reading and searching a pane it did not create: no approval asked.
    assert!(read(&agent, pane).await.iter().any(|l| l == "seen-42"));
    let Ok(Response::TerminalQuery(q)) = agent
        .call(Request::TerminalQuery(api::TerminalQuery {
            pattern: "^seen-[0-9]+$".into(),
            panes: vec![pane],
            ..api::TerminalQuery::default()
        }))
        .await
    else {
        panic!("query")
    };
    assert_eq!(q.matches.len(), 1, "{q:?}");
    let Ok(Response::ApprovalsList(waiting)) =
        owner.call(Request::ApprovalsList(api::Empty {})).await
    else {
        panic!("approvals")
    };
    assert!(waiting.approvals.is_empty());

    // Acting is refused at once, whatever the pane.
    forbidden(agent.call(text(pane, "x", None)).await, "only observes");
    forbidden(
        agent
            .call(Request::LeasesAcquire(api::LeaseAcquire {
                pane,
                ttl_ms: None,
            }))
            .await,
        "only observes",
    );
    forbidden(
        agent
            .call(Request::TerminalRun(api::TerminalRun {
                argv: vec!["true".into()],
                ..api::TerminalRun::default()
            }))
            .await,
        "only observes",
    );
    // An agent cannot see or answer approvals.
    forbidden(
        agent.call(Request::ApprovalsList(api::Empty {})).await,
        "approvals.list",
    );
}

#[tokio::test]
async fn typing_elsewhere_waits_for_approval_and_a_keystroke_takes_the_pane_back() {
    let s = Setup::new();
    let owner = s.client().await;
    let pane = first_pane(&owner).await;

    // At `own`, a pane the agent did not create is refused outright.
    let own = s.agent(api::ActLevel::Own).await;
    forbidden(own.call(text(pane, "x", None)).await, "not created by");
    drop(own);

    let agent = Arc::new(s.agent(api::ActLevel::Elsewhere).await);
    // Taking the lease asks nothing; typing with it does.
    let id = lease(&agent, pane).await;
    let typing = |t: &'static str| {
        let (agent, id) = (agent.clone(), id.clone());
        tokio::spawn(async move { agent.call(text(pane, t, Some(&id))).await })
    };

    // Denied: refused, and nothing reaches the pane.
    let denied = typing("echo nope-$((1+1))\n");
    let a = waiting(&owner).await;
    assert_eq!(a.panes, vec![pane]);
    assert!(!a.closing);
    assert!(a.action.contains(&format!("%{pane}")), "{a:?}");
    answer(&owner, &a.id, api::ApprovalDecision::Deny).await;
    forbidden(denied.await.expect("task"), "did not approve");
    assert!(!read(&owner, pane).await.iter().any(|l| l == "nope-2"));

    // Approved for the pane: it types, and later typing needs no answer.
    let allowed = typing("echo yes-$((2+2))\n");
    let a = waiting(&owner).await;
    answer(&owner, &a.id, api::ApprovalDecision::Pane).await;
    allowed.await.expect("task").expect("typed");
    shown(&owner, pane, "yes-4").await;
    agent
        .call(text(pane, "echo again-$((3+3))\n", Some(&id)))
        .await
        .expect("typed again without asking");
    shown(&owner, pane, "again-6").await;

    // Closing always asks, even with the pane approved; unanswered, it is
    // denied when the wait runs out.
    let started = Instant::now();
    let close = agent
        .call(Request::ActionsSubmit(api::SubmitAction {
            action: Some(api::Action {
                kind: Some(api::action::Kind::KillPane(api::KillPane { pane })),
            }),
            ..api::SubmitAction::default()
        }))
        .await;
    forbidden(close, "no answer in time");
    assert!(started.elapsed() >= Duration::from_millis(APPROVAL_MS - 100));
    assert!(read(&owner, pane).await.iter().any(|l| l == "again-6"));

    // A person's keystroke revokes the lease: the agent's next input fails.
    owner.call(text(pane, " ", None)).await.expect("person");
    forbidden_precondition(
        agent.call(text(pane, "echo late\n", Some(&id))).await,
        "a person typed into the pane",
    );
}

fn forbidden_precondition(r: Result<Response, client::ClientError>, says: &str) {
    match r {
        Err(client::ClientError::Rpc(e)) => {
            assert_eq!(e.code, api::code::PRECONDITION, "{e}");
            assert!(e.message.contains(says), "{e}");
        }
        other => panic!("expected a refusal saying {says:?}: {other:?}"),
    }
}

#[tokio::test]
async fn a_run_in_its_own_pane_returns_status_and_output() {
    let s = Setup::new();
    let owner = s.client().await;
    let agent = s.agent(api::ActLevel::Own).await;

    let run = |argv: &[&str], timeout_ms: u32| {
        Request::TerminalRun(api::TerminalRun {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            timeout_ms: Some(timeout_ms),
            ..api::TerminalRun::default()
        })
    };
    let Ok(Response::TerminalRun(r)) = agent
        .call(run(
            &["/bin/sh", "-c", "echo hello; echo world >&2; exit 3"],
            10_000,
        ))
        .await
    else {
        panic!("run")
    };
    let job = r.job.clone().expect("job");
    assert!(r.finished, "{r:?}");
    assert_eq!(job.state, api::JobState::Failed as i32);
    assert_eq!(job.exit_code, Some(3));
    assert_eq!(r.output, vec!["hello", "world"], "{r:?}");

    // A run that outlasts its wait keeps going in a pane the agent owns:
    // it may type there without approval, and watch it end.
    let Ok(Response::TerminalRun(r)) = agent
        .call(run(&["/bin/sh", "-c", "read x; echo got-$x; exit 0"], 300))
        .await
    else {
        panic!("run")
    };
    assert!(!r.finished, "{r:?}");
    let pane = r.job.as_ref().expect("job").pane;
    let Ok(Response::TerminalList(l)) = agent
        .call(Request::TerminalList(api::TopologyParams::default()))
        .await
    else {
        panic!("list")
    };
    assert!(l.terminals.iter().any(|t| t.pane == pane && t.owned));
    agent
        .call(text(pane, "yes\n", None))
        .await
        .expect("typing in its own pane");
    let Ok(Response::TerminalWatch(w)) = agent
        .call(Request::TerminalWatch(api::TerminalWatch {
            pane,
            exit: true,
            timeout_ms: Some(10_000),
            ..api::TerminalWatch::default()
        }))
        .await
    else {
        panic!("watch")
    };
    assert_eq!(w.outcome, api::WatchOutcome::Exited as i32, "{w:?}");
    assert_eq!(w.exit_code, Some(0));
    shown(&owner, pane, "got-yes").await;
    let Ok(Response::ApprovalsList(waiting)) =
        owner.call(Request::ApprovalsList(api::Empty {})).await
    else {
        panic!("approvals")
    };
    assert!(waiting.approvals.is_empty());
}

#[tokio::test]
async fn watch_returns_on_a_pattern_on_quiet_and_on_timeout() {
    let s = Setup::new();
    let owner = s.client().await;
    let pane = first_pane(&owner).await;
    let agent = s.agent(api::ActLevel::Observe).await;
    let watch = |w: api::TerminalWatch| {
        let req = Request::TerminalWatch(api::TerminalWatch { pane, ..w });
        async {
            match agent.call(req).await {
                Ok(Response::TerminalWatch(r)) => r,
                other => panic!("{other:?}"),
            }
        }
    };

    // A pattern in output written after the watch starts. The command
    // line itself does not match.
    owner
        .call(text(pane, "sleep 0.5; echo READY-$((40+2))\n", None))
        .await
        .expect("type");
    let r = watch(api::TerminalWatch {
        pattern: Some("READY-[0-9]+".into()),
        timeout_ms: Some(10_000),
        ..api::TerminalWatch::default()
    })
    .await;
    assert_eq!(r.outcome, api::WatchOutcome::Matched as i32, "{r:?}");
    assert_eq!(r.text.as_deref(), Some("READY-42"));
    assert_eq!((r.match_start, r.match_end), (0, 8));

    // Quiet: the pane writes for about a second, then stops.
    owner
        .call(text(
            pane,
            "for i in 1 2 3 4 5 6; do echo tick$i; sleep 0.2; done\n",
            None,
        ))
        .await
        .expect("type");
    shown(&owner, pane, "tick1").await;
    let started = Instant::now();
    let r = watch(api::TerminalWatch {
        quiet_ms: Some(600),
        timeout_ms: Some(10_000),
        ..api::TerminalWatch::default()
    })
    .await;
    assert_eq!(r.outcome, api::WatchOutcome::Quiet as i32, "{r:?}");
    assert!(read(&owner, pane).await.iter().any(|l| l == "tick6"));
    assert!(started.elapsed() >= Duration::from_millis(600));

    // Timeout: nothing happens.
    let r = watch(api::TerminalWatch {
        pattern: Some("NEVER-SHOWN".into()),
        timeout_ms: Some(300),
        ..api::TerminalWatch::default()
    })
    .await;
    assert_eq!(r.outcome, api::WatchOutcome::Timeout as i32, "{r:?}");
    assert!(r.elapsed_ms >= 300, "{r:?}");

    // A watch needs a condition.
    let e = agent
        .call(Request::TerminalWatch(api::TerminalWatch {
            pane,
            ..api::TerminalWatch::default()
        }))
        .await
        .expect_err("no condition");
    assert!(e.to_string().contains("pattern"), "{e}");
}

#[tokio::test]
async fn search_is_bounded_in_matches_lines_and_bytes() {
    let s = Setup::new();
    let owner = s.client().await;
    let pane = first_pane(&owner).await;
    s.tmux(&["split-window", "-t", "main", "/bin/sh"]);
    owner
        .call(text(
            pane,
            "i=0; while [ $i -lt 50 ]; do echo row-$i-xxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done; echo d$((1))one\n",
            None,
        ))
        .await
        .expect("type");
    shown(&owner, pane, "d1one").await;
    let agent = s.agent(api::ActLevel::Observe).await;
    let search = |q: api::TerminalQuery| async {
        match agent.call(Request::TerminalQuery(q)).await {
            Ok(Response::TerminalQuery(r)) => r,
            other => panic!("{other:?}"),
        }
    };
    let q = api::TerminalQuery {
        pattern: "^row-[0-9]+-x+$".into(),
        ..api::TerminalQuery::default()
    };

    // Every pane, unbounded enough: all 50 rows.
    let r = search(q.clone()).await;
    assert_eq!(r.matches.len(), 50, "{r:?}");
    assert_eq!(r.panes_searched, 2);
    assert!(!r.truncated);

    // Bytes: each row is 26 or 27 bytes, so 100 bytes hold three.
    let r = search(api::TerminalQuery {
        max_bytes: Some(100),
        ..q.clone()
    })
    .await;
    assert_eq!(r.matches.len(), 3, "{r:?}");
    assert!(r.truncated && r.notes.iter().any(|n| n.starts_with("max_bytes")));

    // Matches, and the newest lines searched.
    let r = search(api::TerminalQuery {
        max_matches: Some(5),
        ..q.clone()
    })
    .await;
    assert_eq!(r.matches.len(), 5);
    assert!(r.truncated && r.notes.iter().any(|n| n.starts_with("max_matches")));
    let r = search(api::TerminalQuery {
        panes: vec![pane],
        lines: Some(10),
        ..q
    })
    .await;
    // The last ten lines: rows 42 to 49, the done line, and the prompt.
    assert_eq!(r.matches.len(), 8, "{r:?}");
}
