//! Dreamer's pi profile through `dream agents profile` and `dream agents
//! pi`, with a stand-in pi, and the profile's extension client against a
//! real `dreamerd`. Nothing is downloaded, and every directory is private
//! to the test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const DREAM: &str = env!("CARGO_BIN_EXE_dream");

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = PathBuf::from(format!("/tmp/drm-profile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        Env { root }
    }

    fn profile(&self) -> PathBuf {
        self.root.join("data/pi-profile")
    }

    /// A stand-in pi as the active managed release. In RPC mode it fails as
    /// pi does when an extension does not load (here: one containing
    /// BROKEN), and otherwise answers the command list. Run any other way,
    /// it prints its arguments and the environment Dreamer gave it.
    fn install_pi(&self) {
        let dir = self.root.join("data/agents/pi/9.9.9");
        std::fs::create_dir_all(&dir).expect("pi dir");
        let script = r#"#!/bin/sh
case "$1" in
--version) echo 9.9.9; exit 0 ;;
auth)
  if [ -f "$PI_CODING_AGENT_DIR/.logged-in" ]; then echo '{"status":"ready"}'; exit 0; fi
  echo '{"status":"not_ready"}'; exit 1 ;;
--mode)
  if grep -rqs BROKEN "$PI_CODING_AGENT_DIR/extensions"; then
    echo "Error: Failed to load extension \"broken\": ParseError" >&2; exit 1
  fi
  read line
  echo '{"id":"dreamer-check","type":"response","command":"get_commands","success":true,"data":{"commands":[{"name":"dreamer"},{"name":"panes"},{"name":"collect"},{"name":"apps"},{"name":"skill:dreamer-apps"},{"name":"skill:dreamer-config"},{"name":"skill:dreamer-pi-profile"}]}}'
  exit 0 ;;
esac
echo "args=$*"
echo "dir=$PI_CODING_AGENT_DIR"
echo "socket=$DREAMER_RPC_SOCKET"
echo "dream=$DREAMER_DREAM_BIN"
echo "key=${OPENAI_API_KEY:+set}"
"#;
        let exe = dir.join("pi");
        std::fs::write(&exe, script).expect("pi");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::os::unix::fs::symlink("9.9.9", self.root.join("data/agents/pi/current"))
            .expect("current");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(DREAM)
            .args(args)
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("OPENAI_API_KEY", "must-not-reach-pi")
            .output()
            .expect("dream")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "dream {args:?} failed:\n{text}");
        text
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.profile().join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, text).expect("write");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git_available() -> bool {
    providers::pi_profile::git_available()
}

