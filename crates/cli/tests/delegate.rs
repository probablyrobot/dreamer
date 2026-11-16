//! `dream agents delegate` against stand-in `claude` and `codex`
//! executables on `PATH` that write recorded stream-json and exec-json
//! output: no real CLI runs and no model is called. API key variables are
//! set in the test's environment; the stand-ins report whether any reached
//! them.
//!
//! Also runs the pi extension's delegation (its `daemon.ts`) under Node
//! against a stand-in daemon, when Node 23 or newer is installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DREAM: &str = env!("CARGO_BIN_EXE_dream");

/// Says whether any API key variable is set, as the stand-ins report it.
const KEYS: &str = r#"k=no-key; for v in "$ANTHROPIC_API_KEY" "$ANTHROPIC_AUTH_TOKEN" "$OPENAI_API_KEY" "$CODEX_API_KEY"; do [ -n "$v" ] && k=key-seen; done"#;

const CLAUDE: &str = r#"#!/bin/sh
here=$(dirname "$0")
case "$1" in
--version) echo "2.1.325 (Claude Code)"; exit 0 ;;
auth)
  if [ -f "$here/logged-out" ]; then echo '{"loggedIn":false}'; else echo '{"loggedIn":true,"authMethod":"claude.ai"}'; fi
  exit 0 ;;
esac
printf '%s\n' "$@" > "$here/claude-args"
KEYS
echo '{"type":"system","subtype":"init","session_id":"s1","model":"claude-standin","apiKeySource":"none"}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.txt"}}]}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"hello"}]}}'
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"claude answer $k $(pwd)\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}"
"#;

const CODEX: &str = r#"#!/bin/sh
here=$(dirname "$0")
case "$1" in
--version) echo "codex-cli 0.46.0"; exit 0 ;;
login)
  if [ -f "$here/logged-out" ]; then echo "Not logged in"; exit 1; fi
  echo "Logged in using ChatGPT"; exit 0 ;;
esac
printf '%s\n' "$@" > "$here/codex-args"
KEYS
echo '{"type":"thread.started","thread_id":"th1"}'
echo '{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"ls","status":"in_progress"}}'
echo '{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"a.txt","exit_code":0,"status":"completed"}}'
echo "{\"type\":\"item.completed\",\"item\":{\"id\":\"m1\",\"type\":\"agent_message\",\"text\":\"codex answer $k\"}}"
echo '{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":1}}'
"#;

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = PathBuf::from(format!("/tmp/drm-delegate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "bin", "work", "artifacts"] {
            std::fs::create_dir_all(root.join(d)).expect("dir");
        }
        for (name, script) in [("claude", CLAUDE), ("codex", CODEX)] {
            let exe = root.join("bin").join(name);
            std::fs::write(&exe, script.replace("KEYS", KEYS)).expect("write");
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        Env { root }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_path(args, &format!("{}:/usr/bin:/bin", self.bin().display()))
    }

    fn run_with_path(&self, args: &[&str], path: &str) -> Output {
        Command::new(DREAM)
            .args(args)
            .current_dir(&self.root)
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_JOB_ARTIFACTS", self.root.join("artifacts"))
            // Every key the providers remove: none may reach the CLI.
            .env("ANTHROPIC_API_KEY", "sk-ant-must-not-arrive")
            .env("ANTHROPIC_AUTH_TOKEN", "must-not-arrive")
            .env("OPENAI_API_KEY", "sk-must-not-arrive")
            .env("CODEX_API_KEY", "must-not-arrive")
            .env("PATH", path)
            .env_remove("DREAMER_CLAUDE")
            .env_remove("DREAMER_CODEX")
            .output()
            .expect("dream")
    }

    fn json(&self, args: &[&str]) -> (bool, serde_json::Value) {
        let out = self.run(args);
        let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "dream {args:?}: not JSON:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.success(), v)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn claude_code_answers_on_its_own_login_with_read_only_tools() {
    let env = Env::new("claude");
    let work = env.root.join("work");
    let work = work.to_str().expect("utf-8");
    let (ok, v) = env.json(&[
        "agents", "delegate", "--to", "claude", "--cwd", work, "--json", "--", "What", "is", "in",
        "a.txt?",
    ]);
    assert!(ok, "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["delegate"], "claude");
    // No API key reached the CLI, and it ran in the directory given.
    let answer = v["answer"].as_str().expect("answer");
    assert!(answer.starts_with("claude answer no-key "), "{answer}");
    assert!(answer.ends_with("/work"), "{answer}");
    assert_eq!(v["model"], "claude-standin");
    assert_eq!(v["tools"]["summary"], "Read");
    assert_eq!(v["usage"]["output_tokens"], 2);
    // The providers' flags, with only the read-only built-in tools.
    let args = std::fs::read_to_string(env.bin().join("claude-args")).expect("args");
    let args: Vec<&str> = args.lines().collect();
    let after = |flag: &str| args.iter().position(|a| *a == flag).map(|i| args[i + 1]);
    assert_eq!(after("-p"), Some("What is in a.txt?"));
    assert_eq!(after("--output-format"), Some("stream-json"));
    assert_eq!(after("--tools"), Some("Read,Grep,Glob"));
    assert_eq!(after("--allowedTools"), Some("Read,Grep,Glob"));
    // The result is also left for the job's reader.
    let file: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.root.join("artifacts/delegate.json")).expect("result file"),
    )
    .expect("json");
    assert_eq!(file["answer"], v["answer"]);
}

