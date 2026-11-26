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
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/drm-cli-{}-{n}", std::process::id()));
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
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon())
            .env("DREAMER_SSH", self.root.join("ssh"))
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env("DREAMER_TEST_BACKOFF_MS", "100")
            .env("DREAMER_TEST_EXTENSION_HEARTBEAT_MS", "300")
            .env("DREAMER_TEST_EXTENSION_BACKOFF_MS", "100")
            .env("DREAMER_TEST_AGENT_STALE_MS", "4000");
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

/// The `dream mirrors` line for generation 1 of a pane (any epoch).
fn mirror_line(mirrors: &str, pane: u64) -> Option<&str> {
    mirrors
        .lines()
        .find(|l| l.starts_with(&format!("%{pane} gen 1 epoch ")))
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
        let live = |p: u64| mirror_line(&mirrors, p).is_some_and(|l| l.contains(" live warm "));
        if live(first) && live(second) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mirrors never converged:\n{mirrors}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = cli.ok(&["tier", &format!("%{first}"), "hot"]);
    assert_eq!(out.trim(), format!("%{first} is hot"));
    assert!(
        cli.ok(&["mirrors"])
            .lines()
            .any(|l| l.starts_with(&format!("%{first} gen 1 epoch ")) && l.contains(" live hot "))
    );

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

impl Cli {
    /// `dream bridge --stdio` for the isolated tmux server, with the same
    /// private environment as every other call here.
    fn bridge(&self, daemon: PathBuf) -> tokio::process::Child {
        let program = tmux_program().display().to_string();
        tokio::process::Command::new(DREAM)
            .args(["bridge", "--stdio", "-L", self.tmux.socket()])
            .args(["--tmux", program.as_str()])
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", daemon)
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("bridge")
    }

    fn daemon_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }
}