/// The agent adds a skill and an extension, each a commit naming its
/// session; reverting one removes it; a broken extension is rolled back
/// before pi next starts, and pi starts with the profile and Dreamer's
/// environment but no API key.
#[test]
fn the_profile_evolves_reverts_and_heals() {
    if !git_available() {
        eprintln!("git is not installed; skipped");
        return;
    }
    let env = Env::new("evolve");
    env.install_pi();
    let out = env.ok(&["agents", "profile", "init"]);
    assert!(out.contains("written: APPEND_SYSTEM.md"), "{out}");
    for f in [
        "extensions/dreamer/index.ts",
        "skills/dreamer-pi-profile/SKILL.md",
        "prompts/panes.md",
        "settings.json",
        ".gitignore",
        ".dreamer/base.json",
    ] {
        assert!(env.profile().join(f).is_file(), "{f}");
    }

    env.write(
        "skills/standup/SKILL.md",
        "---\nname: standup\ndescription: Summarize today.\n---\nSummarize.\n",
    );
    let out = env.ok(&[
        "agents",
        "profile",
        "commit",
        "--session",
        "s-42",
        "-m",
        "add standup skill",
    ]);
    assert!(out.contains("check: ok"), "{out}");
    env.write(
        "extensions/clock/index.ts",
        "export default function (pi) { pi.registerCommand('clock', {}); }\n",
    );
    let out = env.ok(&[
        "agents",
        "profile",
        "commit",
        "--session",
        "s-42",
        "-m",
        "add clock",
    ]);
    assert!(out.contains("check: ok"), "{out}");
    // Skills pi did not load are named.
    assert!(out.contains("not loaded by pi"), "{out}");
    assert!(out.contains("skills/standup"), "{out}");

    let log = env.ok(&["agents", "profile", "log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines[0].contains("add clock") && lines[0].ends_with("[loads]"),
        "{log}"
    );
    assert!(lines[1].contains("add standup skill"), "{log}");
    assert!(lines[2].contains("dreamer base"), "{log}");
    let show = env.ok(&[
        "agents",
        "profile",
        "diff",
        lines[1].split(' ').next().expect("rev"),
    ]);
    assert!(show.contains("Pi-Session: s-42"), "{show}");

    let skill = lines[1].split(' ').next().expect("rev");
    let out = env.ok(&["agents", "profile", "revert", skill]);
    assert!(out.contains("reverted"), "{out}");
    assert!(!env.profile().join("skills/standup").exists());
    assert!(env.profile().join("extensions/clock/index.ts").exists());

    // A scripted prompt while pi reports no login: pi is not started.
    let out = env.run(&["agents", "pi", "--print", "hello"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("auth login openai-codex"), "{err}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("args="),
        "pi was started"
    );
    std::fs::write(env.profile().join(".logged-in"), "").expect("login");

    // A broken extension, never committed: the next start restores the
    // last version pi loaded, keeping the history.
    env.write("extensions/clock/index.ts", "BROKEN {\n");
    let out = env.ok(&["agents", "pi", "--print", "hello"]);
    assert!(out.contains("restored the profile to"), "{out}");
    assert!(out.contains("check: ok"), "{out}");
    let clock =
        std::fs::read_to_string(env.profile().join("extensions/clock/index.ts")).expect("clock");
    assert!(clock.contains("registerCommand"), "{clock}");
    assert!(
        !env.profile().join("skills/standup").exists(),
        "the revert stays"
    );
    assert!(out.contains("args=--print hello"), "{out}");
    assert!(
        out.contains(&format!("dir={}", env.profile().display())),
        "{out}"
    );
    assert!(out.contains("dreamerd.rpc.sock"), "{out}");
    assert!(out.contains("dream=/"), "{out}");
    assert!(out.contains("key=\n"), "an API key reached pi:\n{out}");
    let log = env.ok(&["agents", "profile", "log"]);
    assert!(
        log.lines().next().is_some_and(|l| l.contains("restore")),
        "{log}"
    );
    assert!(log.contains("changes outside a pi session"), "{log}");

    let status: serde_json::Value =
        serde_json::from_str(&env.ok(&["agents", "profile", "status", "--json"])).expect("json");
    assert_eq!(status["versioned"], true);
    assert_eq!(status["uncommitted"].as_array().map(Vec::len), Some(0));
    assert!(status["last_good"].is_string());
}

/// A shipped file the agent edited survives a Dreamer update, and is
/// reported; an unedited one is replaced.
#[test]
fn an_update_keeps_the_agents_edit_of_a_shipped_file() {
    let env = Env::new("update");
    env.ok(&["agents", "profile", "init"]);
    env.write("prompts/panes.md", "my own panes prompt\n");
    // Pretend the profile was laid out by an older Dreamer whose shipped
    // files differed.
    let manifest = env.profile().join(".dreamer/base.json");
    let mut m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).expect("m")).expect("json");
    m["version"] = "0.0.1".into();
    m["files"]["prompts/panes.md"] = "0".repeat(64).into();
    m["files"]["prompts/apps.md"] = "0".repeat(64).into();
    std::fs::write(&manifest, m.to_string()).expect("write");
    std::fs::write(env.profile().join("prompts/apps.md"), "old apps prompt").expect("w");
    // The old apps prompt is what that Dreamer wrote: record its hash.
    m["files"]["prompts/apps.md"] = providers::pi_profile::sha256(b"old apps prompt").into();
    std::fs::write(&manifest, m.to_string()).expect("write");

    env.install_pi();
    let out = env.ok(&["agents", "profile", "check"]);
    assert!(
        out.contains("kept the agent's version of prompts/panes.md"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(env.profile().join("prompts/panes.md")).expect("p"),
        "my own panes prompt\n"
    );
    let apps = std::fs::read_to_string(env.profile().join("prompts/apps.md")).expect("a");
    assert!(apps.contains("dreamer-apps skill"), "{apps}");
}

/// Node 23 or newer on `PATH` (it strips the extension's types), else
/// `None`, with the reason printed.
fn node_for_types() -> Option<PathBuf> {
    let node = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("node"))
            .find(|p| p.is_file())
    });
    let Some(node) = node else {
        eprintln!("node is not installed; skipped");
        return None;
    };
    // Type stripping needs Node 22.6 or newer.
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
        return None;
    }
    Some(node)
}