#[test]
fn codex_answers_and_its_commands_are_summarised() {
    let env = Env::new("codex");
    let out = env.run(&[
        "agents", "delegate", "--to", "codex", "--", "List", "the", "files",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert_eq!(stdout.trim(), "codex answer no-key");
    assert!(
        stderr.contains("billed to your own Codex login"),
        "{stderr}"
    );
    assert!(stderr.contains("[tools: command]"), "{stderr}");
    let args = std::fs::read_to_string(env.bin().join("codex-args")).expect("args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(&args[..2], ["exec", "--json"]);
    assert!(
        args.windows(2).any(|w| w == ["--sandbox", "read-only"]),
        "{args:?}"
    );
    assert_eq!(args.last(), Some(&"List the files"));
}

#[test]
fn a_logged_out_or_missing_cli_is_reported_and_never_run() {
    let env = Env::new("out");
    std::fs::write(env.bin().join("logged-out"), "").expect("mark");
    for (to, args_file) in [("claude", "claude-args"), ("codex", "codex-args")] {
        let (ok, v) = env.json(&["agents", "delegate", "--to", to, "--json", "--", "hi"]);
        assert!(!ok, "{v}");
        assert_eq!(v["ok"], false, "{v}");
        let error = v["error"].as_str().expect("error");
        assert!(error.contains("not logged in"), "{error}");
        assert!(
            v["state"]
                .as_str()
                .is_some_and(|s| s.starts_with("logged out")),
            "{v}"
        );
        assert!(
            !env.bin().join(args_file).exists(),
            "{to} was asked to run while logged out"
        );
    }
    let out = env.run_with_path(
        &["agents", "delegate", "--to", "claude", "--json", "--", "hi"],
        "/usr/bin:/bin",
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(!out.status.success());
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|e| e.contains("not installed")),
        "{v}"
    );
}

#[test]
fn a_delegate_turned_off_in_the_settings_is_refused() {
    let env = Env::new("off");
    std::fs::write(
        env.root.join("config/dreamer.yaml"),
        "agents:\n  pi:\n    delegates: [codex]\n",
    )
    .expect("settings");
    let out = env.run(&["agents", "delegate", "--to", "claude", "--", "hi"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("agents.pi.delegates"), "{stderr}");
    assert!(!env.bin().join("claude-args").exists());
    let out = env.run(&["agents", "delegate", "--to", "codex", "--", "hi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The extension's delegation request building and result handling, under
/// Node with its types stripped, against a stand-in daemon.
#[test]
fn the_extension_hands_tasks_to_dream_agents_delegate() {
    let node = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("node"))
            .find(|p| p.is_file())
    });
    let Some(node) = node else {
        eprintln!("node is not installed; skipped");
        return;
    };
    let v = Command::new(&node).arg("--version").output().expect("node");
    let major: u32 = String::from_utf8_lossy(&v.stdout)
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if major < 23 {
        eprintln!("node {major} cannot strip types; skipped");
        return;
    }
    let env = Env::new("ext");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(&node)
        .arg(manifest.join("tests/pi_extension_delegate.mjs"))
        .arg(manifest.join("../providers/pi-profile/extensions/dreamer/daemon.ts"))
        .arg(&env.root)
        .output()
        .expect("node");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for want in [
        "delegates-default",
        "delegates-some",
        "delegates-none",
        "tool-names",
        "argv",
        "empty-task",
        "run-request",
        "followed",
        "answer",
        "logged-out",
        "no-result",
        "cancel",
    ] {
        assert!(
            text.lines()
                .any(|l| l.contains(&format!("\"step\":\"{want}\",\"ok\":true"))),
            "no passing step {want}:\n{text}"
        );
    }
}
