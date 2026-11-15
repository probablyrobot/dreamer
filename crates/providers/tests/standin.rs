//! The provider contract against stand-in CLIs (shell scripts that write
//! what the real CLIs write): streaming to an answer, no API key in the
//! child's environment, cancellation that ends the whole process group,
//! a timeout, and probes that report a logged-out or incompatible CLI.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use providers::{Event, Failure, Kind, Request, State};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("providers-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    d
}

fn script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("cli");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

async fn all(run: &mut providers::Run) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while let Ok(Some(e)) = tokio::time::timeout_at(deadline, run.events.recv()).await {
        out.push(e);
    }
    out
}

/// Whether `pid` still runs, within a moment for a signal to land. An
/// orphan that exited but was not reaped (a container's first process may
/// not reap) is a zombie, which has ended.
async fn running(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit(')')
                    .next()
                    .map(|r| r.trim_start().starts_with('Z'))
            })
            .unwrap_or(false);
        if !platform::process_alive(pid) || zombie {
            return false;
        }
        if Instant::now() > deadline {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const INIT: &str =
    r#"{"type":"system","subtype":"init","session_id":"s","model":"m","apiKeySource":"none"}"#;

#[tokio::test]
async fn a_run_streams_to_its_answer_without_an_api_key_in_reach() {
    let d = dir("stream");
    // The answer says whether the key reached the CLI.
    let cli = script(
        &d,
        &format!(
            r#"echo '{INIT}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"hi"}}}}}}'
if [ -n "$ANTHROPIC_API_KEY" ]; then k=key-seen; else k=no-key; fi
echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$k $EXTRA\",\"usage\":{{}}}}""#
        ),
    );
    let req = Request {
        prompt: "hello".into(),
        env: vec![
            ("ANTHROPIC_API_KEY".into(), "sk-should-never-arrive".into()),
            ("EXTRA".into(), "extra-arrived".into()),
        ],
        ..Request::default()
    };
    let mut run = providers::start_program(Kind::ClaudeCli, &cli, &req).expect("start");
    let e = all(&mut run).await;
    assert!(matches!(e[0], Event::Started { .. }), "{e:?}");
    assert_eq!(e[1], Event::Text("hi".into()));
    assert_eq!(
        e.last(),
        Some(&Event::Done("no-key extra-arrived".into())),
        "{e:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn cancelling_ends_the_cli_and_everything_it_started() {
    let d = dir("cancel");
    let pids = d.join("pids");
    let cli = script(
        &d,
        &format!(
            r#"echo '{INIT}'
sleep 60 & echo $! > '{}'
wait"#,
            pids.display()
        ),
    );
    let mut run =
        providers::start_program(Kind::ClaudeCli, &cli, &Request::default()).expect("start");
    assert!(matches!(
        run.events.recv().await,
        Some(Event::Started { .. })
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    let sleeper = loop {
        if let Some(p) = std::fs::read_to_string(&pids)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            break p;
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in never started its child"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    run.cancel();
    let e = all(&mut run).await;
    assert_eq!(e, vec![Event::Failed(Failure::Cancelled)]);
    assert!(!running(run.pgid).await, "the CLI is still running");
    assert!(!running(sleeper).await, "its child is still running");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_run_past_its_time_is_stopped() {
    let d = dir("timeout");
    let cli = script(&d, &format!("echo '{INIT}'\nexec sleep 60"));
    let req = Request {
        timeout: Some(Duration::from_millis(300)),
        ..Request::default()
    };
    let mut run = providers::start_program(Kind::ClaudeCli, &cli, &req).expect("start");
    let e = all(&mut run).await;
    assert_eq!(e.last(), Some(&Event::Failed(Failure::TimedOut)));
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_cli_that_dies_says_why() {
    let d = dir("dies");
    let cli = script(&d, "echo 'Not logged in. Please run /login' >&2\nexit 1");
    let mut run =
        providers::start_program(Kind::ClaudeCli, &cli, &Request::default()).expect("start");
    let e = all(&mut run).await;
    assert!(
        matches!(e.as_slice(), [Event::Failed(Failure::LoggedOut(why))] if why.contains("/login")),
        "{e:?}"
    );
    let missing =
        providers::start_program(Kind::ClaudeCli, &d.join("missing"), &Request::default());
    assert_eq!(missing.map(|_| ()).err(), Some(Failure::NotInstalled));
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn probes_report_what_the_cli_says_about_itself() {
    let d = dir("probe");
    let cli = |version: &str, logged_in: &str| {
        script(
            &d,
            &format!(
                r#"case "$1" in
--version) echo '{version}' ;;
auth) echo '{{"loggedIn": {logged_in}, "authMethod": "claude.ai", "email": "never-kept@example.com"}}' ;;
esac"#
            ),
        )
    };
    let p =
        providers::probe_program(Kind::ClaudeCli, Some(cli("2.1.325 (Claude Code)", "true"))).await;
    assert_eq!(p.state, State::Ready);
    assert_eq!(p.login.as_deref(), Some("claude.ai"));
    assert!(!format!("{p:?}").contains("never-kept"), "{p:?}");
    let p = providers::probe_program(Kind::ClaudeCli, Some(cli("2.1.325 (Claude Code)", "false")))
        .await;
    assert_eq!(p.state, State::LoggedOut);
    let p =
        providers::probe_program(Kind::ClaudeCli, Some(cli("3.0.0 (Claude Code)", "true"))).await;
    assert!(matches!(p.state, State::Incompatible(_)), "{p:?}");
    let p = providers::probe_program(Kind::CodexCli, None).await;
    assert_eq!(p.state, State::NotInstalled);

    let codex = script(
        &d,
        r#"case "$1" in
--version) echo 'codex-cli 0.46.0' ;;
login) echo 'Logged in using an API key - sk-…' ;;
esac"#,
    );
    let p = providers::probe_program(Kind::CodexCli, Some(codex)).await;
    assert!(matches!(p.state, State::MeteredLogin(_)), "{p:?}");
    let _ = std::fs::remove_dir_all(&d);
}