/// A running `dreamerd` on an isolated tmux, with private directories.
struct Live {
    env: Env,
    tmux: tmux_backend::TmuxServer,
    daemon: PathBuf,
}

impl Live {
    fn start(name: &str, settings: Option<&str>) -> Live {
        let env = Env::new(name);
        for d in ["rt", "st"] {
            let p = env.root.join(d);
            std::fs::create_dir_all(&p).expect("dir");
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
        if let Some(s) = settings {
            std::fs::write(env.root.join("config/dreamer.yaml"), s).expect("settings");
        }
        let tmux = tmux_backend::TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let daemon = PathBuf::from(DREAM).with_file_name("dreamerd");
        assert!(
            daemon.exists(),
            "build dreamerd: cargo build -p dreamer-server"
        );
        let live = Live { env, tmux, daemon };
        let program = tmux_backend::server::tmux_program().display().to_string();
        let up = live.dream(&[
            "ensure-running",
            "-L",
            live.tmux.socket(),
            "--tmux",
            &program,
        ]);
        assert!(
            up.status.success(),
            "{}",
            String::from_utf8_lossy(&up.stderr)
        );
        live
    }

    fn dream(&self, args: &[&str]) -> Output {
        let root = &self.env.root;
        Command::new(DREAM)
            .args(args)
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_DATA_DIR", root.join("data"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", &self.daemon)
            .output()
            .expect("dream")
    }

    /// The first pane of the tmux server.
    fn first_pane(&self) -> String {
        let program = tmux_backend::server::tmux_program().display().to_string();
        let topo: serde_json::Value = serde_json::from_slice(
            &self
                .dream(&["ls", "--json", "-L", self.tmux.socket(), "--tmux", &program])
                .stdout,
        )
        .expect("ls");
        topo["panes"][0]["id"].as_u64().expect("pane").to_string()
    }

