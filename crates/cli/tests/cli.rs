//! An isolated tmux session discovered and controlled entirely through the
//! `dream` CLI, with `dreamerd` started on demand. Nothing here uses a GUI.
//!
//! Needs `dreamerd` built next to `dream` (a workspace build or test run
//! does that).

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const DREAM: &str = env!("CARGO_BIN_EXE_dream");

struct Cli {
    root: PathBuf,
    tmux: TmuxServer,
}

impl Cli {
    fn new() -> Cli {
        let root = PathBuf::from(format!("/tmp/drm-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(120, 40, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        Cli { root, tmux }
    }

    fn daemon() -> PathBuf {
        let p = PathBuf::from(DREAM).with_file_name("dreamerd");
        assert!(
            p.exists(),
            "{} is missing; build it with `cargo build -p dreamer-server`",
            p.display()
        );
        p
    }

    /// Run `dream` with private directories, the isolated tmux server, and
    /// the guard that keeps any daemon away from the user's own tmux.
    fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(DREAM);
        c.args(args)
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon());
        c.output().expect("dream")
    }

    /// `dream <args> -L <isolated> --tmux <baseline>`.
    fn on_tmux(&self, args: &[&str]) -> Output {
        let program = tmux_program().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        let at = all.iter().position(|a| *a == "--").unwrap_or(all.len());
        all.splice(
            at..at,
            ["-L", self.tmux.socket(), "--tmux", program.as_str()],
        );
        self.run(&all)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.on_tmux(args);
        assert!(
            out.status.success(),
            "dream {args:?} failed ({:?}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn topology(&self) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["ls", "--json"])).expect("json")
    }

    fn wait_for_text(&self, pane: &str, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let shown = self
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", pane])
                .unwrap_or_default();
            if shown.contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "pane {pane} never showed {text:?}:\n{shown}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = self.run(&["shutdown"]);
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn an_isolated_session_is_discovered_and_controlled_from_the_cli() {
    let cli = Cli::new();
    // Nothing runs yet; diagnostics say so without starting anything.
    let doctor = String::from_utf8_lossy(&cli.run(&["doctor"]).stdout).into_owned();
    assert!(doctor.contains("daemon   not running"), "{doctor}");
    assert!(!cli.run(&["status"]).status.success());

    // Discover: the first `ls` starts the daemon on the isolated server.
    let t = cli.topology();
    assert_eq!(t["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(t["sessions"][0]["name"], "main");
    let first = t["panes"][0]["id"].as_u64().expect("pane id");
    let window = t["windows"][0]["id"].as_u64().expect("window id");
    let text = cli.ok(&["ls"]);
    assert!(
        text.contains("$0 main") && text.contains(&format!("%{first}*")),
        "{text}"
    );

    // Control.
    let out = cli.ok(&[
        "act",
        "split",
        &format!("%{first}"),
        "-h",
        "--percent",
        "40",
        "--",
        "sh",
        "-c",
        "echo cli-split-ok; exec cat",
    ]);
    assert!(out.contains("completed: created pane %"), "{out}");
    let second: u64 = out
        .split("created pane %")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("new pane id");
    cli.wait_for_text(&format!("%{second}"), "cli-split-ok");

    cli.ok(&[
        "act",
        "send",
        &format!("%{second}"),
        "typed by dream",
        "--enter",
    ]);
    cli.wait_for_text(&format!("%{second}"), "typed by dream");
    cli.ok(&[
        "act",
        "keys",
        &format!("%{first}"),
        "e",
        "c",
        "h",
        "o",
        "Space",
        "k",
        "e",
        "y",
        "e",
        "d",
        "Enter",
    ]);
    cli.wait_for_text(&format!("%{first}"), "\nkeyed");
    // The daemon's own mirror of the pane shows the same output.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mirrored = cli.ok(&["read", &format!("%{first}"), "--history", "100"]);
        if mirrored.lines().any(|l| l == "keyed") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mirror never showed the output:\n{mirrored}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Mirrors follow tmux asynchronously; a new pane is attached shortly.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mirrors = cli.ok(&["mirrors"]);
        let live = |p: u64| mirrors.contains(&format!("%{p} gen 1 epoch 1 live"));
        if live(first) && live(second) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mirrors never converged:\n{mirrors}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = cli.ok(&["act", "rename", &format!("@{window}"), "from cli #1"]);
    assert!(out.contains("named \"from cli #1\""), "{out}");
    let out = cli.ok(&["act", "resize", &format!("%{second}"), "--width", "30"]);
    assert!(out.contains(&format!("pane %{second} is 30x")), "{out}");
    cli.ok(&["act", "select", &format!("%{first}")]);
    let out = cli.ok(&["act", "zoom", &format!("%{first}")]);
    assert!(out.contains("is zoomed"), "{out}");
    cli.ok(&["act", "zoom", &format!("%{first}")]);

    // An idempotent split runs once.
    let a = cli.ok(&["act", "split", &format!("%{first}"), "-v", "--key", "once"]);
    let b = cli.ok(&["act", "split", &format!("%{first}"), "-v", "--key", "once"]);
    assert_eq!(a.split_whitespace().next(), b.split_whitespace().next());
    let t = cli.topology();
    assert_eq!(t["panes"].as_array().map(Vec::len), Some(3));
    assert_eq!(t["windows"][0]["name"], "from cli #1");

    let out = cli.ok(&["act", "kill", &format!("%{second}")]);
    assert!(out.contains("is gone"), "{out}");

    // A refused action exits 1 and says why; nothing changed.
    let refused = cli.on_tmux(&["act", "select", "%999"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stdout).contains("failed: pane %999 does not exist"));
    assert_eq!(cli.topology()["panes"].as_array().map(Vec::len), Some(2));

    // The ledger, through the CLI.
    let list: Vec<serde_json::Value> = serde_json::from_str(&String::from_utf8_lossy(
        &cli.run(&["actions", "--json"]).stdout,
    ))
    .expect("json");
    assert_eq!(
        list.len(),
        11,
        "the repeated idempotent split adds no record: {list:#?}"
    );
    assert_eq!(list[0]["state"], "failed");
    let id = list[1]["id"].as_str().expect("id");
    let one = String::from_utf8_lossy(&cli.run(&["actions", id]).stdout).into_owned();
    assert!(one.starts_with(id) && one.contains("completed"), "{one}");

    let doctor = String::from_utf8_lossy(&cli.run(&["doctor"]).stdout).into_owned();
    assert!(doctor.contains("daemon   running"), "{doctor}");
    assert!(
        doctor.contains("11 actions (10 completed, 1 failed)"),
        "{doctor}"
    );
    assert!(
        doctor.contains(&format!("(-L {}) live", cli.tmux.socket())),
        "{doctor}"
    );

    // A different tmux server than the running daemon drives is refused.
    let other = cli.run(&["ls", "-L", "some-other-socket"]);
    assert!(!other.status.success());
    assert!(String::from_utf8_lossy(&other.stderr).contains("dream shutdown"));

    let out = String::from_utf8_lossy(&cli.run(&["shutdown"]).stdout).into_owned();
    assert!(out.contains("keep running"), "{out}");
    cli.tmux
        .run_checked(&["has-session", "-t", "main"])
        .expect("tmux keeps running");
}
