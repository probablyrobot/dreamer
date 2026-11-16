//! pi sessions over RPC, against the scripted stand-in (`testdata/fake-pi`):
//! the start check, a turn with a tool call and an approval, cancelling,
//! resuming with pi's own session, no API key in reach, and closing that
//! ends the process.
//!
//! `pi_contract` runs against a real pi when `DREAMER_PI_CONTRACT` names its
//! executable; it needs no login and makes no model call.

use std::path::{Path, PathBuf};
use std::time::Duration;

use providers::pi_rpc;
use providers::session::{Harness, Input, Spec, StopReason, ToolStatus, Update};

fn fake() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/fake-pi")
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pi-session-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    d
}

fn spec(d: &Path) -> Spec {
    Spec {
        program: PathBuf::from("/bin/sh"),
        args: vec![fake().display().to_string()],
        cwd: d.to_path_buf(),
        env: vec![("OPENAI_API_KEY".into(), "sk-must-not-arrive".into())],
        agent_dir: Some(d.to_path_buf()),
        ..Spec::default()
    }
}

/// Updates until the turn ends (inclusive).
async fn turn(h: &mut Harness) -> Vec<Update> {
    let mut out = Vec::new();
    loop {
        let u = tokio::time::timeout(Duration::from_secs(20), h.updates.recv())
            .await
            .expect("an update in time")
            .expect("the harness runs");
        let end = matches!(u, Update::TurnEnd(_) | Update::Exited(_));
        out.push(u);
        if end {
            return out;
        }
    }
}

async fn until_approval(h: &mut Harness) -> (Vec<Update>, String) {
    let mut out = Vec::new();
    loop {
        let u = tokio::time::timeout(Duration::from_secs(20), h.updates.recv())
            .await
            .expect("an update in time")
            .expect("the harness runs");
        if let Update::Approval(a) = &u {
            let id = a.id.clone();
            out.push(u);
            return (out, id);
        }
        out.push(u);
    }
}