    /// `node tests/SCRIPT daemon.ts ARGS...`, reaching this daemon.
    fn node(&self, node: &Path, script: &str, args: &[&Path]) -> std::process::Child {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        Command::new(node)
            .arg(manifest.join("tests").join(script))
            .arg(manifest.join("../providers/pi-profile/extensions/dreamer/daemon.ts"))
            .args(args)
            .env("DREAMER_RUNTIME_DIR", self.env.root.join("rt"))
            // What it runs (`dream`) stays in the private directories too.
            .env("DREAMER_STATE_DIR", self.env.root.join("st"))
            .env("DREAMER_DATA_DIR", self.env.root.join("data"))
            .env("DREAMER_CONFIG_DIR", self.env.root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", &self.daemon)
            .env_remove("DREAMER_AGENT_LEVEL")
            .env_remove("DREAMER_APPS_ROOT")
            .env_remove("TMUX_PANE")
            .env_remove("DREAMER_RPC_SOCKET")
            .env_remove("DREAMER_SOCKET")
            .env_remove("DREAMER_DRAWER")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("node")
    }

    /// The script's JSON step lines: each of `want` is there and ok.
    fn steps(&self, child: std::process::Child, want: &[&str]) {
        let out = child.wait_with_output().expect("node");
        let text = String::from_utf8_lossy(&out.stdout);
        let _ = self.dream(&["shutdown"]);
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let steps: Vec<serde_json::Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        for want in want {
            let s = steps
                .iter()
                .find(|s| s["step"] == *want)
                .unwrap_or_else(|| panic!("no step {want}:\n{text}"));
            assert_eq!(s["ok"], true, "{want}:\n{text}");
        }
    }
}

/// The extension's daemon client (plain Node, with its types stripped)
/// against a real daemon: it lists and reads panes; its connection is an
/// agent's, so it cannot see or answer approvals; typing into the user's
/// pane shows an approval and is refused when the user denies it; in a
/// window it opened it acts without asking.
#[test]
fn the_extension_client_works_under_an_agents_grant() {
    let Some(node) = node_for_types() else {
        return;
    };
    let live = Live::start("ext", None);
    let pane = live.first_pane();
    let signal = live.env.root.join("deny");
    let child = live.node(
        &node,
        "pi_extension_client.mjs",
        &[Path::new(&pane), &signal],
    );
    // The user, through the owner's `dream approvals`, denies what the
    // agent asks.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !signal.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let id = std::fs::read_to_string(&signal).expect("an approval was shown");
    let denied = live.dream(&["approvals", "deny", id.trim()]);
    assert!(
        denied.status.success(),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    live.steps(
        child,
        &[
            "list",
            "read",
            "grant",
            "approvals.list",
            "approval",
            "send",
            "answered",
            "run",
            "own",
            "owned",
            "rename",
            "woken",
            "grouped",
        ],
    );
}

/// The extension's app tools against a real daemon and this `dream`: a new
/// package, a type error the check places, the fix, a text preview and the
/// test, an image preview refused with no desktop, the install, and the
/// app opened beside a pane the session created; at `observe`, installing
/// and launching are refused.
#[test]
fn the_extension_builds_installs_and_opens_an_app() {
    let Some(node) = node_for_types() else {
        return;
    };
    let live = Live::start("apps", None);
    let root = live.env.root.join("work");
    std::fs::create_dir_all(&root).expect("work");
    let child = live.node(&node, "pi_extension_apps.mjs", &[Path::new(DREAM), &root]);
    live.steps(
        child,
        &[
            "new",
            "type error",
            "fixed",
            "outside",
            "preview text",
            "test",
            "preview image",
            "install",
            "launch",
            "observe check",
            "observe install",
            "observe launch",
        ],
    );
}

/// Pane context and send_to_pane as pi in a harness drawer uses them: once
/// the drawer is shown, `@pane` attaches the pane it came from, `@%N` a
/// named pane, and `@errors` the errors collector's rows; a code block sent
/// to the user's pane is pasted there after approval, without Enter; and
/// the user typing in that pane ends the agent's lease.
#[test]
fn the_extension_attaches_pane_context_and_sends_to_a_pane() {
    let Some(node) = node_for_types() else {
        return;
    };
    let live = Live::start(
        "ctx",
        Some("agents:\n  default: pi\n  pi:\n    command: ['/bin/sh', '-c', 'exec sleep 600']\n"),
    );
    let pane = live.first_pane();
    let child = live.node(&node, "pi_extension_context.mjs", &[Path::new(&pane)]);
    live.steps(
        child,
        &[
            "drawer", "@pane", "@%N", "plain", "last", "@errors", "send", "revoked", "stopped",
            "not sent",
        ],
    );
}
