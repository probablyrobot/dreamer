//! Agentic panes in a real tmux pane: an inspector runs a
//! prompt on a provider (a stand-in Claude Code here) with Dreamer's tools
//! narrowed to its declared read-only grant, and shows its scope, grant,
//! provider, activity, and every tool call; with no provider ready, the
//! inspector answers by a scripted search instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

fn target_dir() -> PathBuf {
    PathBuf::from(APP)
        .parent()
        .expect("target dir")
        .to_path_buf()
}

const INSPECTOR: &str = r#"
schema_version = 1
id = "test.inspector"
version = "0.1.0"
title = "Inspector"
capabilities = ["terminal.query", "tmux.topology"]

[view]
type = "agent"

[agent]
kind = "inspector"
provider = "claude"
instructions = "Answer in one line."
tools = { methods = ["terminal.query", "tmux.topology"], all_panes = true }
"#;

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Option<std::process::Child>,
}

impl Setup {
    fn new(name: &str) -> Setup {
        let root = PathBuf::from(format!("/tmp/drm-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(120, 30, "/bin/sh").expect("tmux");
        let daemon = target_dir().join("dreamerd");
        assert!(
            daemon.exists(),
            "{} is missing; build dreamer-server",
            daemon.display()
        );
        let child = Command::new(daemon)
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("rt/dreamerd.sock").exists() {
            assert!(Instant::now() < deadline, "daemon never started");
            std::thread::sleep(Duration::from_millis(50));
        }
        Setup {
            root,
            tmux,
            daemon: Some(child),
        }
    }

    /// Run the app in a new window with `claude` standing in, if given.
    fn app(&self, manifest: &Path, claude: &Path) -> String {
        let env = [
            format!("DREAMER_RUNTIME_DIR={}", self.root.join("rt").display()),
            format!("DREAMER_STATE_DIR={}", self.root.join("st").display()),
            "DREAMER_FORBID_DEFAULT_TMUX=1".to_owned(),
            format!("DREAMER_CLAUDE={}", claude.display()),
            // Codex stays out of the way even where it is installed.
            "DREAMER_CODEX=/nonexistent".to_owned(),
        ];
        let m = manifest.display().to_string();
        let mut args = vec!["new-window", "-d", "-P", "-F", "#{pane_id}", "env"];
        args.extend(env.iter().map(String::as_str));
        args.extend([APP, m.as_str()]);
        self.tmux
            .run_checked(&args)
            .expect("new-window")
            .trim()
            .to_owned()
    }

    fn screen(&self, pane: &str) -> String {
        self.tmux
            .run_checked(&["capture-pane", "-p", "-t", pane])
            .unwrap_or_default()
    }

    fn wait(&self, pane: &str, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = self.screen(pane);
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "never {what}:\n{s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn type_line(&self, pane: &str, text: &str) {
        self.tmux
            .run_checked(&["send-keys", "-t", pane, "-l", text])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", pane, "Enter"])
            .expect("enter");
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(mut c) = self.daemon.take() {
            let _ = Command::new("kill").arg(c.id().to_string()).status();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A stand-in `claude` that reports itself ready, records its arguments,
/// and answers a run with one tool call and an answer.
fn standin(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("claude");
    let args = dir.join("args");
    std::fs::write(
        &p,
        format!(
            r#"#!/bin/sh
case "$1" in
--version) echo '2.1.325 (Claude Code)'; exit 0 ;;
auth) echo '{{"loggedIn": true, "authMethod": "claude.ai"}}'; exit 0 ;;
esac
for a in "$@"; do printf '%s\n' "$a"; done > '{args}'
echo '{{"type":"system","subtype":"init","session_id":"s","model":"standin-model","apiKeySource":"none"}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"mcp__dreamer__terminal_query","input":{{"pattern":"x"}}}}]}}}}'
echo '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"2 matches"}}]}}}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"STANDIN-ANSWER two panes"}}}}}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"STANDIN-ANSWER two panes","usage":{{"input_tokens":7,"output_tokens":3}}}}'
"#,
            args = args.display()
        ),
    )
    .expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

#[test]
fn an_inspector_runs_a_prompt_with_its_read_only_tools_and_shows_what_it_did() {
    let s = Setup::new("insp");
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, INSPECTOR).expect("manifest");
    let claude = standin(&s.root);
    let pane = s.app(&manifest, &claude);
    let head = s.wait(&pane, "showed its provider and grant", |t| {
        t.contains("claude ready") && t.contains("[live]")
    });
    assert!(head.contains("inspector"), "{head}");
    assert!(
        head.contains("grant: read-only; terminal.query, tmux.topology; all panes"),
        "{head}"
    );
    // `q` is part of the prompt, not quit.
    s.type_line(&pane, "which panes quit?");
    // The usage line comes when the turn ends, a moment after the answer.
    let done = s.wait(&pane, "answered", |t| {
        t.contains("STANDIN-ANSWER two panes") && t.contains("tokens in")
    });
    assert!(done.contains("> which panes quit?"), "{done}");
    assert!(done.contains("→ terminal_query"), "{done}");
    assert!(done.contains("← 2 matches"), "{done}");
    assert!(done.contains("7 tokens in, 3 out"), "{done}");

    // What the provider was given: only Dreamer's tools, narrowed.
    let recorded = std::fs::read_to_string(s.root.join("args")).expect("args");
    let args: Vec<&str> = recorded.lines().collect();
    let after = |flag: &str| args.iter().position(|a| *a == flag).map(|i| args[i + 1]);
    assert_eq!(after("--tools"), Some(""), "{args:?}");
    let mcp = after("--mcp-config").expect("mcp config");
    assert!(mcp.contains("\"--method\",\"terminal.query\""), "{mcp}");
    assert!(mcp.contains("--all-panes"), "{mcp}");
    assert!(!mcp.contains("actions.submit"), "{mcp}");
    // The instructions (several lines) follow the kind's own.
    let system = after("--append-system-prompt").expect("system prompt");
    assert!(system.contains("an inspector pane"), "{system}");
    assert!(recorded.contains("Answer in one line."), "{recorded}");
}

#[test]
fn without_a_provider_an_inspector_searches_instead() {
    let s = Setup::new("nomodel");
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, INSPECTOR).expect("manifest");
    // A pane with something to find.
    let other = s
        .tmux
        .run_checked(&["new-window", "-d", "-P", "-F", "#{pane_id}", "/bin/sh"])
        .expect("window")
        .trim()
        .to_owned();
    s.tmux
        .run_checked(&["send-keys", "-t", &other, "echo NEEDLE''-41", "Enter"])
        .expect("echo");
    let pane = s.app(&manifest, Path::new("/nonexistent/claude"));
    let head = s.wait(&pane, "said it has no model", |t| {
        t.contains("no model") && t.contains("[live]")
    });
    assert!(head.contains("claude: not installed"), "{head}");
    // The new pane's mirror may still be starting: search until it is in.
    // A result line names the pane it came from; the line itself may carry
    // the shell's prompt (`$ NEEDLE-41`), depending on the shell.
    let from_other = |t: &str| {
        t.lines()
            .any(|l| l.contains(&format!("{other} ")) && l.contains("NEEDLE-41"))
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let found = loop {
        s.type_line(&pane, "NEEDLE-41");
        let tried = Instant::now();
        let screen = loop {
            let t = s.screen(&pane);
            if from_other(&t) || tried.elapsed() > Duration::from_secs(2) {
                break t;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if from_other(&screen) {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "never found NEEDLE-41 in {other}:\n{screen}"
        );
    };
    // Never its own pane: earlier results there would match again.
    assert!(
        !found.contains(&format!("{pane} ")),
        "searched itself:\n{found}"
    );
}

const OPERATOR: &str = r#"
schema_version = 1
id = "test.operator"
version = "0.1.0"
title = "Operator"
capabilities = ["actions.submit", "jobs.start", "hosts.call", "tmux.topology"]

[view]
type = "agent"

[agent]
kind = "operator"
provider = "claude"
tools = { methods = ["actions.submit", "jobs.start", "hosts.call", "tmux.topology"], all_panes = true }
max_turns = 6
timeout_s = 120
"#;

/// A stand-in `claude` that plays `events` (stream-json lines) for a run.
fn scripted_claude(dir: &Path, events: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("claude");
    let play = dir.join("events.jsonl");
    std::fs::write(&play, events).expect("events");
    std::fs::write(
        &p,
        format!(
            r#"#!/bin/sh
case "$1" in
--version) echo '2.1.325 (Claude Code)'; exit 0 ;;
auth) echo '{{"loggedIn": true, "authMethod": "claude.ai"}}'; exit 0 ;;
esac
cat '{play}'
"#,
            play = play.display()
        ),
    )
    .expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

/// An operator's intent, actions, and outcomes are inspectable.
/// Its plan (what it said before acting) and every step it took, with the
/// step's target and state, stay above the transcript; a failed step names
/// the tool and its actual target.
#[test]
fn an_operator_shows_its_plan_and_every_step_with_its_target_and_state() {
    let s = Setup::new("oper");
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, OPERATOR).expect("manifest");
    let events = [
        r#"{"type":"system","subtype":"init","session_id":"s","model":"standin-model","apiKeySource":"none"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"I will split %0 to the right, then run the check on devbox."}}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__dreamer__create_pane","input":{"pane":0,"direction":"horizontal"}}]}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"{\"id\":\"act-1\",\"action\":{},\"state\":\"completed\",\"observed\":\"created pane %9\"}"}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"mcp__dreamer__start_job","input":{"argv":["make","check"],"host":"devbox"}}]}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":"devbox: jobs.start: host devbox is not connected"}]}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Split done; devbox is not connected."}}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Split done; devbox is not connected.","usage":{"input_tokens":9,"output_tokens":5}}"#,
    ]
    .join("\n");
    let claude = scripted_claude(&s.root, &events);
    let pane = s.app(&manifest, &claude);
    let head = s.wait(&pane, "showed its limits", |t| {
        t.contains("claude ready") && t.contains("[live]")
    });
    assert!(
        head.contains("at most 6 turns; stops after 120 s"),
        "{head}"
    );
    s.type_line(&pane, "split and check");
    let done = s.wait(&pane, "showed its steps", |t| {
        t.contains("Split done") && t.contains("✗ start_job")
    });
    assert!(
        done.contains("plan: I will split %0 to the right, then run the check on devbox."),
        "{done}"
    );
    assert!(
        done.contains("✓ create_pane %0 → completed: created pane %9"),
        "{done}"
    );
    assert!(
        done.contains("✗ start_job make check @devbox → failed: devbox: jobs.start: host devbox is not connected"),
        "{done}"
    );
}

const FILTER: &str = r#"
schema_version = 1
id = "test.filter"
version = "0.1.0"
title = "Filter"
capabilities = ["terminal.query", "tmux.topology", "filters.manage"]

[view]
type = "agent"

[agent]
kind = "filter"
provider = "claude"
tools = { methods = ["terminal.query", "tmux.topology", "filters.manage"], all_panes = true }
"#;

/// A filter pane with no model watches what is typed as plain
/// text, shows each line its filter matches in another pane as it happens
/// (the derived stream), and removes the filter on `stop NAME`.
#[test]
fn without_a_provider_a_filter_pane_watches_what_is_typed() {
    let s = Setup::new("filter");
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, FILTER).expect("manifest");
    let other = s
        .tmux
        .run_checked(&["new-window", "-d", "-P", "-F", "#{pane_id}", "/bin/sh"])
        .expect("window")
        .trim()
        .to_owned();
    let pane = s.app(&manifest, Path::new("/nonexistent/claude"));
    let head = s.wait(&pane, "said it has no model", |t| {
        t.contains("no model") && t.contains("[live]") && t.contains("no filters yet")
    });
    assert!(head.contains("watched as plain text"), "{head}");
    s.type_line(&pane, "ALARM");
    s.wait(&pane, "installed the filter", |t| {
        t.contains("filters: watch-alarm")
    });
    // The other pane's mirror may still be starting: say it until seen.
    let seen = |t: &str| {
        t.lines()
            .any(|l| l.contains(&format!("{other} watch-alarm:")) && l.contains("ALARM-7"))
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let shown = loop {
        s.tmux
            .run_checked(&["send-keys", "-t", &other, "echo ALARM''-7", "Enter"])
            .expect("echo");
        std::thread::sleep(Duration::from_millis(700));
        let t = s.screen(&pane);
        if seen(&t) {
            break t;
        }
        assert!(Instant::now() < deadline, "never showed the match:\n{t}");
    };
    assert!(
        !shown.contains(&format!("{pane} watch-alarm:")),
        "it showed its own lines:\n{shown}"
    );
    s.type_line(&pane, "stop watch-alarm");
    s.wait(&pane, "removed the filter", |t| {
        t.contains("no filters yet")
    });
}