#[tokio::test]
async fn a_session_streams_turns_with_tools_approvals_and_cancel() {
    let d = dir("turns");
    let mut h = pi_rpc::start(&spec(&d)).await.expect("start");
    assert_eq!(h.version.as_deref(), Some("0.0.0-fake"));
    let (sid, file) = h.session.clone().expect("pi's session");
    assert!(sid.starts_with("fake-"), "{sid}");
    assert!(file.is_some());
    assert!(matches!(h.updates.recv().await, Some(Update::HarnessSession { id, .. }) if id == sid));

    h.send(Input::Prompt("hello".into())).expect("send");
    let u = turn(&mut h).await;
    assert!(u.contains(&Update::Thinking("Thinking.".into())));
    assert!(u.contains(&Update::Text("Hello from fake pi.".into())));
    assert!(
        u.iter()
            .any(|u| matches!(u, Update::Usage(x) if x.output == 5))
    );
    assert_eq!(u.last(), Some(&Update::TurnEnd(StopReason::EndTurn)));

    // A tool call that waits for an approval, allowed.
    h.send(Input::Prompt("use a tool".into())).expect("send");
    let (before, approval) = until_approval(&mut h).await;
    assert!(before.iter().any(|u| matches!(u,
        Update::Tool(t) if t.status == ToolStatus::Started && t.name == "bash"
            && t.input == Some(serde_json::json!({ "command": "ls" })))));
    h.send(Input::Answer {
        id: approval,
        allow: true,
        option: None,
    })
    .expect("answer");
    let u = turn(&mut h).await;
    assert!(u.iter().any(|u| matches!(u,
        Update::Tool(t) if t.status == ToolStatus::Finished && t.output.as_deref() == Some("file-a"))));
    assert_eq!(u.last(), Some(&Update::TurnEnd(StopReason::EndTurn)));

    // Denied, the tool call fails.
    h.send(Input::Prompt("use a tool".into())).expect("send");
    let (_, approval) = until_approval(&mut h).await;
    h.send(Input::Answer {
        id: approval,
        allow: false,
        option: None,
    })
    .expect("answer");
    let u = turn(&mut h).await;
    assert!(
        u.iter()
            .any(|u| matches!(u, Update::Tool(t) if t.status == ToolStatus::Failed))
    );

    // Cancelled.
    h.send(Input::Prompt("work slowly".into())).expect("send");
    let first = h.updates.recv().await.expect("text");
    assert_eq!(first, Update::Text("Working".into()));
    h.send(Input::Cancel).expect("cancel");
    let u = turn(&mut h).await;
    assert_eq!(u.last(), Some(&Update::TurnEnd(StopReason::Cancelled)));

    // Closing ends the process.
    let pid = h.pid;
    h.close();
    let u = turn(&mut h).await;
    assert!(matches!(u.last(), Some(Update::Exited(_))), "{u:?}");
    assert!(!platform::process_alive(pid), "the harness process ended");

    let log = std::fs::read_to_string(d.join("fake-pi.log")).expect("log");
    assert!(log.contains("args:--mode rpc"), "{log}");
    assert!(
        log.contains("key:none") && !log.contains("key:seen"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_resumed_session_continues_pis_own_session() {
    let d = dir("resume");
    let mut s = spec(&d);
    s.resume = Some(d.join("sessions/fake-42.jsonl").display().to_string());
    s.model = Some("openai/gpt-5.5".into());
    let mut h = pi_rpc::start(&s).await.expect("start");
    assert_eq!(h.session.as_ref().map(|s| s.0.as_str()), Some("fake-42"));
    h.close();
    let log = std::fs::read_to_string(d.join("fake-pi.log")).expect("log");
    assert!(log.contains("--model openai/gpt-5.5 --session"), "{log}");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_pi_without_the_rpc_surface_is_refused_clearly() {
    let d = dir("old");
    let mut s = spec(&d);
    s.env.push(("FAKE_PI_OLD".into(), "1".into()));
    let e = pi_rpc::start(&s).await.expect_err("refused");
    assert!(
        e.contains("pi 0.0.0-fake") && e.contains("`get_state`") && e.contains("Unknown command"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// The RPC surface the adapter relies on, against a real pi: the start
/// check (`get_state`, `abort`, `pi --version`), resuming pi's own session
/// with `--session`, and closing. No prompt is sent, so no model is called
/// and no login is needed. Skipped unless `DREAMER_PI_CONTRACT` names pi's
/// executable.
#[tokio::test]
async fn pi_contract() {
    let Some(pi) = std::env::var_os("DREAMER_PI_CONTRACT").filter(|p| !p.is_empty()) else {
        eprintln!("skipped: set DREAMER_PI_CONTRACT to a pi executable");
        return;
    };
    let d = dir("contract");
    let mut s = Spec {
        program: PathBuf::from(pi),
        args: Vec::new(),
        cwd: d.clone(),
        // No network use of its own (update checks, package installs).
        env: vec![("PI_OFFLINE".into(), "1".into())],
        agent_dir: Some(d.join("agent")),
        ..Spec::default()
    };
    let mut h = pi_rpc::start(&s).await.expect("pi passes the start check");
    let version = h.version.clone().expect("pi --version");
    let (id, file) = h.session.clone().expect("get_state gives pi's session id");
    eprintln!("pi {version}: session {id} file {file:?}");
    let pid = h.pid;
    h.close();
    while let Some(u) = h.updates.recv().await {
        if let Update::Exited(why) = u {
            eprintln!("closed: {why}");
            break;
        }
    }
    assert!(!platform::process_alive(pid), "closing ends pi");

    // Resumed from a session file: pi keeps a session on disk once it has
    // a message, so one is written here in pi's session format.
    let resumed = "0198c0de-0000-7000-8000-00000000d7ea";
    let file = d.join("resumed.jsonl");
    let cwd = d.display().to_string();
    let lines = [
        serde_json::json!({"type": "session", "version": 3, "id": resumed,
            "timestamp": "2026-11-16T10:00:00.000Z", "cwd": cwd}),
        serde_json::json!({"type": "message", "id": "a1b2c3d4", "parentId": null,
            "timestamp": "2026-11-16T10:00:01.000Z",
            "message": {"role": "user", "content": "hello", "timestamp": 1_794_823_201_000_u64}}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&file, text).expect("session file");
    s.resume = Some(file.display().to_string());
    let mut h = pi_rpc::start(&s).await.expect("pi resumes a kept session");
    assert_eq!(
        h.session.as_ref().map(|s| s.0.as_str()),
        Some(resumed),
        "the same session"
    );
    h.close();
    let _ = std::fs::remove_dir_all(&d);
}