#[test]
fn the_stdio_bridge_carries_a_whole_session_and_leaves_the_daemon_running() {
    let cli = Cli::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        // The bridge starts the daemon; a client speaks to it through the
        // bridge's pipes exactly as it would through the socket.
        let mut bridge = cli.bridge(Cli::daemon());
        let stdout = bridge.stdout.take().expect("stdout");
        let stdin = bridge.stdin.take().expect("stdin");
        let client = client::Client::over(stdout, stdin, "bridge-test", 64)
            .await
            .expect("handshake through the bridge");
        let pid = cli.daemon_pid().expect("the bridge started a daemon");
        assert_eq!(client.welcome.pid, pid);
        let api::Response::DaemonStatus(s) = client
            .call(api::Request::DaemonStatus(api::Empty {}))
            .await
            .expect("status through the bridge")
        else {
            panic!("unexpected reply");
        };
        assert_eq!(
            s.endpoints.first().map(|e| e.socket.as_str()),
            Some(format!("-L {}", cli.tmux.socket()).as_str())
        );

        // Closing the client's side ends the bridge, not the daemon.
        drop(client);
        let status = tokio::time::timeout(Duration::from_secs(10), bridge.wait())
            .await
            .expect("the bridge ends when its input ends")
            .expect("wait");
        assert!(status.success(), "bridge exit {status:?}");
        assert!(platform::process_alive(pid), "the daemon keeps running");
    });
    let out = cli.run(&["status"]);
    assert!(out.status.success(), "dream status after the bridge");

    // With no daemon to reach, the bridge says why on standard error and
    // writes nothing at all where protocol bytes would go.
    let _ = cli.run(&["shutdown"]);
    // Wait until nothing answers (an unreaped daemon in a container with no
    // init still looks alive by pid, so ask the socket).
    let deadline = Instant::now() + Duration::from_secs(10);
    while cli.run(&["status"]).status.success() {
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = rt.block_on(async {
        let mut child = cli.bridge(cli.root.join("no-such-dreamerd"));
        drop(child.stdin.take());
        child.wait_with_output().await.expect("bridge output")
    });
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not start dreamerd"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Kills a daemon by its lock file when dropped, so a failed test leaves
/// nothing running.
struct DaemonGuard(PathBuf);

impl DaemonGuard {
    fn pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.0.join("dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid().filter(|p| platform::process_alive(*p)) {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[test]
fn the_local_daemon_reaches_a_remote_daemon_through_ssh_and_the_bridge() {
    let cli = Cli::new();
    // The "remote host": its own tmux server and private daemon
    // directories, reached only through a stand-in ssh that does what sshd
    // does with the command (hand it to a shell), in that host's
    // environment. No listener and no PTY anywhere.
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    far.run_checked(&["rename-session", "-t", "main", "faraway"])
        .expect("rename");
    let far_root = cli.root.join("far");
    let far_daemon = DaemonGuard(far_root.join("rt"));
    let ssh_argv = cli.root.join("ssh-argv");
    let ssh = cli.root.join("ssh");
    std::fs::write(
        &ssh,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > '{argv}'
[ "$1" = -T ] || exit 90
while [ "$1" != -- ]; do shift; done
echo "stand-in ssh: connected to $2" >&2
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
exec /bin/sh -c "$3"
"#,
            argv = ssh_argv.display(),
            rt = far_root.join("rt").display(),
            st = far_root.join("st").display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    let program = tmux_program().display().to_string();
    let connect = [
        "hosts",
        "connect",
        "devbox",
        "devbox",
        "--dream",
        DREAM,
        "--remote-socket",
        far.socket(),
        "--remote-tmux",
        program.as_str(),
    ];

    let out = cli.ok(&connect);
    assert!(out.contains("devbox (devbox) live: dreamerd pid"), "{out}");
    let argv = std::fs::read_to_string(&ssh_argv).expect("argv");
    assert!(
        argv.starts_with("-T\n-a\n-x\n-o\nBatchMode=yes\n--\ndevbox\n"),
        "{argv}"
    );
    let far_pid = far_daemon
        .pid()
        .expect("the bridge started the remote daemon");

    // A typed call travels to the remote daemon and back.
    let topo = cli.ok(&["hosts", "call", "devbox", "tmux.topology"]);
    assert!(topo.contains("faraway"), "{topo}");
    let list: serde_json::Value =
        serde_json::from_str(&cli.ok(&["hosts", "list", "--json"])).expect("json");
    let host = &list["hosts"][0];
    assert_eq!(host["state"], "live", "{list}");
    assert_eq!(host["remote_pid"], far_pid, "{list}");

    // Streams are refused, not half-forwarded.
    let out = cli.on_tmux(&["hosts", "call", "devbox", "tmux.watch"]);
    assert!(!out.status.success());

    // Disconnecting closes SSH; the remote daemon and tmux keep running.
    let out = cli.ok(&["hosts", "disconnect", "devbox"]);
    assert!(out.contains("closed"), "{out}");
    assert!(platform::process_alive(far_pid));
    far.run_checked(&["has-session", "-t", "faraway"])
        .expect("remote tmux keeps running");

    // Reconnect, then lose the remote daemon: the host is marked failed,
    // with SSH's own diagnostic kept apart from the protocol.
    let out = cli.ok(&connect);
    assert!(out.contains("live"), "{out}");
    let _ = Command::new("kill")
        .args(["-9", &far_pid.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    let host = loop {
        let list: serde_json::Value =
            serde_json::from_str(&cli.ok(&["hosts", "list", "--json"])).expect("json");
        if list["hosts"][0]["state"] == "failed" {
            break list["hosts"][0].clone();
        }
        assert!(Instant::now() < deadline, "never marked failed: {list}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let detail = host["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("ssh: stand-in ssh: connected to devbox"),
        "{detail}"
    );
    let out = cli.on_tmux(&["hosts", "call", "devbox", "tmux.topology"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not connected"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn lines_in(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn host_json(cli: &Cli) -> serde_json::Value {
    let list: serde_json::Value =
        serde_json::from_str(&cli.ok(&["hosts", "list", "--json"])).expect("json");
    list["hosts"][0].clone()
}

fn wait_until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_profile_reconnects_with_backoff_but_never_retries_a_refused_login() {
    let cli = Cli::new();
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    let far_root = cli.root.join("far");
    let far_daemon = DaemonGuard(far_root.join("rt"));
    let r = |name: &str| cli.root.join(name);
    // A stand-in ssh whose behaviour the test steers with marker files:
    // `deny` refuses the login, `down` makes the host unreachable. Every
    // attempt is counted, and the bridge's pid is recorded (the remote
    // command is exec'd, so it keeps this pid) to cut the link later.
    std::fs::write(
        r("ssh"),
        format!(
            r#"#!/bin/sh
echo attempt >> '{attempts}'
[ "$1" = -T ] || exit 90
while [ "$1" != -- ]; do shift; done
if [ -e '{deny}' ]; then echo "$2: Permission denied (publickey)." >&2; exit 255; fi
if [ -e '{down}' ]; then echo "ssh: connect to host $2 port 22: Connection refused" >&2; exit 255; fi
echo $$ > '{pid}'
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
exec /bin/sh -c "exec $3"
"#,
            attempts = r("attempts").display(),
            deny = r("deny").display(),
            down = r("down").display(),
            pid = r("bridge-pid").display(),
            rt = far_root.join("rt").display(),
            st = far_root.join("st").display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(
        r("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::create_dir_all(r("config")).expect("config dir");
    std::fs::write(
        r("config/hosts.yaml"),
        format!(
            "hosts:\n  - name: devbox\n    destination: devbox\n    dream: '{}'\n    \
             tmux: '{}'\n    tmux_socket: '{}'\n    tags: [lab]\n",
            DREAM,
            tmux_program().display(),
            far.socket()
        ),
    )
    .expect("hosts.yaml");
    let touch = |name: &str| std::fs::write(r(name), "").expect("marker");
    let kill_bridge = || {
        let pid = std::fs::read_to_string(r("bridge-pid")).expect("bridge pid");
        let _ = Command::new("kill").args(["-9", pid.trim()]).status();
    };

    cli.ok(&["ensure-running"]);
    let host = host_json(&cli);
    assert_eq!(host["name"], "devbox");
    assert_eq!(host["profile"], true);
    assert_eq!(host["auto_reconnect"], true);

    // A refused login is tried once and then left alone.
    touch("deny");
    let out = cli.on_tmux(&["hosts", "connect", "devbox"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Permission denied"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lines_in(&r("attempts")), 1, "a refused login was retried");
    let host = host_json(&cli);
    assert_eq!(host["failure"], "auth", "{host}");
    assert!(host["next_retry_unix_ms"].is_null(), "{host}");
    let text = cli.ok(&["hosts", "list"]);
    assert!(text.contains("not retried automatically"), "{text}");

    // Fixed: an explicit reconnect brings it up.
    std::fs::remove_file(r("deny")).expect("rm");
    let out = cli.ok(&["hosts", "reconnect", "devbox"]);
    assert!(out.contains("live"), "{out}");
    let far_pid = far_daemon.pid().expect("remote daemon");

    // The link drops while the host is unreachable: retries come with
    // growing delays (100, 200, 400 ms here, less up to a fifth of jitter).
    touch("down");
    let before = lines_in(&r("attempts"));
    let cut = Instant::now();
    kill_bridge();
    wait_until("three retries", 10, || {
        lines_in(&r("attempts")) >= before + 3
    });
    assert!(
        cut.elapsed() >= Duration::from_millis(560),
        "three retries within {:?}",
        cut.elapsed()
    );
    let host = host_json(&cli);
    assert_eq!(host["failure"], "transient", "{host}");
    assert!(host["attempts"].as_u64().unwrap_or(0) >= 3, "{host}");

    // Reachable again: the next retry reconnects to the same remote daemon,
    // which kept running.
    std::fs::remove_file(r("down")).expect("rm");
    wait_until("live again", 15, || host_json(&cli)["state"] == "live");
    assert_eq!(far_daemon.pid(), Some(far_pid));
    assert_eq!(host_json(&cli)["remote_pid"], far_pid);

    // An explicit disconnect during backoff stops the retries.
    touch("down");
    kill_bridge();
    wait_until("a retry is scheduled", 10, || {
        !host_json(&cli)["next_retry_unix_ms"].is_null()
    });
    let out = cli.ok(&["hosts", "disconnect", "devbox"]);
    assert!(out.contains("closed"), "{out}");
    // An attempt already under way when the disconnect came may still log
    // itself; count once that has settled.
    std::thread::sleep(Duration::from_millis(500));
    let count = lines_in(&r("attempts"));
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lines_in(&r("attempts")), count, "retried after disconnect");
    assert_eq!(host_json(&cli)["state"], "closed");
}

#[test]
fn a_service_definition_is_written_valid_and_removed_exactly() {
    let root = PathBuf::from(format!("/tmp/drm-service-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let dream = |args: &[&str]| {
        let out = Command::new(DREAM)
            .args(args)
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env("DREAMER_DAEMON", Cli::daemon())
            .output()
            .expect("dream");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "dream {args:?}: {text}");
        text
    };
    // Write only: no service manager is asked to load anything.
    let out = dream(&["service", "install", "--no-start", "-L", "work"]);
    let path = if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/dev.dreamer.dreamerd.plist")
    } else {
        home.join(".config/systemd/user/dreamerd.service")
    };
    assert!(out.contains(&path.display().to_string()), "{out}");
    let text = std::fs::read_to_string(&path).expect("definition");
    assert!(text.contains("--tmux-socket"), "{text}");
    assert!(text.contains("work"), "{text}");
    if cfg!(target_os = "macos") {
        let lint = Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .expect("plutil");
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
    } else if let Ok(v) = Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(&path)
        .output()
    {
        let said = String::from_utf8_lossy(&v.stderr);
        // Only problems with this unit count; the host's own units may warn.
        assert!(
            !said.lines().any(|l| l.contains("dreamerd.service")),
            "systemd-analyze: {said}"
        );
    }
    let before: Vec<PathBuf> = walk(&home);
    dream(&["service", "uninstall", "--no-stop"]);
    assert!(!path.exists());
    let after: Vec<PathBuf> = walk(&home);
    let gone: Vec<&PathBuf> = before.iter().filter(|p| !after.contains(p)).collect();
    assert_eq!(gone, [&path], "uninstall removed exactly the definition");
    let _ = std::fs::remove_dir_all(&root);
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

impl Cli {
    /// `dream <args>` with `input` on standard input.
    fn run_with_input(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write as _;
        let mut c = Command::new(DREAM);
        c.args(args)
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon())
            .env("DREAMER_SSH", self.root.join("ssh"))
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = c.spawn().expect("dream");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
        child.wait_with_output().expect("dream")
    }
}

#[test]
fn an_unknown_host_key_is_answered_by_the_user_in_a_visible_connection() {
    let cli = Cli::new();
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    let far_root = cli.root.join("far");
    let _far_daemon = DaemonGuard(far_root.join("rt"));
    let r = |name: &str| cli.root.join(name);
    // The coordinator's ssh (-T, BatchMode) refuses an unknown host key, as
    // OpenSSH does. The visible one asks, and remembers a "yes".
    std::fs::write(
        r("ssh"),
        format!(
            r#"#!/bin/sh
if [ "$1" = -T ]; then
  while [ "$1" != -- ]; do shift; done
  if [ ! -e '{known}' ]; then echo "Host key verification failed." >&2; exit 255; fi
  export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
  export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
  exec /bin/sh -c "exec $3"
fi
[ "$1" = -- ] && [ "$3" = true ] || exit 91
echo "The authenticity of host '$2' can't be established."
printf 'Are you sure you want to continue connecting (yes/no)? '
read answer
if [ "$answer" = yes ]; then touch '{known}'; exit 0; fi
echo "Host key verification failed." >&2
exit 255
"#,
            known = r("known_hosts").display(),
            rt = far_root.join("rt").display(),
            st = far_root.join("st").display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(
        r("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::create_dir_all(r("config")).expect("config dir");
    std::fs::write(
        r("config/hosts.yaml"),
        format!(
            "hosts:\n  - name: devbox\n    destination: devbox\n    dream: '{}'\n    \
             tmux: '{}'\n    tmux_socket: '{}'\n",
            DREAM,
            tmux_program().display(),
            far.socket()
        ),
    )
    .expect("hosts.yaml");
    cli.ok(&["ensure-running"]);

    // The coordinator never prompts: the unknown key is a host_key failure,
    // not retried.
    let out = cli.on_tmux(&["hosts", "connect", "devbox"]);
    assert!(!out.status.success());
    let host = host_json(&cli);
    assert_eq!(host["failure"], "host_key", "{host}");
    assert!(host["next_retry_unix_ms"].is_null(), "{host}");

    // Declined in the visible connection: nothing changes, and SSH's own
    // message is shown.
    let out = cli.run_with_input(&["hosts", "verify", "devbox"], "no\n");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("Are you sure you want to continue connecting"),
        "{text}"
    );
    assert!(text.contains("Host key verification failed."), "{text}");
    assert!(!r("known_hosts").exists());
    assert_eq!(host_json(&cli)["failure"], "host_key");

    // Accepted by the user: the coordinator connects.
    let out = cli.run_with_input(&["hosts", "verify", "devbox"], "yes\n");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("devbox (devbox) live"), "{text}");
    assert_eq!(host_json(&cli)["state"], "live");
}

#[test]
fn a_server_is_installed_on_each_host_for_its_architecture_and_kept_on_a_bad_transfer() {
    let cli = Cli::new();
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    let r = |name: &str| cli.root.join(name);
    let _guards: Vec<DaemonGuard> = ["x86box", "armbox"]
        .iter()
        .map(|d| DaemonGuard(r(&format!("{d}/rt"))))
        .collect();
    let version = env!("CARGO_PKG_VERSION");
    // Each "host": its own home, and a uname saying what it is.
    for (dest, arch) in [("x86box", "x86_64"), ("armbox", "aarch64")] {
        let bin = r(&format!("{dest}/fakebin"));
        std::fs::create_dir_all(&bin).expect("dirs");
        std::fs::create_dir_all(r(&format!("{dest}/home"))).expect("home");
        let uname = bin.join("uname");
        std::fs::write(
            &uname,
            format!("#!/bin/sh\ncase \"$1\" in -s) echo Linux;; -m) echo {arch};; esac\n"),
        )
        .expect("uname");
        std::fs::set_permissions(&uname, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
    }
    // ssh: the host's environment, then the command in a shell as sshd
    // does. With `corrupt`, the bytes sent are changed on the way.
    std::fs::write(
        r("ssh"),
        format!(
            r#"#!/bin/sh
while [ "$1" != -- ]; do shift; done
h='{root}'/"$2"
export HOME="$h/home" PATH="$h/fakebin:/usr/bin:/bin"
export DREAMER_RUNTIME_DIR="$h/rt" DREAMER_STATE_DIR="$h/st"
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
if [ -e '{root}/corrupt' ]; then {{ cat; echo junk; }} | /bin/sh -c "$3"; exit $?; fi
exec /bin/sh -c "$3"
"#,
            root = cli.root.display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(
        r("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    // Server archives whose dream runs the real one, each marked with its
    // target.
    let artifacts = r("artifacts");
    for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
        let name = format!("dreamer-server-{version}-{target}");
        let stage = artifacts.join("stage").join(&name);
        std::fs::create_dir_all(stage.join("bin")).expect("stage");
        std::fs::write(
            stage.join("bin/dream"),
            format!("#!/bin/sh\nexec '{DREAM}' \"$@\"\n"),
        )
        .expect("dream");
        std::fs::set_permissions(
            stage.join("bin/dream"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");
        std::fs::write(
            stage.join("VERSION"),
            format!("version {version} for {target}\n"),
        )
        .expect("VERSION");
        let tarball = artifacts.join(format!("{name}.tar.gz"));
        assert!(
            Command::new("tar")
                .arg("-C")
                .arg(artifacts.join("stage"))
                .arg("-czf")
                .arg(&tarball)
                .arg(&name)
                .status()
                .expect("tar")
                .success()
        );
        let sum = Command::new("shasum")
            .args(["-a", "256"])
            .arg(&tarball)
            .output()
            .or_else(|_| Command::new("sha256sum").arg(&tarball).output())
            .expect("sha256");
        let sum = String::from_utf8_lossy(&sum.stdout);
        let sum = sum.split_whitespace().next().expect("sum");
        std::fs::write(
            artifacts.join(format!("{name}.tar.gz.sha256")),
            format!("{sum}  {name}.tar.gz\n"),
        )
        .expect("sha file");
    }
    std::fs::create_dir_all(r("config")).expect("config dir");
    std::fs::write(
        r("config/hosts.yaml"),
        format!(
            "hosts:\n  - {{name: x86, destination: x86box, tmux: '{t}', tmux_socket: '{s}'}}\n  \
             - {{name: arm, destination: armbox, tmux: '{t}', tmux_socket: '{s}'}}\n",
            t = tmux_program().display(),
            s = far.socket()
        ),
    )
    .expect("hosts.yaml");
    cli.ok(&["ensure-running"]);
    let host = |name: &str| -> serde_json::Value {
        let list: serde_json::Value =
            serde_json::from_str(&cli.ok(&["hosts", "list", "--json"])).expect("json");
        list["hosts"]
            .as_array()
            .and_then(|a| a.iter().find(|h| h["name"] == name).cloned())
            .expect("host")
    };
    let artifacts_arg = artifacts.display().to_string();

    // Nothing installed and no dream on the host's PATH.
    assert!(!cli.on_tmux(&["hosts", "connect", "x86"]).status.success());
    assert_eq!(host("x86")["failure"], "not_installed", "{}", host("x86"));

    for (name, dest, target) in [
        ("x86", "x86box", "x86_64-unknown-linux-gnu"),
        ("arm", "armbox", "aarch64-unknown-linux-gnu"),
    ] {
        let out = cli.ok(&[
            "hosts",
            "install",
            name,
            "--artifacts",
            &artifacts_arg,
            "--yes",
        ]);
        assert!(out.contains("is Linux "), "{out}");
        assert!(out.contains(&format!("target {target}")), "{out}");
        assert!(out.contains("Check its SHA-256 on the host"), "{out}");
        assert!(out.contains(&format!("{name} ({dest}) live")), "{out}");
        let current = r(&format!("{dest}/home/.local/share/dreamer/current"));
        assert_eq!(
            std::fs::read_link(&current).expect("current is a symlink"),
            PathBuf::from(format!("versions/{version}"))
        );
        assert_eq!(
            std::fs::read_to_string(current.join("VERSION")).expect("VERSION"),
            format!("version {version} for {target}\n")
        );
        assert_eq!(host(name)["state"], "live");
    }

    // A transfer changed on the way is refused on the host; the installed
    // version stays.
    std::fs::write(r("corrupt"), "").expect("marker");
    let out = cli.on_tmux(&[
        "hosts",
        "install",
        "arm",
        "--artifacts",
        &artifacts_arg,
        "--yes",
    ]);
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(said.contains("checksum mismatch"), "{said}");
    let current = r("armbox/home/.local/share/dreamer/current");
    assert_eq!(
        std::fs::read_to_string(current.join("VERSION")).expect("kept"),
        format!("version {version} for aarch64-unknown-linux-gnu\n")
    );
    let leftovers: Vec<_> = std::fs::read_dir(r("armbox/home/.local/share/dreamer"))
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

/// The pane `dream app run` reported, and its launch id.
fn launched(out: &str) -> (String, String) {
    let pane = out
        .split_whitespace()
        .find(|w| w.starts_with('%'))
        .expect("pane")
        .to_owned();
    let id = out
        .split("(launch ")
        .nth(1)
        .and_then(|r| r.split(')').next())
        .expect("launch id")
        .to_owned();
    (pane, id)
}

impl Cli {
    fn pane_text(&self, pane: &str) -> String {
        self.tmux
            .run_checked(&["capture-pane", "-p", "-J", "-t", pane])
            .unwrap_or_default()
    }

    fn wait_pane(&self, pane: &str, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let shown = self.pane_text(pane);
            if shown.contains(text) {
                return shown;
            }
            assert!(
                Instant::now() < deadline,
                "pane {pane} never showed {text:?}:\n{shown}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn a_pane_app_registers_for_a_narrow_grant_and_its_ticket_never_leaks() {
    let cli = Cli::new();
    // A probe app allowed only to read: its own pane is its scope.
    let out = cli.ok(&[
        "app",
        "run",
        "probe",
        "--method",
        "terminal.read",
        "--",
        DREAM,
        "app",
        "probe",
        "--other",
        "%0",
    ]);
    let (pane, _) = launched(&out);
    let shown = cli.wait_pane(&pane, "probe: done");
    let line = |prefix: &str| {
        shown
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in:\n{shown}"))
            .to_owned()
    };
    assert!(
        line("probe: registered").contains(&format!("panes [{}]", &pane[1..])),
        "{shown}"
    );
    assert_eq!(line("probe: read own pane:"), "probe: read own pane: ok");
    assert!(
        line("probe: read other pane:").contains("pane %0 is outside"),
        "{shown}"
    );
    assert!(
        line("probe: shutdown:").contains("not in this connection's grant"),
        "{shown}"
    );
    assert!(
        line("probe: launch another app:").contains("only the user"),
        "{shown}"
    );
    assert!(
        line("probe: register again here:").contains("not in this connection's grant"),
        "{shown}"
    );
    assert!(
        line("probe: reuse ticket:").contains("already used"),
        "{shown}"
    );
    assert!(
        cli.run(&["status"]).status.success(),
        "the daemon is still running (the app could not stop it)"
    );

    // An expired ticket is refused.
    let out = cli.ok(&[
        "app",
        "run",
        "late",
        "--ttl-ms",
        "200",
        "--method",
        "terminal.read",
        "--",
        DREAM,
        "app",
        "probe",
        "--delay",
        "1500",
    ]);
    let (pane, _) = launched(&out);
    let shown = cli.wait_pane(&pane, "probe: register refused");
    assert!(shown.contains("expired"), "{shown}");

    // The right ticket from a process outside the pane is refused, and the
    // ticket appears nowhere it should not.
    let out = cli.ok(&[
        "app",
        "run",
        "idle",
        "--method",
        "terminal.read",
        "--",
        "sleep",
        "60",
    ]);
    let (pane, id) = launched(&out);
    let ticket_file = cli.root.join("rt/launch").join(&id);
    let ticket = std::fs::read_to_string(&ticket_file).expect("ticket file");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&ticket_file)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the ticket file is owner-only");
    }
    let ps = Command::new("ps")
        .args(["-axww", "-o", "command"])
        .output()
        .expect("ps");
    assert!(
        !String::from_utf8_lossy(&ps.stdout).contains(&ticket),
        "ticket in argv"
    );
    assert!(!cli.pane_text(&pane).contains(&ticket), "ticket on screen");
    for args in [
        &["show-environment", "-g"][..],
        &["show-options", "-g"][..],
        &["show-environment", "-t", "main"][..],
        &["list-windows", "-a", "-F", "#{window_name} #{pane_title}"][..],
    ] {
        let shown = cli.tmux.run_checked(args).unwrap_or_default();
        assert!(!shown.contains(&ticket), "ticket in tmux {args:?}");
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let refused = rt.block_on(async {
        let c = client::Client::connect(&cli.root.join("rt/dreamerd.sock"), "outsider")
            .await
            .expect("connect");
        c.call(api::Request::AppsRegister(api::RegisterApp {
            launch_id: id.clone(),
            ticket: ticket.clone(),
        }))
        .await
        .expect_err("not in the pane")
    });
    assert!(
        refused.to_string().contains("not running in pane"),
        "{refused}"
    );
    assert!(!ticket_file.exists(), "a refused ticket is used up too");
    let log = std::fs::read_to_string(cli.root.join("st/dreamerd.log")).unwrap_or_default();
    assert!(!log.contains(&ticket), "ticket in the daemon log");
}

/// An app launched for a narrow grant knows it from registering (a launch
/// grant need not list `grants.current`), so a `terminal` node outside it
/// is drawn as outside the app's grant, not left to the daemon's refusal.
#[test]
fn a_registered_apps_embed_outside_its_grant_is_marked() {
    let cli = Cli::new();
    let pkg = cli.root.join("embeds");
    std::fs::create_dir_all(&pkg).expect("package");
    std::fs::write(
        pkg.join("app.toml"),
        "schema_version = 2\nid = \"local.embeds\"\nversion = \"0.1.0\"\ntitle = \"Embeds\"\n\
         capabilities = [\"terminal.read\"]\nroot = \"main\"\n\n[nodes.main]\ntype = \"terminal\"\npane = 0\n",
    )
    .expect("manifest");
    let termapp = PathBuf::from(DREAM).with_file_name("dreamer-termapp");
    let manifest = pkg.join("app.toml").display().to_string();
    let out = cli.ok(&[
        "app",
        "run",
        "embeds",
        "--method",
        "terminal.read",
        "--",
        &termapp.display().to_string(),
        &manifest,
    ]);
    let (pane, _) = launched(&out);
    let shown = cli.wait_pane(&pane, "outside the app's grant");
    assert!(
        shown.contains("pane %0 is outside the app's grant"),
        "{shown}"
    );
}

#[test]
fn two_registered_apps_exchange_messages_with_stamped_senders() {
    let cli = Cli::new();
    // One app answers requests and prints what it receives.
    let out = cli.ok(&[
        "app",
        "run",
        "echo",
        "--method",
        "messages.*",
        "--",
        DREAM,
        "app",
        "probe",
        "--serve",
    ]);
    let (echo_pane, echo_launch) = launched(&out);
    let echo = format!("echo-{echo_launch}");
    cli.wait_pane(&echo_pane, &format!("probe: serving as {echo}"));
    // Another asks it, claiming to be the owner each time.
    let out = cli.ok(&[
        "app",
        "run",
        "asker",
        "--method",
        "messages.*",
        "--",
        DREAM,
        "app",
        "probe",
        "--ask",
        &echo,
    ]);
    let (ask_pane, ask_launch) = launched(&out);
    let asker = format!("asker-{ask_launch}");
    let shown = cli.wait_pane(&ask_pane, "probe: done");
    let line = |shown: &str, prefix: &str| {
        shown
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in:\n{shown}"))
            .to_owned()
    };
    assert!(
        line(&shown, "probe: reserved topic:").contains("reserved topic"),
        "{shown}"
    );
    assert_eq!(
        line(&shown, "probe: topic delivered"),
        "probe: topic delivered 1"
    );
    assert_eq!(
        line(&shown, "probe: reply from"),
        format!("probe: reply from {echo}: echo:ping (depth 1)")
    );
    // The server saw the real sender; "owner" was only a claim.
    let served = cli.wait_pane(&echo_pane, "probe: request from");
    assert_eq!(
        line(&served, "probe: chat.hello from"),
        format!("probe: chat.hello from {asker} claimed owner: hello")
    );
    assert_eq!(
        line(&served, "probe: request from"),
        format!("probe: request from {asker} claimed owner: ping")
    );
    // Messages are never typed into a pane: nothing but the probe's own
    // lines carries the payloads.
    assert!(
        !served
            .lines()
            .any(|l| l.trim() == "ping" || l.trim() == "hello")
    );
}

#[test]
fn a_pane_app_over_json_rpc_is_held_to_its_grant() {
    let cli = Cli::new();
    // A shell script in the pane registers through `dream rpc` and then
    // tries to stop the daemon; it may only read.
    let script = format!(
        "t=$(cat \"$DREAMER_LAUNCH_TICKET\"); \
         {{ printf '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"apps.register\",\"params\":{{\"launch_id\":\"%s\",\"ticket\":\"%s\"}}}}\\n' \"$DREAMER_LAUNCH_ID\" \"$t\"; \
         sleep 1; \
         printf '{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"daemon.shutdown\"}}\\n'; \
         sleep 1; }} | {DREAM} rpc; echo rpc-done; sleep 60"
    );
    let out = cli.ok(&[
        "app",
        "run",
        "jsonapp",
        "--method",
        "terminal.read",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    let (pane, _) = launched(&out);
    let shown = cli.wait_pane(&pane, "rpc-done");
    // Replies are one JSON object per line; a long one may wrap in the pane.
    let joined: String = shown.lines().map(str::trim_end).collect();
    let replies: Vec<serde_json::Value> =
        serde_json::Deserializer::from_str(&joined.replace("rpc-done", ""))
            .into_iter::<serde_json::Value>()
            .filter_map(Result::ok)
            .collect();
    let by_id = |id: u64| {
        replies
            .iter()
            .find(|r| r["id"] == serde_json::json!(id))
            .unwrap_or_else(|| panic!("no reply {id} in:\n{shown}"))
    };
    assert_eq!(by_id(1)["result"]["app_id"], "jsonapp", "{shown}");
    let refused = &by_id(2)["error"];
    assert_eq!(refused["code"], -32005, "{shown}");
    assert_eq!(
        refused["message"], "`daemon.shutdown` is not in this connection's grant",
        "the same refusal as a native client"
    );
    assert!(
        cli.run(&["status"]).status.success(),
        "the daemon still runs"
    );
}

impl Cli {
    fn write_extensions(&self, yaml: &str) {
        let dir = self.root.join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("extensions.yaml"), yaml).expect("extensions.yaml");
    }

    fn extensions(&self) -> serde_json::Value {
        let out = self.ok(&["extensions", "--json"]);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    /// The extension named `name` once `want` holds for it.
    fn extension_until(
        &self,
        name: &str,
        within: Duration,
        mut want: impl FnMut(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + within;
        loop {
            let all = self.extensions();
            let x = all["extensions"]
                .as_array()
                .and_then(|a| a.iter().find(|x| x["name"] == name).cloned())
                .unwrap_or_else(|| panic!("no extension {name}: {all}"));
            if want(&x) {
                return x;
            }
            assert!(Instant::now() < deadline, "{name} never got there: {x}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn extension_log(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(format!("st/extensions/{name}.log")))
            .unwrap_or_default()
    }
}

#[test]
fn a_crashing_extension_restarts_with_growing_delays_then_parks() {
    let cli = Cli::new();
    cli.write_extensions(&format!(
        "extensions:\n  - name: crashy\n    argv: [{DREAM}, extension-probe, --crash-after, '0']\n"
    ));
    cli.ok(&["ensure-running"]);
    // Watch each start; the gaps between them grow.
    let mut starts: Vec<u64> = Vec::new();
    let parked = cli.extension_until("crashy", Duration::from_secs(60), |x| {
        if let Some(t) = x["started_unix_ms"].as_u64()
            && starts.last() != Some(&t)
        {
            starts.push(t);
        }
        x["state"] == "failed"
    });
    assert_eq!(parked["failures"], 8, "{parked}");
    assert_eq!(parked["restarts"], 7, "{parked}");
    assert!(
        parked["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("exited"),
        "{parked}"
    );
    let gaps: Vec<u64> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.len() >= 5, "saw starts {starts:?}");
    assert!(gaps[gaps.len() - 1] > 2 * gaps[0], "delays grow: {gaps:?}");
    // Parked until asked; then it starts again.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(cli.extensions()["extensions"][0]["state"], "failed");
    cli.ok(&["extensions", "restart", "crashy"]);
    let again = cli.extension_until("crashy", Duration::from_secs(10), |x| {
        x["restarts"].as_u64() > Some(7)
    });
    assert_ne!(again["state"], "failed", "{again}");
    assert!(
        cli.extension_log("crashy")
            .contains("probe: crashing after 0 pings")
    );
}

#[test]
fn a_stalled_extension_is_restarted_and_terminals_keep_flowing() {
    let cli = Cli::new();
    cli.write_extensions(&format!(
        "extensions:\n  - name: stally\n    argv: [{DREAM}, extension-probe, --stall-after, '1']\n"
    ));
    cli.ok(&["ensure-running"]);
    cli.extension_until("stally", Duration::from_secs(10), |x| {
        x["state"] == "running"
    });
    // While it stalls, terminal calls answer promptly.
    for _ in 0..5 {
        let t = Instant::now();
        cli.ok(&["read", "%0"]);
        cli.ok(&["ls"]);
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "terminal calls took {:?}",
            t.elapsed()
        );
        std::thread::sleep(Duration::from_millis(150));
    }
    let x = cli.extension_until("stally", Duration::from_secs(10), |x| {
        x["restarts"].as_u64() >= Some(1)
    });
    assert!(
        x["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("stalled"),
        "{x}"
    );
    assert!(
        cli.extension_log("stally")
            .contains("probe: stalling after 1 pings")
    );
}

#[test]
fn an_extension_is_held_to_its_grant_and_messages_as_itself() {
    let cli = Cli::new();
    cli.write_extensions(&format!(
        "extensions:\n  - name: nosy\n    argv: [{DREAM}, extension-probe, --try, daemon.shutdown, --try, grants.current, --publish, chat.ext]\n    grant:\n      methods: [grants.current, messages.*]\n"
    ));
    cli.ok(&["ensure-running"]);
    cli.extension_until("nosy", Duration::from_secs(10), |x| x["state"] == "running");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let envelope = rt.block_on(async {
        let mut c = client::Client::connect(&cli.root.join("rt/dreamerd.sock"), "test")
            .await
            .expect("connect");
        let mut pushed = c.pushed().expect("pushed");
        c.call(api::Request::MessagesSubscribe(api::MessageSubscribe {
            topics: vec!["chat.*".into()],
        }))
        .await
        .expect("subscribe");
        // Start it again so it publishes while we listen.
        c.call(api::Request::ExtensionsRestart(api::ExtensionRef {
            name: "nosy".into(),
        }))
        .await
        .expect("restart");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, pushed.recv()).await {
                Ok(Some(client::Pushed::Event { topic, payload, .. }))
                    if topic == "messages.deliver" =>
                {
                    let d = <api::MessageDelivery as prost::Message>::decode(payload.as_slice())
                        .expect("delivery");
                    break d.envelope.expect("envelope");
                }
                Ok(Some(_)) => {}
                other => panic!("no message from the extension: {other:?}"),
            }
        }
    });
    let sender = envelope.sender.expect("stamped");
    assert_eq!(sender.instance, "extension:nosy");
    assert!(!sender.owner, "an extension is not the user");
    assert_eq!(envelope.claimed_sender.as_deref(), Some("owner"));
    let log = cli.extension_log("nosy");
    let reply = |id: &str| {
        log.lines()
            .filter_map(|l| l.strip_prefix("probe: reply "))
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["id"] == id)
            .unwrap_or_else(|| panic!("no reply {id} in:\n{log}"))
    };
    assert_eq!(reply("try-0")["error"]["code"], -32005, "{log}");
    assert_eq!(reply("try-1")["result"]["owner"], false, "{log}");
    assert!(
        cli.run(&["status"]).status.success(),
        "the daemon still runs"
    );
    // Stopping the daemon asks the extension to stop, and it is gone.
    let pid = cli.extensions()["extensions"][0]["pid"]
        .as_u64()
        .expect("running pid") as u32;
    assert!(cli.run(&["shutdown"]).status.success(), "shutdown");
    let deadline = Instant::now() + Duration::from_secs(10);
    while platform::process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "extension {pid} outlived the daemon"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(cli.extension_log("nosy").contains("probe: shutdown"));
}

#[test]
fn a_query_reaches_a_connected_host() {
    let cli = Cli::new();
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    let far_root = cli.root.join("far");
    let _far_daemon = DaemonGuard(far_root.join("rt"));
    let r = |name: &str| cli.root.join(name);
    std::fs::write(
        r("ssh"),
        format!(
            r#"#!/bin/sh
[ "$1" = -T ] || exit 90
while [ "$1" != -- ]; do shift; done
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
exec /bin/sh -c "exec $3"
"#,
            rt = far_root.join("rt").display(),
            st = far_root.join("st").display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(
        r("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::create_dir_all(r("config")).expect("config dir");
    std::fs::write(
        r("config/hosts.yaml"),
        format!(
            "hosts:\n  - name: devbox\n    destination: devbox\n    dream: '{}'\n    tmux: '{}'\n    tmux_socket: '{}'\n",
            DREAM,
            tmux_program().display(),
            far.socket()
        ),
    )
    .expect("hosts.yaml");
    far.run_checked(&["send-keys", "-t", "main", "echo far-''marker", "Enter"])
        .expect("far output");
    cli.ok(&["ensure-running"]);
    cli.ok(&["hosts", "connect", "devbox"]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let c = client::Client::connect(&cli.root.join("rt/dreamerd.sock"), "test")
            .await
            .expect("connect");
        for hosts in [vec!["devbox".to_owned()], vec!["*".to_owned()]] {
            let deadline = Instant::now() + Duration::from_secs(15);
            let r = loop {
                let q = api::TerminalQuery {
                    pattern: "^far-marker$".into(),
                    hosts: hosts.clone(),
                    ..api::TerminalQuery::default()
                };
                let Ok(api::Response::TerminalQuery(r)) =
                    c.call(api::Request::TerminalQuery(q)).await
                else {
                    panic!("query")
                };
                // The local pane's mirror may still be starting: wait for
                // both hosts' panes, not just the first match.
                if !r.matches.is_empty() && r.panes_searched >= 2 {
                    break r;
                }
                assert!(
                    Instant::now() < deadline,
                    "never searched local and remote panes: {r:?}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            assert_eq!(r.matches.len(), 1, "{r:?}");
            assert_eq!(r.matches[0].host.as_deref(), Some("devbox"), "{r:?}");
            assert_eq!(r.matches[0].text, "far-marker");
            assert!(r.panes_searched >= 2, "local and remote panes: {r:?}");
        }
        // An unknown host is a note, not a failure.
        let q = api::TerminalQuery {
            pattern: "x".into(),
            hosts: vec!["nowhere".into()],
            ..api::TerminalQuery::default()
        };
        let Ok(api::Response::TerminalQuery(r)) = c.call(api::Request::TerminalQuery(q)).await
        else {
            panic!("query")
        };
        assert!(
            r.notes.iter().any(|n| n.starts_with("host nowhere")),
            "{r:?}"
        );
    });
}

fn job_json(cli: &Cli, id: &str) -> serde_json::Value {
    let out = cli.ok(&["jobs", "--json"]);
    let all: serde_json::Value = serde_json::from_str(&out).expect("json");
    all["jobs"]
        .as_array()
        .and_then(|a| a.iter().find(|j| j["id"] == id).cloned())
        .unwrap_or_else(|| panic!("no job {id}: {out}"))
}

#[test]
fn a_job_outlives_the_daemon_and_records_its_exit_and_artifacts() {
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    let script = "echo started; echo data > \"$DREAMER_JOB_ARTIFACTS/out.txt\"; \
                  mkdir -p \"$DREAMER_JOB_ARTIFACTS/sub\"; echo y > \"$DREAMER_JOB_ARTIFACTS/sub/y\"; \
                  sleep 2; exit 3";
    let out = cli.ok(&[
        "jobs", "start", "--name", "build", "--", "/bin/sh", "-c", script,
    ]);
    assert!(out.contains("running"), "{out}");
    let id = out.split_whitespace().next().expect("id").to_owned();
    let pane = job_json(&cli, &id)["pane"].as_u64().expect("pane");
    // The daemon stops; the job, in its tmux window, does not.
    assert!(cli.run(&["shutdown"]).status.success());
    let panes = cli
        .tmux
        .run_checked(&["list-panes", "-a", "-F", "#{pane_id} #{window_name}"])
        .expect("panes");
    assert!(panes.contains(&format!("%{pane} build")), "{panes}");
    std::thread::sleep(Duration::from_secs(3));
    // A new daemon knows it, finished, with its status and its files.
    cli.ok(&["ensure-running"]);
    let j = job_json(&cli, &id);
    assert_eq!(
        (j["state"].as_str(), j["exit_code"].as_i64()),
        (Some("failed"), Some(3)),
        "{j}"
    );
    let out = cli.ok(&["jobs", "artifacts", &id]);
    assert!(
        out.contains("out.txt (5 bytes)") && out.contains("sub/y (2 bytes)"),
        "{out}"
    );
    // Its output is still in its pane (tmux keeps it in the pane's history
    // once the program is gone).
    let shown = cli
        .tmux
        .run_checked(&["capture-pane", "-p", "-S", "-", "-t", &format!("%{pane}")])
        .expect("capture");
    assert!(
        shown.contains("started") && shown.contains("[job exited with 3]"),
        "{shown}"
    );
}

#[test]
fn a_cancelled_job_ends_by_signal_and_says_so() {
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut c = rt
        .block_on(client::Client::connect(
            &cli.root.join("rt/dreamerd.sock"),
            "test",
        ))
        .expect("connect");
    let mut rx = c.pushed().expect("pushed");
    rt.block_on(c.call(api::Request::EventsSubscribe(api::EventSubscribe {
        kinds: vec!["jobs".into()],
        ..api::EventSubscribe::default()
    })))
    .expect("events");
    let out = cli.ok(&["jobs", "start", "--", "sleep", "100"]);
    let id = out.split_whitespace().next().expect("id").to_owned();
    wait_until("the job's program runs", 10, || {
        job_json(&cli, &id)["pid"].is_u64()
    });
    let out = cli.ok(&["jobs", "cancel", &id]);
    assert!(out.contains(&id), "{out}");
    wait_until("the job ends", 10, || {
        job_json(&cli, &id)["state"] == "killed"
    });
    let j = job_json(&cli, &id);
    assert_eq!(
        (j["signal"].as_i64(), j["cancelled"].as_bool()),
        (Some(15), Some(true)),
        "{j}"
    );
    // Its start and end were events.
    let states = rt.block_on(async {
        let mut states = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !states.iter().any(|s: &String| s == "killed") {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(client::Pushed::Event { topic, payload, .. }))
                    if topic == "events.batch" =>
                {
                    let b = <api::EventBatch as prost::Message>::decode(payload.as_slice())
                        .expect("batch");
                    states.extend(
                        b.events
                            .into_iter()
                            .filter(|e| e.subject.as_deref() == Some(id.as_str()))
                            .filter_map(|e| e.state),
                    );
                }
                Ok(Some(_)) => {}
                other => panic!("no job events: {other:?} after {states:?}"),
            }
        }
        states
    });
    assert_eq!(states, ["running", "killed"]);
}

#[test]
fn agent_runs_are_listed_here_and_on_a_host_without_their_helpers() {
    let cli = Cli::new();
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    let far_root = cli.root.join("far");
    let _far_daemon = DaemonGuard(far_root.join("rt"));
    let r = |name: &str| cli.root.join(name);
    std::fs::write(
        r("ssh"),
        format!(
            r#"#!/bin/sh
[ "$1" = -T ] || exit 90
while [ "$1" != -- ]; do shift; done
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
exec /bin/sh -c "exec $3"
"#,
            rt = far_root.join("rt").display(),
            st = far_root.join("st").display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(
        r("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::create_dir_all(r("config")).expect("config dir");
    std::fs::write(
        r("config/hosts.yaml"),
        format!(
            "hosts:\n  - name: devbox\n    destination: devbox\n    dream: '{}'\n    tmux: '{}'\n    tmux_socket: '{}'\n",
            DREAM,
            tmux_program().display(),
            far.socket()
        ),
    )
    .expect("hosts.yaml");
    // Programs named like the CLIs: codex here (a shell that starts a
    // helper), claude on the host.
    let bin = r("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::os::unix::fs::symlink("/bin/bash", bin.join("codex")).expect("codex");
    std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).expect("claude");
    cli.tmux
        .run_checked(&[
            "send-keys",
            "-t",
            "main",
            &format!("{} -c 'sleep 300 & wait'", bin.join("codex").display()),
            "Enter",
        ])
        .expect("codex");
    far.run_checked(&[
        "send-keys",
        "-t",
        "main",
        &format!("{} 300", bin.join("claude").display()),
        "Enter",
    ])
    .expect("claude");
    cli.ok(&["ensure-running"]);
    cli.ok(&["hosts", "connect", "devbox"]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let c = client::Client::connect(&cli.root.join("rt/dreamerd.sock"), "test")
            .await
            .expect("connect");
        let deadline = Instant::now() + Duration::from_secs(15);
        let runs = loop {
            let Ok(api::Response::AgentsRuns(l)) = c
                .call(api::Request::AgentsRuns(api::AgentRunQuery {
                    hosts: vec!["devbox".into()],
                    provider: None,
                }))
                .await
            else {
                panic!("runs")
            };
            let ours: Vec<api::AgentRun> =
                l.runs.into_iter().filter(|r| r.pane.is_some()).collect();
            if ours.len() >= 2 && ours.iter().any(|r| r.helpers >= 1) {
                break ours;
            }
            assert!(
                Instant::now() < deadline,
                "runs never showed: {ours:?} ({:?})",
                l.notes
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        // Exactly one run each: helpers are not runs.
        let codex: Vec<_> = runs.iter().filter(|r| r.provider == "codex").collect();
        let claude: Vec<_> = runs.iter().filter(|r| r.provider == "claude").collect();
        assert_eq!((codex.len(), claude.len()), (1, 1), "{runs:?}");
        assert_eq!(codex[0].host, None);
        assert_eq!(codex[0].helpers, 1, "the sleep it started");
        assert_eq!(claude[0].host.as_deref(), Some("devbox"));
        for r in &runs {
            assert_eq!(
                (
                    r.state.as_str(),
                    r.instrumentation.as_str(),
                    r.confidence.as_str()
                ),
                ("running", "passive", "high")
            );
            assert!(r.rss_kib > 0 && r.age_ms < 60_000, "{r:?}");
        }
    });
}

#[test]
fn the_hook_helper_returns_at_once_without_a_daemon() {
    let cli = Cli::new();
    for input in [
        r#"{"session_id":"s","hook_event_name":"PreToolUse","tool_name":"Bash"}"#,
        "not json at all",
        "",
    ] {
        let started = Instant::now();
        let mut child = Command::new(DREAM)
            .args(["hook", "claude"])
            .env("DREAMER_RUNTIME_DIR", cli.root.join("nobody-here"))
            .env("DREAMER_STATE_DIR", cli.root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("hook");
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "{input:?}: {out:?}");
        assert!(out.stdout.is_empty(), "nothing on stdout: {out:?}");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }
    assert!(
        !cli.root.join("nobody-here/dreamerd.sock").exists(),
        "it never starts a daemon"
    );
}

#[test]
fn hook_reports_drive_a_runs_state_until_they_stop() {
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    // A program named claude that reports like Claude Code's hooks would.
    let bin = cli.root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::os::unix::fs::symlink("/bin/bash", bin.join("claude")).expect("claude");
    let report = |event: &str, extra: &str| {
        format!(
            "printf '%s' '{{\"session_id\":\"s1\",\"hook_event_name\":\"{event}\",\"cwd\":\"/work\"{extra}}}' | {DREAM} hook claude\n"
        )
    };
    let script = cli.root.join("session.sh");
    std::fs::write(
        &script,
        format!(
            "export DREAMER_RUNTIME_DIR='{}'\n{}{}{}sleep 300\n",
            cli.root.join("rt").display(),
            report("SessionStart", ""),
            report("UserPromptSubmit", ""),
            report(
                "Notification",
                ",\"notification_type\":\"permission_prompt\""
            ),
        ),
    )
    .expect("script");
    cli.tmux
        .run_checked(&[
            "send-keys",
            "-t",
            "main",
            &format!("{} {}", bin.join("claude").display(), script.display()),
            "Enter",
        ])
        .expect("run");
    let runs = || -> serde_json::Value {
        let out = cli.ok(&["agents", "runs", "--json"]);
        serde_json::from_str(&out).expect("json")
    };
    let mut seen = serde_json::Value::Null;
    wait_until("the run reports waiting for permission", 15, || {
        seen = runs();
        seen["runs"]
            .as_array()
            .and_then(|r| {
                r.iter()
                    .find(|r| r["provider"] == "claude" && !r["pane"].is_null())
                    .cloned()
            })
            .is_some_and(|r| r["state"] == "waiting_for_permission")
    });
    let run = seen["runs"]
        .as_array()
        .and_then(|r| {
            r.iter()
                .find(|r| r["provider"] == "claude" && !r["pane"].is_null())
                .cloned()
        })
        .expect("run");
    assert_eq!(
        (run["instrumentation"].as_str(), run["session_id"].as_str()),
        (Some("hooks"), Some("s1")),
        "{run}"
    );
    // No reports for a while: stale, not still waiting.
    wait_until("the run goes stale", 15, || {
        runs()["runs"]
            .as_array()
            .and_then(|r| {
                r.iter()
                    .find(|r| r["provider"] == "claude" && !r["pane"].is_null())
                    .cloned()
            })
            .is_some_and(|r| r["state"] == "stale")
    });
}

#[test]
fn hooks_are_written_only_when_asked_with_a_backup_and_once() {
    let cli = Cli::new();
    let printed = cli.run(&["agents", "hooks"]);
    let snippet: serde_json::Value = serde_json::from_slice(&printed.stdout).expect("snippet");
    assert!(
        snippet["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .is_some_and(|c| c.ends_with("hook claude"))
    );
    let file = cli.root.join("settings.json");
    std::fs::write(
        &file,
        r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#,
    )
    .expect("settings");
    let w = cli.run(&["agents", "hooks", "--write", &file.display().to_string()]);
    assert!(w.status.success(), "{w:?}");
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("read")).expect("json");
    assert_eq!(after["model"], "opus");
    assert_eq!(
        after["hooks"]["Stop"].as_array().map(Vec::len),
        Some(2),
        "{after}"
    );
    assert!(cli.root.join("settings.json.bak").exists());
    let again = cli.run(&["agents", "hooks", "--write", &file.display().to_string()]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("already there"));
}

impl Cli {
    /// `dream mcp <args>` on the isolated tmux server, with stdio piped.
    fn mcp(&self, args: &[&str]) -> std::process::Child {
        let program = tmux_program().display().to_string();
        Command::new(DREAM)
            .arg("mcp")
            .args(args)
            .args(["-L", self.tmux.socket(), "--tmux", program.as_str()])
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon())
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env_remove("TMUX_PANE")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("dream mcp")
    }

    /// `dream mcp <args>` as an agent in pane `pane` (`%N`) starts it.
    fn mcp_in_pane(&self, pane: &str, args: &[&str]) -> std::process::Child {
        let program = tmux_program().display().to_string();
        Command::new(DREAM)
            .arg("mcp")
            .args(args)
            .args(["-L", self.tmux.socket(), "--tmux", program.as_str()])
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon())
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env("TMUX_PANE", pane)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("dream mcp")
    }

    /// `dream mcp <args>` working in `dir` (its apps directory), as a
    /// harness outside tmux starts it.
    fn mcp_in(&self, dir: &std::path::Path, args: &[&str]) -> std::process::Child {
        let program = tmux_program().display().to_string();
        Command::new(DREAM)
            .arg("mcp")
            .args(args)
            .args(["-L", self.tmux.socket(), "--tmux", program.as_str()])
            .current_dir(dir)
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon())
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env_remove("DREAMER_APPS_ROOT")
            .env_remove("TMUX_PANE")
            .env_remove("DREAMER_SESSION_ID")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("dream mcp")
    }
}

/// A 1x1 PNG, as a desktop writes one for `apps.capture`.
const DOT_PNG: [u8; 68] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0x00, 0x00,
    0x00, 0x06, 0x00, 0x02, 0x30, 0x81, 0xd0, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

/// A desktop as the daemon sees one, with no window: on a connection of
/// its own it waits for capture requests, and answers the first by
/// reading the pane's view tree and writing [`DOT_PNG`] where it was
/// asked. Returns the request and the tree it read. The caller waits for
/// [`desktop_present`] before asking for an image.
fn fake_desktop(socket: PathBuf) -> std::thread::JoinHandle<(api::AppCaptureRequest, String)> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let c = client::Client::connect(&socket, "desktop")
                .await
                .expect("connect");
            let deadline = Instant::now() + Duration::from_secs(60);
            let asked = loop {
                let r = match c
                    .call(api::Request::AppsCaptureWait(api::AppCaptureWait {
                        wait_ms: Some(1_000),
                    }))
                    .await
                {
                    Ok(api::Response::AppsCaptureWait(r)) => r,
                    other => panic!("apps.capture_wait: {other:?}"),
                };
                if r.id != 0 {
                    break r;
                }
                assert!(Instant::now() < deadline, "no capture was asked for");
            };
            let tree = match c
                .call(api::Request::AppsTree(api::AppTreeQuery {
                    pane: asked.pane,
                    after: 0,
                    seq: 0,
                    wait_ms: Some(1_000),
                }))
                .await
            {
                Ok(api::Response::AppsTree(t)) => t.tree,
                other => panic!("apps.tree: {other:?}"),
            };
            std::fs::write(&asked.path, DOT_PNG).expect("the image");
            c.call(api::Request::AppsCaptureDone(api::AppCaptureDone {
                id: asked.id,
                width: 1,
                height: 1,
                ..api::AppCaptureDone::default()
            }))
            .await
            .expect("apps.capture_done");
            (asked, tree)
        })
    })
}

/// Waits until the daemon counts a desktop that answers captures.
fn desktop_present(socket: &std::path::Path) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let c = rt
        .block_on(client::Client::connect(socket, "test"))
        .expect("connect");
    wait_until("the desktop waits for captures", 10, || {
        matches!(
            rt.block_on(c.call(api::Request::DaemonStatus(api::Empty {}))),
            Ok(api::Response::DaemonStatus(s)) if s.desktops > 0
        )
    });
}

/// The loop an agent follows to build an app, over MCP at the `own`
/// level: a new package; an edit with a type error, which the check
/// reports at its line and column; the fix; a text preview and the test;
/// an image preview, refused with no desktop, then made by a desktop (a
/// stand-in that answers captures) from a preview pane of the agent's
/// own that is closed again; the install; and the app opened beside a
/// pane the agent created. A directory outside the apps
/// directory, and a pane the agent did not create, are refused; at
/// `observe` installing and launching are refused.
#[test]
fn an_agent_builds_an_app_and_opens_it_over_mcp() {
    use serde_json::json;
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    let work = cli.root.join("work");
    std::fs::create_dir_all(&work).expect("work");
    let mut m = Mcp::start(cli.mcp_in(&work, &["--level", "own"]));
    let err = |r: &serde_json::Value| r["isError"] == true;
    let said = |r: &serde_json::Value| r["content"][0]["text"].as_str().unwrap_or("").to_owned();

    let created = m.call("app_new", json!({ "name": "hello" }));
    assert!(!err(&created), "{created}");
    let dir = PathBuf::from(created["structuredContent"]["dir"].as_str().expect("dir"));
    assert!(dir.ends_with("work/hello"), "{created}");

    let source = dir.join("app.ts");
    let good = std::fs::read_to_string(&source).expect("app.ts");
    std::fs::write(&source, good.replace("ui.table({", "ui.tabel({")).expect("edit");
    let checked = m.call("app_check", json!({ "dir": "hello" }));
    let c = &checked["structuredContent"];
    assert_eq!(c["ok"], false, "{checked}");
    let p = &c["problems"][0];
    let line = good
        .lines()
        .position(|l| l.contains("ui.table({"))
        .expect("the table")
        + 1;
    assert_eq!(p["file"], "app.ts", "{checked}");
    assert_eq!(p["line"], line, "{checked}");
    assert!(p["column"].as_u64().unwrap_or(0) > 0, "{checked}");
    assert!(
        p["message"].as_str().unwrap_or("").contains("ui.tabel"),
        "{checked}"
    );
    std::fs::write(&source, &good).expect("fix");
    let checked = m.call("app_check", json!({ "dir": "hello" }));
    assert_eq!(checked["structuredContent"]["ok"], true, "{checked}");

    let outside = m.call("app_check", json!({ "dir": "../elsewhere" }));
    assert!(err(&outside), "{outside}");
    let outside = m.call("app_check", json!({ "dir": "/etc" }));
    assert!(
        err(&outside) && said(&outside).contains("outside the apps directory"),
        "{outside}"
    );

    let text = m.call("app_preview", json!({ "dir": "hello", "mode": "text" }));
    assert!(!err(&text) && said(&text).contains("cargo"), "{text}");
    let tested = m.call("app_test", json!({ "dir": "hello" }));
    assert_eq!(tested["structuredContent"]["ok"], true, "{tested}");
    let image = m.call("app_preview", json!({ "dir": "hello", "mode": "image" }));
    assert!(
        err(&image) && said(&image).contains("no Dreamer window is running"),
        "{image}"
    );
    // With a desktop: the PNG it wrote, drawn from the app's fixtures.
    let socket = cli.root.join("rt/dreamerd.sock");
    let desktop = fake_desktop(socket.clone());
    desktop_present(&socket);
    let image = m.call("app_preview", json!({ "dir": "hello", "mode": "image" }));
    assert!(!err(&image), "{image}");
    assert_eq!(image["content"][0]["type"], "image", "{image}");
    assert_eq!(image["content"][0]["mimeType"], "image/png", "{image}");
    assert_eq!(
        image["content"][0]["data"],
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=",
        "{image}"
    );
    assert!(
        image["content"][1]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("1x1")),
        "{image}"
    );
    let (asked, tree) = desktop.join().expect("the desktop");
    assert!(asked.method.is_empty() && asked.seq > 0, "{asked:?}");
    assert!(
        tree.contains("Panes") && tree.contains("cargo"),
        "the preview drew the fixtures: {tree}"
    );
    // The preview pane was only for the image, and the file is gone.
    wait_until("the preview pane closes", 10, || {
        !cli.topology()["panes"]
            .as_array()
            .is_some_and(|a| a.iter().any(|p| p["id"] == asked.pane))
    });
    assert!(
        !std::path::Path::new(&asked.path).exists(),
        "{}",
        asked.path
    );

    let installed = m.call("app_install", json!({ "dir": "hello" }));
    assert!(!err(&installed), "{installed}");
    assert_eq!(installed["structuredContent"]["name"], "local.hello");

    // A pane of the agent's own, and the app opened beside it.
    let run = m.call(
        "terminal_run",
        json!({ "argv": ["/bin/sh"], "timeout_ms": 300 }),
    );
    let own = run["structuredContent"]["job"]["pane"]
        .as_u64()
        .unwrap_or_else(|| panic!("{run}"));
    let launched = m.call(
        "app_launch",
        json!({ "name": "local.hello", "beside": own }),
    );
    assert!(!err(&launched), "{launched}");
    let pane = launched["structuredContent"]["pane"]
        .as_u64()
        .expect("pane");
    cli.wait_pane(&format!("%{pane}"), "Enter selects the pane");
    let t = cli.topology();
    let window = |p: u64| {
        t["panes"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["id"] == p))
            .map(|x| x["window"].clone())
    };
    assert_eq!(window(pane), window(own), "beside it: {t}");
    let listed = m.call("terminals_list", json!({}));
    let owned = listed["structuredContent"]["terminals"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["pane"] == pane))
        .map(|x| x["owned"].clone());
    assert_eq!(owned, Some(json!(true)), "{listed}");
    // Beside a pane it did not create: not at `own`.
    let foreign = m.call("app_launch", json!({ "name": "local.hello", "beside": 0 }));
    assert!(
        err(&foreign) && said(&foreign).contains("not created by this connection"),
        "{foreign}"
    );
    drop(m);

    let mut o = Mcp::start(cli.mcp_in(&work, &["--level", "observe"]));
    let install = o.call("app_install", json!({ "dir": "hello" }));
    assert!(
        err(&install) && said(&install).contains("apps.launch"),
        "{install}"
    );
    let launch = o.call(
        "app_launch",
        json!({ "name": "local.hello", "new_window": true }),
    );
    assert!(
        err(&launch) && said(&launch).contains("launch apps"),
        "{launch}"
    );
    // Building and looking need no level.
    let checked = o.call("app_check", json!({ "dir": "hello" }));
    assert_eq!(checked["structuredContent"]["ok"], true, "{checked}");
    drop(o);

    // An agent never hands an app more than it holds: this app needs
    // actions.submit, which this agent's grant lacks.
    let mut n = Mcp::start(cli.mcp_in(
        &work,
        &[
            "--level",
            "own",
            "--method",
            "apps.launch",
            "--method",
            "tmux.topology",
        ],
    ));
    let wider = n.call(
        "app_launch",
        json!({ "name": "local.hello", "new_window": true }),
    );
    assert!(
        err(&wider) && said(&wider).contains("the app's grant: `actions.submit`"),
        "{wider}"
    );
}

/// A minimal MCP client over a child's stdio.
struct Mcp {
    child: std::process::Child,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    next: u64,
}

impl Mcp {
    fn start(mut child: std::process::Child) -> Mcp {
        use std::io::BufRead;
        let out = child.stdout.take().expect("stdout");
        let mut m = Mcp {
            child,
            lines: std::io::BufReader::new(out).lines(),
            next: 1,
        };
        let init = m.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" },
            }),
        );
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );
        m.send(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        m
    }

    fn send(&mut self, v: serde_json::Value) {
        use std::io::Write;
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{v}").expect("write");
        stdin.flush().expect("flush");
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next;
        self.next += 1;
        self.send(
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        loop {
            let line = self.lines.next().expect("a reply").expect("read");
            let v: serde_json::Value = serde_json::from_str(&line).expect("json");
            if v["id"] == id {
                return v;
            }
        }
    }

    fn call(&mut self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        let r = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": args }),
        );
        r["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_mcp_client_uses_the_tools_and_a_narrowed_server_stays_narrow() {
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    cli.tmux
        .run_checked(&["split-window", "-t", "main", "/bin/sh"])
        .expect("split");
    let mut m = Mcp::start(cli.mcp(&[]));
    let tools = m.request("tools/list", serde_json::json!({}));
    let mut names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "app_check",
            "app_install",
            "app_launch",
            "app_new",
            "app_preview",
            "app_test",
            "browser_open",
            "cancel_job",
            "check_action",
            "check_job",
            "collector_create",
            "collector_list",
            "collector_query",
            "collector_stop",
            "create_pane",
            "find_lines",
            "layout_close",
            "layout_move",
            "layout_rename",
            "layout_select",
            "layout_split",
            "layout_zoom",
            "list_filters",
            "list_hosts",
            "list_jobs",
            "list_panes",
            "list_runs",
            "memory_forget",
            "memory_recall",
            "memory_remember",
            "move_panes",
            "read_pane",
            "send_input",
            "start_job",
            "stop_filter",
            "terminal_read",
            "terminal_run",
            "terminal_search",
            "terminal_send",
            "terminal_watch",
            "terminals_list",
            "watch_lines",
            "web_click",
            "web_read",
            "web_screenshot",
            "web_type"
        ]
    );
    let panes = m.call("list_panes", serde_json::json!({}));
    assert!(!panes["isError"].as_bool().unwrap_or(false), "{panes}");
    assert_eq!(
        panes["structuredContent"]["panes"].as_array().map(Vec::len),
        Some(2),
        "{panes}"
    );
    let typed = m.call(
        "send_input",
        serde_json::json!({ "pane": 0, "text": "echo via-mcp\n" }),
    );
    assert_ne!(typed["isError"], true, "{typed}");
    cli.wait_pane("%0", "via-mcp");
    let found = m.call("find_lines", serde_json::json!({ "text": "VIA-MCP" }));
    assert_ne!(found["isError"], true, "{found}");
    assert_eq!(
        found["structuredContent"]["matches"][0]["pane"], 0,
        "{found}"
    );
    // An agent in %0 searching every pane does not find its own lines
    // there; naming its pane does.
    let mut inside = Mcp::start(cli.mcp_in_pane("%0", &[]));
    let mine = inside.call("find_lines", serde_json::json!({ "text": "VIA-MCP" }));
    assert_eq!(
        mine["structuredContent"]["matches"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "{mine}"
    );
    let named = inside.call(
        "terminal_search",
        serde_json::json!({ "pattern": "via-mcp", "panes": [0] }),
    );
    assert_eq!(
        named["structuredContent"]["matches"][0]["pane"], 0,
        "{named}"
    );
    drop(inside);
    let moved = m.call(
        "move_panes",
        serde_json::json!({ "panes": [1], "window": "gathered" }),
    );
    assert_ne!(moved["isError"], true, "{moved}");
    assert_eq!(
        moved["structuredContent"]["moves"][0]["state"], "completed",
        "{moved}"
    );
    assert_eq!(
        cli.tmux
            .run_checked(&["display-message", "-p", "-t", "%1", "#{window_name}"])
            .expect("window name")
            .trim(),
        "gathered"
    );
    drop(m);

    // A server narrowed to reading pane 0.
    let mut m = Mcp::start(cli.mcp(&["--method", "terminal.read", "--pane", "%0"]));
    let read = m.call("read_pane", serde_json::json!({ "pane": 0, "lines": 10 }));
    assert_ne!(read["isError"], true, "{read}");
    assert!(
        read["structuredContent"]["lines"]
            .to_string()
            .contains("via-mcp"),
        "{read}"
    );
    let other = m.call("read_pane", serde_json::json!({ "pane": 1 }));
    assert_eq!(other["isError"], true, "{other}");
    assert!(
        other["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("outside"),
        "{other}"
    );
    let input = m.call("send_input", serde_json::json!({ "pane": 0, "text": "x" }));
    assert_eq!(input["isError"], true, "{input}");
    assert!(
        input["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("not in this connection's grant"),
        "{input}"
    );
    drop(m);

    // An agent's server at the observe level reads every pane and acts on
    // none.
    let mut m = Mcp::start(cli.mcp(&["--level", "observe"]));
    let listed = m.call("terminals_list", serde_json::json!({}));
    assert_ne!(listed["isError"], true, "{listed}");
    assert_eq!(
        listed["structuredContent"]["terminals"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "{listed}"
    );
    let read = m.call(
        "terminal_read",
        serde_json::json!({ "pane": 0, "lines": 20 }),
    );
    assert!(
        read["structuredContent"]["lines"]
            .to_string()
            .contains("via-mcp"),
        "{read}"
    );
    // Pages are not an agent's to read unless its grant says so.
    let page = m.call("web_read", serde_json::json!({ "pane": 0 }));
    assert_eq!(page["isError"], true, "{page}");
    assert!(
        page["content"][0]["text"]
            .to_string()
            .contains("`web.read` is not in this connection's grant"),
        "{page}"
    );
    let mut inspecting = Mcp::start(cli.mcp(&["--level", "observe", "--web-inspect"]));
    let page = inspecting.call("web_read", serde_json::json!({ "pane": 0 }));
    assert!(
        page["content"][0]["text"]
            .to_string()
            .contains("no Dreamer window is running"),
        "{page}"
    );
    drop(inspecting);
    let typed = m.call(
        "terminal_send",
        serde_json::json!({ "pane": 0, "text": "x" }),
    );
    assert_eq!(typed["isError"], true, "{typed}");
    drop(m);

    // A filter pane's grant: content read and filters.manage on pane 0.
    let mut m = Mcp::start(cli.mcp(&[
        "--method",
        "terminal.query",
        "--method",
        api::MANAGE_FILTERS,
        "--pane",
        "%0",
    ]));
    let watch = m.call(
        "watch_lines",
        serde_json::json!({ "name": "errs", "text": "ERROR", "panes": [0], "mark": "highlight" }),
    );
    assert_ne!(watch["isError"], true, "{watch}");
    let wide = m.call(
        "watch_lines",
        serde_json::json!({ "name": "wide", "text": "ERROR", "panes": [1] }),
    );
    assert_eq!(wide["isError"], true, "{wide}");
    let listed = m.call("list_filters", serde_json::json!({}));
    assert_eq!(
        listed["structuredContent"]["filters"][0]["name"], "errs",
        "{listed}"
    );
    let stopped = m.call("stop_filter", serde_json::json!({ "name": "errs" }));
    assert_ne!(stopped["isError"], true, "{stopped}");
}

/// The names a server's `tools/list` holds, sorted.
fn tool_names(m: &mut Mcp) -> Vec<String> {
    let tools = m.request("tools/list", serde_json::json!({}));
    let mut names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// A narrowed server lists only the tools its grant lets it call, so an
/// agent does not spend turns on refusals; calling another is still
/// refused. A monitor granted `jobs.list` finds the job that failed.
#[test]
fn a_narrowed_mcp_server_lists_only_the_tools_it_may_call() {
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    let mut owner = Mcp::start(cli.mcp(&[]));
    let mut ids = Vec::new();
    for code in [0, 3] {
        let started = owner.call(
            "start_job",
            serde_json::json!({ "argv": ["sh", "-c", format!("exit {code}")] }),
        );
        assert_ne!(started["isError"], true, "{started}");
        ids.push(
            started["structuredContent"]["id"]
                .as_str()
                .expect("id")
                .to_owned(),
        );
    }
    drop(owner);

    let mut monitor = Mcp::start(cli.mcp(&[
        "--method",
        "jobs.list",
        "--method",
        "terminal.query",
        "--method",
        "tmux.topology",
        "--all-panes",
    ]));
    assert_eq!(
        tool_names(&mut monitor),
        [
            "app_check",
            "app_new",
            "app_preview",
            "app_test",
            "find_lines",
            "list_jobs",
            "list_panes",
            "terminal_search",
        ]
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let jobs = loop {
        let j = monitor.call("list_jobs", serde_json::json!({}));
        assert_ne!(j["isError"], true, "{j}");
        let ended = j["structuredContent"]["jobs"]
            .as_array()
            .is_some_and(|a| a.len() == 2 && a.iter().all(|x| x["state"] != "running"));
        if ended {
            break j;
        }
        assert!(Instant::now() < deadline, "the jobs never ended: {j}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let failed: Vec<&serde_json::Value> = jobs["structuredContent"]["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter(|x| x["state"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{jobs}");
    assert_eq!(failed[0]["id"], ids[1].as_str(), "{jobs}");
    assert_eq!(failed[0]["exit_code"], 3, "{jobs}");
    // A tool it does not list is still refused when called.
    let read = monitor.call("read_pane", serde_json::json!({ "pane": 0 }));
    assert_eq!(read["isError"], true, "{read}");
    drop(monitor);

    // At observe: the reading tools, none that act.
    let mut observer = Mcp::start(cli.mcp(&["--level", "observe"]));
    let names = tool_names(&mut observer);
    for listed in [
        "terminals_list",
        "terminal_read",
        "list_jobs",
        "check_job",
        "collector_create",
        "memory_recall",
    ] {
        assert!(names.iter().any(|n| n == listed), "{listed}: {names:?}");
    }
    for hidden in [
        "terminal_send",
        "terminal_run",
        "send_input",
        "layout_split",
        "start_job",
        "cancel_job",
        "collector_stop",
        "memory_forget",
        "browser_open",
        "app_launch",
        "app_install",
        "web_read",
        "watch_lines",
        "list_hosts",
    ] {
        assert!(!names.iter().any(|n| n == hidden), "{hidden}: {names:?}");
    }
    drop(observer);

    // Pages: reading at observe with web.inspect, clicking from own up.
    let mut inspecting = Mcp::start(cli.mcp(&["--level", "observe", "--web-inspect"]));
    let names = tool_names(&mut inspecting);
    assert!(names.iter().any(|n| n == "web_read"), "{names:?}");
    assert!(!names.iter().any(|n| n == "web_click"), "{names:?}");
    drop(inspecting);
    let mut acting = Mcp::start(cli.mcp(&["--level", "own", "--web-inspect"]));
    let names = tool_names(&mut acting);
    for listed in [
        "web_click",
        "terminal_send",
        "start_job",
        "browser_open",
        "app_launch",
        "app_install",
    ] {
        assert!(names.iter().any(|n| n == listed), "{listed}: {names:?}");
    }
    drop(acting);

    // Reaching connected hosts: the host tools, through hosts.call.
    let mut hosts = Mcp::start(cli.mcp(&["--method", "hosts.call", "--method", "hosts.list"]));
    let names = tool_names(&mut hosts);
    for listed in [
        "list_hosts",
        "list_panes",
        "read_pane",
        "check_job",
        "list_jobs",
        "start_job",
    ] {
        assert!(names.iter().any(|n| n == listed), "{listed}: {names:?}");
    }
    assert!(!names.iter().any(|n| n == "terminal_read"), "{names:?}");
}

/// An agent at the observe level keeps the test-failures preset over every
/// pane; failing tests in another pane become rows, grouped by test.
#[test]
fn an_mcp_agent_collects_test_failures_from_another_pane() {
    let cli = Cli::new();
    cli.ok(&["ensure-running"]);
    cli.tmux
        .run_checked(&["split-window", "-t", "main", "/bin/sh"])
        .expect("split");
    let mut m = Mcp::start(cli.mcp(&["--level", "observe"]));
    let created = m.call(
        "collector_create",
        serde_json::json!({ "preset": "test-failures" }),
    );
    assert_ne!(created["isError"], true, "{created}");
    assert_eq!(
        created["structuredContent"]["name"], "test-failures",
        "{created}"
    );
    let bad = m.call("collector_create", serde_json::json!({ "preset": "nope" }));
    assert_eq!(bad["isError"], true, "{bad}");

    // Two runs in pane 1: `a` fails twice, `b` once; a passing test is no
    // row.
    for out in [
        r"test a ... FAILED\ntest ok ... ok\n",
        r"test a ... FAILED\ntest b ... FAILED\n",
    ] {
        cli.tmux
            .run_checked(&["send-keys", "-t", "%1", &format!("printf '{out}'"), "Enter"])
            .expect("type");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let grouped = loop {
        let q = m.call(
            "collector_query",
            serde_json::json!({ "name": "test-failures", "group_by": "test" }),
        );
        assert_ne!(q["isError"], true, "{q}");
        if q["structuredContent"]["matched"] == 3 {
            break q;
        }
        assert!(Instant::now() < deadline, "never three rows: {q}");
        std::thread::sleep(Duration::from_millis(200));
    };
    let groups = &grouped["structuredContent"]["groups"];
    assert_eq!(groups[0]["value"], "a", "{grouped}");
    assert_eq!(groups[0]["count"], 2, "{grouped}");
    assert_eq!(groups[1]["value"], "b", "{grouped}");
    let only_b = m.call(
        "collector_query",
        serde_json::json!({ "name": "test-failures", "fields": { "test": "b" }, "panes": [1] }),
    );
    let rows = &only_b["structuredContent"]["rows"];
    assert_eq!(rows.as_array().map(Vec::len), Some(1), "{only_b}");
    assert_eq!(rows[0]["pane"], 1, "{only_b}");
    let listed = m.call("collector_list", serde_json::json!({}));
    assert_eq!(
        listed["structuredContent"]["collectors"][0]["spec"]["preset"], "test-failures",
        "{listed}"
    );
    // Stopping a collector, which may be the user's, is an action:
    // refused at observe, allowed from own up.
    let refused = m.call(
        "collector_stop",
        serde_json::json!({ "name": "test-failures", "delete": true }),
    );
    assert_eq!(refused["isError"], true, "{refused}");
    let mut own = Mcp::start(cli.mcp(&["--level", "own"]));
    let stopped = own.call(
        "collector_stop",
        serde_json::json!({ "name": "test-failures", "delete": true }),
    );
    assert_ne!(stopped["isError"], true, "{stopped}");
}

/// "Run this check on two connected hosts and show each result".
/// Through `dream mcp`, a job starts on this host and on a connected one,
/// and each result comes back separately, naming its host.
#[test]
fn an_mcp_client_runs_a_check_here_and_on_a_connected_host() {
    let cli = Cli::new();
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    let far_root = cli.root.join("far");
    let _far_daemon = DaemonGuard(far_root.join("rt"));
    let r = |name: &str| cli.root.join(name);
    std::fs::write(
        r("ssh"),
        format!(
            r#"#!/bin/sh
[ "$1" = -T ] || exit 90
while [ "$1" != -- ]; do shift; done
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
exec /bin/sh -c "exec $3"
"#,
            rt = far_root.join("rt").display(),
            st = far_root.join("st").display(),
            daemon = Cli::daemon().display(),
        ),
    )
    .expect("stand-in ssh");
    std::fs::set_permissions(
        r("ssh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::create_dir_all(r("config")).expect("config dir");
    std::fs::write(
        r("config/hosts.yaml"),
        format!(
            "hosts:\n  - name: devbox\n    destination: devbox\n    dream: '{}'\n    tmux: '{}'\n    tmux_socket: '{}'\n",
            DREAM,
            tmux_program().display(),
            far.socket()
        ),
    )
    .expect("hosts.yaml");
    cli.ok(&["ensure-running"]);
    cli.ok(&["hosts", "connect", "devbox"]);

    let mut m = Mcp::start(cli.mcp(&[]));
    let hosts = m.call("list_hosts", serde_json::json!({}));
    assert_ne!(hosts["isError"], true, "{hosts}");
    assert!(hosts.to_string().contains("devbox"), "{hosts}");

    let mut ids = Vec::new();
    for (host, code) in [(None, 0), (Some("devbox"), 3)] {
        let started = m.call(
            "start_job",
            serde_json::json!({
                "argv": ["sh", "-c", format!("exit {code}")],
                "host": host,
            }),
        );
        assert_ne!(started["isError"], true, "{started}");
        let id = started["structuredContent"]["id"]
            .as_str()
            .expect("job id")
            .to_owned();
        ids.push((host, id, code));
    }
    for (host, id, code) in ids {
        let deadline = Instant::now() + Duration::from_secs(20);
        let job = loop {
            let j = m.call("check_job", serde_json::json!({ "id": id, "host": host }));
            assert_ne!(j["isError"], true, "{j}");
            if j["structuredContent"]["state"] != "running" {
                break j;
            }
            assert!(Instant::now() < deadline, "job never ended: {j}");
            std::thread::sleep(Duration::from_millis(100));
        };
        let s = &job["structuredContent"];
        assert_eq!(s["host"].as_str(), host, "{job}");
        if code == 0 {
            assert_eq!(s["state"], "succeeded", "{job}");
        } else {
            assert_eq!(s["state"], "failed", "{job}");
            assert_eq!(s["exit_code"], code, "{job}");
        }
    }
    // The far host's jobs, listed there.
    let far_jobs = m.call("list_jobs", serde_json::json!({ "host": "devbox" }));
    assert_ne!(far_jobs["isError"], true, "{far_jobs}");
    assert_eq!(
        far_jobs["structuredContent"]["host"], "devbox",
        "{far_jobs}"
    );
    assert_eq!(
        far_jobs["structuredContent"]["jobs"][0]["exit_code"], 3,
        "{far_jobs}"
    );
    // A host that is not connected is a tool error naming it.
    let nowhere = m.call("list_panes", serde_json::json!({ "host": "nowhere" }));
    assert_eq!(nowhere["isError"], true, "{nowhere}");
    assert!(nowhere.to_string().contains("nowhere"), "{nowhere}");
}

/// The small-app builder: A generated monitor package is
/// checked, previewed from its fixtures with no daemon, installed (the
/// version it replaces kept as the last good one), and launched in a real
/// tmux pane with the grant it declares, where it follows live data with
/// no GUI. A broken version is refused and changes nothing.
#[test]
fn a_generated_monitor_package_is_built_installed_and_runs_in_a_pane() {
    let cli = Cli::new();
    let pkg = cli.root.join("jobs-monitor");
    std::fs::create_dir_all(pkg.join("fixtures")).expect("package");
    let manifest = r#"schema_version = 1
id = "local.jobs-monitor"
version = "0.1.0"
title = "Jobs"
capabilities = ["jobs.list"]
all_panes = true                # job events are about the jobs' panes

[data.jobs]
method = "jobs.list"
path = "jobs"
events = ["jobs.state"]

[view]
type = "table"
source = "jobs"
row_key = "id"
columns = ["argv", "state"]
"#;
    std::fs::write(pkg.join("app.toml"), manifest).expect("manifest");
    std::fs::write(
        pkg.join("fixtures/jobs.json"),
        r#"{"jobs":[{"id":"a","argv":["fixture-build"],"state":"running"}]}"#,
    )
    .expect("fixture");
    let path = pkg.display().to_string();
    let checked = cli.run(&["app", "check", &path]);
    assert!(checked.status.success(), "{checked:?}");
    let said = String::from_utf8_lossy(&checked.stdout);
    assert!(said.contains("needs jobs.list, events.subscribe"), "{said}");
    let preview = cli.run(&["app", "preview", &path, "--size", "60x8"]);
    let drawn = String::from_utf8_lossy(&preview.stdout);
    assert!(
        preview.status.success() && drawn.contains("fixture-build") && drawn.contains("preview"),
        "{preview:?}"
    );
    assert!(cli.run(&["app", "install", &path]).status.success());

    // A broken update: refused, and the installed version is untouched.
    let broken = cli.root.join("broken");
    std::fs::create_dir_all(&broken).expect("broken");
    std::fs::write(
        broken.join("app.toml"),
        manifest.replace("type = \"table\"", "type = \"tabel\""),
    )
    .expect("broken manifest");
    let refused = cli.run(&[
        "app",
        "install",
        &broken.display().to_string(),
        "--name",
        "local.jobs-monitor",
    ]);
    assert!(!refused.status.success(), "{refused:?}");
    let installed = cli.root.join("config/apps/local.jobs-monitor/app.toml");
    assert_eq!(
        std::fs::read_to_string(&installed).expect("installed"),
        manifest
    );
    // A second good install keeps the first as the last good one.
    assert!(cli.run(&["app", "install", &path]).status.success());
    let listed = String::from_utf8_lossy(&cli.run(&["app", "list"]).stdout).into_owned();
    assert!(
        listed.contains("local.jobs-monitor 0.1.0  Jobs  (last good kept)"),
        "{listed}"
    );

    let out = cli.ok(&["app", "launch", "local.jobs-monitor"]);
    let (pane, _) = launched(&out);
    cli.wait_pane(&pane, "[live]");
    // Live data, followed by events: a job started now shows up.
    cli.ok(&["jobs", "start", "--", "/bin/sh", "-c", "echo from-the-test"]);
    let shown = cli.wait_pane(&pane, "from-the-test");
    assert!(
        !shown.contains("fixture-build"),
        "fixtures are for previews:\n{shown}"
    );
    cli.wait_pane(&pane, "succeeded");
}

#[test]
fn the_drawer_toggles_from_a_shell_and_asks_before_replacing_its_harness() {
    let cli = Cli::new();
    std::fs::create_dir_all(cli.root.join("config")).expect("config");
    std::fs::write(
        cli.root.join("config/dreamer.yaml"),
        "agents:\n  default: claude\n  claude:\n    command: ['/bin/sh', '-c', 'echo DRAWER''''HARNESS; exec sleep 600']\n",
    )
    .expect("settings");
    let shown = cli.ok(&["drawer", "toggle", "--session", "main"]);
    assert!(
        shown.contains("shown in @") && shown.contains("(claude)"),
        "{shown}"
    );
    let status: serde_json::Value =
        serde_json::from_str(&cli.ok(&["drawer", "status", "--session", "main", "--json"]))
            .expect("json");
    let pane = status["pane"].as_u64().expect("pane");
    cli.wait_for_text(&format!("%{pane}"), "DRAWERHARNESS");
    let refused = cli.on_tmux(&["drawer", "new", "--session", "main"]);
    assert!(!refused.status.success());
    let why = String::from_utf8_lossy(&refused.stderr);
    assert!(why.contains("--yes"), "{why}");
    let hidden = cli.ok(&["drawer", "toggle", "--session", "main"]);
    assert!(hidden.contains("hidden"), "{hidden}");
    let ls = cli.ok(&["ls"]);
    assert!(
        ls.contains("(hidden drawers)") && ls.contains("(drawer of "),
        "{ls}"
    );
    let fresh: serde_json::Value =
        serde_json::from_str(&cli.ok(&["drawer", "new", "--yes", "--session", "main", "--json"]))
            .expect("json");
    assert_eq!(fresh["created"], true);
    assert_ne!(fresh["pane"].as_u64(), Some(pane));
}

/// `dream memory` with `provider: files`: what it remembers is a plain
/// file in the project, a person's edit is what it shows next, and forget
/// takes the file.
#[test]
fn dream_memory_keeps_plain_files_a_person_can_edit() {
    let cli = Cli::new();
    std::fs::create_dir_all(cli.root.join("config")).expect("config");
    std::fs::write(
        cli.root.join("config/dreamer.yaml"),
        "memory:\n  provider: files\n",
    )
    .expect("settings");
    let project = cli.root.join("work/app");
    std::fs::create_dir_all(project.join(".git")).expect("project");
    let cwd = project.display().to_string();
    let out = cli.ok(&[
        "memory",
        "remember",
        "--kind",
        "feedback",
        "--scope",
        "project",
        "--cwd",
        &cwd,
        "--title",
        "Small commits",
        "--",
        "Keep",
        "commits",
        "small",
    ]);
    assert!(out.starts_with("remembered project/small-commits"), "{out}");
    let file = project.join(".dreamer/memory/small-commits.md");
    let text = std::fs::read_to_string(&file).expect("file");
    assert!(text.starts_with("---\nname: Small commits\n"), "{text}");
    assert!(text.ends_with("---\n\nKeep commits small\n"), "{text}");
    std::fs::write(
        &file,
        text.replace("Keep commits small", "Keep commits tiny"),
    )
    .expect("edit");

    let list: serde_json::Value =
        serde_json::from_str(&cli.ok(&["memory", "list", "--cwd", &cwd, "--json"])).expect("json");
    assert_eq!(list["items"][0]["id"], "project/small-commits");
    assert_eq!(list["items"][0]["kind"], "feedback");
    assert_eq!(list["items"][0]["text"], "Keep commits tiny");
    let recalled = cli.ok(&["memory", "recall", "tiny", "--cwd", &cwd]);
    assert!(recalled.contains("Keep commits tiny"), "{recalled}");
    assert!(
        cli.ok(&["memory", "recall", "tiny", "--cwd", &cwd, "--scope", "user"])
            .contains("nothing recalled")
    );

    cli.ok(&["memory", "forget", "project/small-commits", "--cwd", &cwd]);
    assert!(!file.exists());
    let index = std::fs::read_to_string(project.join(".dreamer/memory/MEMORY.md")).expect("index");
    assert_eq!(index, "");
    let out = cli.on_tmux(&["memory", "get", "project/small-commits", "--cwd", &cwd]);
    assert!(!out.status.success());
}

#[test]
fn app_check_reports_type_mistakes_in_an_app_ts_with_their_places() {
    let root = PathBuf::from(format!("/tmp/drm-cli-types-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pkg = root.join("panes");
    std::fs::create_dir_all(&pkg).expect("package");
    std::fs::write(
        pkg.join("app.toml"),
        "schema_version = 2\nid = \"local.panes\"\nversion = \"0.1.0\"\ntitle = \"Panes\"\n\
         capabilities = [\"tmux.topology\", \"terminal.read\"]\nall_panes = true\n\n\
         [script]\nsource = \"app.ts\"\n",
    )
    .expect("manifest");
    let good = "import { app, ui, dreamer } from \"dreamer\";\n\
                app({\n\
                \x20 async load() {\n\
                \x20   const t = await dreamer.tmux.topology();\n\
                \x20   return await dreamer.terminal.read({ pane: 1 });\n\
                \x20 },\n\
                \x20 view: () => ui.text(\"panes\"),\n\
                });\n";
    let bad = good
        .replace("tmux.topology()", "tmux.topolgy()")
        .replace("pane: 1", "pane: \"1\"");
    // Checking starts no daemon; the guards are there in case it would.
    let dream = |args: &[&str]| {
        Command::new(DREAM)
            .args(args)
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .output()
            .expect("dream")
    };
    let path = pkg.display().to_string();

    std::fs::write(pkg.join("app.ts"), &bad).expect("app.ts");
    let out = dream(&["app", "check", &path, "--json"]);
    assert!(!out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["ok"], false, "{v}");
    let problems = v["problems"].as_array().expect("problems");
    assert_eq!(problems.len(), 2, "{v}");
    let at = |p: &serde_json::Value| {
        (
            p["file"].as_str().unwrap_or_default().to_owned(),
            p["line"].as_u64().unwrap_or_default(),
            p["column"].as_u64().unwrap_or_default(),
        )
    };
    assert_eq!(at(&problems[0]), ("app.ts".into(), 4, 34), "{v}");
    let said = problems[0]["message"].as_str().unwrap_or_default();
    assert!(
        said.contains("dreamer.tmux.topolgy") && said.contains("`topology`"),
        "{said}"
    );
    assert_eq!(at(&problems[1]), ("app.ts".into(), 5, 48), "{v}");
    let said = problems[1]["message"].as_str().unwrap_or_default();
    assert!(
        said.contains("`pane`") && said.contains("not a string"),
        "{said}"
    );

    // Plain output names the same places.
    let out = dream(&["app", "check", &path]);
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        text.contains("app.ts:4:34: ") && text.contains("app.ts:5:48: "),
        "{text}"
    );

    std::fs::write(pkg.join("app.ts"), good).expect("app.ts");
    let out = dream(&["app", "check", &path, "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["problems"], serde_json::json!([]), "{v}");
    assert_eq!(v["ok"], true, "{v}");

    // The declarations an editor reads.
    assert!(dream(&["app", "types", &path]).status.success());
    let dts = std::fs::read_to_string(pkg.join("types/dreamer.d.ts")).expect("d.ts");
    assert!(dts.contains("topology(params?: In<TopologyParams>): Promise<Topology>;"));
    assert!(pkg.join("tsconfig.json").is_file());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn each_template_checks_previews_and_passes_its_test_straight_after_new() {
    let root = PathBuf::from(format!("/tmp/drm-cli-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    // None of this starts a daemon; the guards are there in case it would.
    let dream = |args: &[&str]| {
        Command::new(DREAM)
            .args(args)
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .output()
            .expect("dream")
    };
    let said = |o: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    for (template, shows) in [
        ("table", "cargo"),
        ("form", "Rename a window"),
        ("chat", "Type a message"),
    ] {
        let dir = root.join(template);
        let path = dir.display().to_string();
        let name = format!("my-{template}");
        let made = dream(&["app", "new", &name, "--template", template, "--dir", &path]);
        assert!(made.status.success(), "{template}: {}", said(&made));
        assert!(dir.join("types/dreamer.d.ts").is_file());
        let checked = dream(&["app", "check", &path]);
        assert!(checked.status.success(), "{template}: {}", said(&checked));
        assert!(said(&checked).contains(&format!("local.my-{template}")));
        let preview = dream(&["app", "preview", &path, "--text"]);
        assert!(
            preview.status.success() && said(&preview).contains(shows),
            "{template}: {}",
            said(&preview)
        );
        let tested = dream(&["app", "test", &path]);
        assert!(tested.status.success(), "{template}: {}", said(&tested));
        assert!(said(&tested).starts_with("ok: "), "{}", said(&tested));
    }

    // The table's test walks to the second row and presses Enter, whose
    // call is answered from fixtures/calls.json.
    let table = root.join("table");
    let last = std::fs::read_to_string(table.join("fixtures/expect/002.txt")).expect("expect");
    assert!(
        last.contains("selected %2") && last.contains("--- panes: row 2 selected"),
        "{last}"
    );

    // A deliberately broken expectation fails with the rows that differ.
    let broken = last.replace("selected %2", "selected %3");
    std::fs::write(table.join("fixtures/expect/002.txt"), &broken).expect("break");
    let path = table.display().to_string();
    let failed = dream(&["app", "test", &path]);
    let text = said(&failed);
    assert!(!failed.status.success(), "{text}");
    assert!(
        text.contains(r#"after event 2 {"kind":"key","key":"Enter"}"#)
            && text.contains("- selected %3")
            && text.contains("+ selected %2")
            && text.contains("1 of 3 screens differ"),
        "{text}"
    );
    // --update records what it draws now, and the test passes again.
    assert!(dream(&["app", "test", &path, "--update"]).status.success());
    assert!(dream(&["app", "test", &path]).status.success());

    // There is no web template yet: said, not faked.
    let web_dir = root.join("web");
    let web = dream(&[
        "app",
        "new",
        "site",
        "--template",
        "web",
        "--dir",
        &web_dir.display().to_string(),
    ]);
    assert!(!web.status.success() && said(&web).contains("no web template"));
    assert!(!web_dir.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_preview_pane_follows_saves_and_an_image_needs_a_desktop() {
    let cli = Cli::new();
    let dir = cli.root.join("demo");
    let path = dir.display().to_string();
    let made = cli.run(&["app", "new", "demo", "--template", "table", "--dir", &path]);
    assert!(made.status.success(), "{made:?}");
    let png = cli.root.join("demo.png");
    let png_path = png.display().to_string();
    let no_desktop = "no Dreamer window is running; --png needs one";
    // No daemon at all: no desktop either, and none is started for it.
    let out = cli.run(&["app", "preview", &path, "--png", &png_path]);
    assert!(
        !out.status.success() && String::from_utf8_lossy(&out.stderr).contains(no_desktop),
        "{out:?}"
    );
    assert!(!cli.root.join("rt/dreamerd.sock").exists());

    // A live preview in a new tmux window: the host draws the fixtures.
    let out = cli.ok(&["app", "preview", &path, "--open"]);
    assert!(out.contains("no Dreamer window is running"), "{out}");
    let pane = out
        .split_whitespace()
        .find(|w| w.starts_with('%'))
        .expect("pane")
        .trim_end_matches(';')
        .to_owned();
    cli.wait_pane(&pane, "cargo");
    cli.wait_pane(&pane, "preview from fixtures");

    // A save rebuilds the app and the pane follows.
    let ts = std::fs::read_to_string(dir.join("app.ts")).expect("app.ts");
    let saved = ts.replace("Enter selects the pane", "Saved once more");
    assert_ne!(ts, saved);
    std::fs::write(dir.join("app.ts"), &saved).expect("save");
    let at = Instant::now();
    let shown = cli.wait_pane(&pane, "Saved once more");
    println!("the preview pane followed the save in {:?}", at.elapsed());
    assert!(shown.contains("reloaded 1x"), "{shown}");
    // So does the tree the desktop draws from.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let id: u32 = pane[1..].parse().expect("pane id");
    let tree = rt.block_on(async {
        let c = client::Client::connect(&cli.root.join("rt/dreamerd.sock"), "test")
            .await
            .expect("connect");
        match c
            .call(api::Request::AppsTree(api::AppTreeQuery {
                pane: id,
                after: 0,
                seq: 0,
                wait_ms: Some(0),
            }))
            .await
            .expect("tree")
        {
            api::Response::AppsTree(t) => t,
            other => panic!("{other:?}"),
        }
    });
    assert_eq!(tree.app_id, "local.demo.preview");
    assert!(tree.tree.contains("Saved once more"), "{}", tree.tree);

    // A save that does not build says why and keeps the last good app.
    std::fs::write(dir.join("app.ts"), format!("{saved}\nlet b: = 3;\n")).expect("break");
    let shown = cli.wait_pane(&pane, "not built: app.ts:");
    assert!(shown.contains("cargo"), "{shown}");

    // A daemon with no desktop: still no image, and nothing written.
    let out = cli.on_tmux(&["app", "preview", &path, "--png", &png_path]);
    assert!(
        !out.status.success() && String::from_utf8_lossy(&out.stderr).contains(no_desktop),
        "{out:?}"
    );
    assert!(!png.exists());
}

#[test]
fn skills_install_writes_where_each_agent_looks_opencode_under_its_config_dir() {
    let root = PathBuf::from(format!("/tmp/drm-skills-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let xdg = root.join("xdg");
    std::fs::create_dir_all(&home).expect("home");
    let dream = |args: &[&str], xdg: Option<&PathBuf>| {
        let mut c = Command::new(DREAM);
        c.args(args)
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME");
        if let Some(x) = xdg {
            c.env("XDG_CONFIG_HOME", x);
        }
        let out = c.output().expect("dream");
        assert!(
            out.status.success(),
            "dream {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let names = |dir: &PathBuf| -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().join("SKILL.md").is_file())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        n.sort();
        n
    };

    // opencode alone, under $XDG_CONFIG_HOME: nothing else is written.
    dream(&["skills", "install", "--opencode"], Some(&xdg));
    let opencode = names(&xdg.join("opencode/skills"));
    assert!(
        opencode.contains(&"dreamer-config".to_owned()),
        "{opencode:?}"
    );
    // opencode loads a skill only when its name is lowercase words
    // joined by single hyphens.
    for n in &opencode {
        assert!(
            n.split('-').all(|w| !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())),
            "{n}"
        );
    }
    assert!(std::fs::read_dir(&home).expect("home").next().is_none());

    // Without an option, all three agents; opencode's under ~/.config.
    dream(&["skills", "install"], None);
    for dir in [".claude/skills", ".codex/skills", ".config/opencode/skills"] {
        assert_eq!(names(&home.join(dir)), opencode, "{dir}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
