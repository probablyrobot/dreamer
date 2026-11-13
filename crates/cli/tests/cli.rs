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
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_DAEMON", Self::daemon())
            .env("DREAMER_SSH", self.root.join("ssh"))
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env("DREAMER_TEST_BACKOFF_MS", "100");
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
    let count = lines_in(&r("attempts"));
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lines_in(&r("attempts")), count, "retried after disconnect");
    assert_eq!(host_json(&cli)["state"], "closed");
}

impl Cli {
    /// `dream <args>` with `input` on standard input.
    fn run_with_input(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write as _;
        let mut c = Command::new(DREAM);
        c.args(args)
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
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
