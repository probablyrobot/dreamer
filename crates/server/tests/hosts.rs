//! Remote terminal streams through the coordinator: a
//! pane on another host followed through the local daemon, SSH cut under
//! it, and the subscription resumed from the host's journal with no new
//! checkpoint; a host daemon restart told apart from a lost link.
//!
//! The "other host" is a second daemon with its own tmux server and private
//! directories, reached only through a stand-in `ssh` that runs the remote
//! command in a shell, as sshd does. Needs `dream` built next to `dreamerd`
//! (a workspace build or test run does that).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message as _;
use replica::Replica;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Two {
    root: PathBuf,
    near: TmuxServer,
    far: TmuxServer,
    daemon: std::process::Child,
}

impl Two {
    fn new() -> Two {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-hosts-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        let near = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
        far.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let dream = PathBuf::from(EXE).with_file_name("dream");
        assert!(
            dream.exists(),
            "{} is missing; build it with `cargo build -p dreamer-cli`",
            dream.display()
        );
        let ssh = root.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
while [ "$1" != -- ]; do shift; done
echo $$ > '{pid}'
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
exec /bin/sh -c "exec $3"
"#,
                pid = root.join("bridge-pid").display(),
                rt = root.join("far/rt").display(),
                st = root.join("far/st").display(),
                daemon = EXE,
            ),
        )
        .expect("stand-in ssh");
        std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", near.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_SSH", &ssh)
            .env("DREAMER_TEST_BACKOFF_MS", "100")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Two {
            root,
            near,
            far,
            daemon,
        }
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.root.join("rt/dreamerd.sock"), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn far_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.root.join("far/rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn kill(pid: &str) {
        let _ = Command::new("kill").args(["-9", pid.trim()]).status();
    }

    fn cut_ssh(&self) {
        Two::kill(&std::fs::read_to_string(self.root.join("bridge-pid")).expect("bridge pid"));
    }

    fn type_far(&self, line: &str) {
        self.far
            .run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        self.far
            .run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    }
}

impl Drop for Two {
    fn drop(&mut self) {
        for lock in ["rt/dreamerd.lock", "far/rt/dreamerd.lock"] {
            if let Some(pid) = std::fs::read_to_string(self.root.join(lock))
                .ok()
                .filter(|p| p.trim().parse().is_ok_and(platform::process_alive))
            {
                Two::kill(&pid);
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = &self.near;
        if std::env::var_os("DREAMER_TEST_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// What the local connection received for a relayed subscription.
#[derive(Default)]
struct Seen {
    checkpoints: usize,
    ends: Vec<api::SubscriptionEnd>,
}

/// Apply pushed items to the replica until `done` holds.
async fn pump(
    rx: &mut tokio::sync::mpsc::Receiver<Pushed>,
    replica: &mut Replica,
    seen: &mut Seen,
    what: &str,
    done: impl Fn(&Replica, &Seen) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(replica, seen) {
        assert!(
            Instant::now() < deadline,
            "never {what}; replica shows:\n{}",
            replica.text()
        );
        let Ok(Some(p)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await else {
            continue;
        };
        match p {
            Pushed::Bulk { topic, data, .. } if topic == "terminal.checkpoint" => {
                seen.checkpoints += 1;
                let d = api::CheckpointData::decode(data.as_slice()).expect("checkpoint");
                replica.checkpoint(&d).expect("restore");
            }
            Pushed::Event { topic, payload, .. } if topic == "terminal.records" => {
                let b = api::RecordBatch::decode(payload.as_slice()).expect("records");
                replica.records(&b).expect("apply");
            }
            Pushed::Event { topic, payload, .. } if topic == "terminal.end" => {
                seen.ends
                    .push(api::SubscriptionEnd::decode(payload.as_slice()).expect("end"));
            }
            Pushed::Closed { error } => panic!("local connection closed: {error:?}"),
            _ => {}
        }
    }
}

async fn host_state(c: &Client) -> api::HostStatus {
    let Response::HostsList(l) = c
        .call(Request::HostsList(api::Empty {}))
        .await
        .expect("list")
    else {
        panic!("list");
    };
    l.hosts.into_iter().next().expect("a host")
}

/// The remote mirror's text, read through the coordinator.
async fn far_text(c: &Client) -> String {
    let Response::HostsCall(r) = c
        .call(Request::HostsCall(api::HostCall {
            name: "devbox".into(),
            method: "terminal.read".into(),
            payload: Request::TerminalRead(api::ReadParams {
                pane: 0,
                history: Some(0),
                endpoint: None,
            })
            .encode(),
        }))
        .await
        .expect("read")
    else {
        panic!("read");
    };
    let Response::TerminalRead(t) = Response::decode("terminal.read", &r.payload).expect("decode")
    else {
        panic!("read");
    };
    t.lines.join("\n")
}

fn normalized(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|l| l.trim_end().to_owned()).collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relayed_pane_resumes_from_the_journal_after_ssh_is_cut() {
    let two = Two::new();
    let mut c = two.client().await;
    let mut rx = c.pushed().expect("pushed");
    let Response::HostsConnect(s) = c
        .call(Request::HostsConnect(api::HostConnect {
            name: "devbox".into(),
            destination: "devbox".into(),
            dream: Some(
                PathBuf::from(EXE)
                    .with_file_name("dream")
                    .display()
                    .to_string(),
            ),
            tmux: Some(tmux_program().display().to_string()),
            tmux_socket: Some(two.far.socket().into()),
            tmux_socket_path: None,
            auto_reconnect: Some(true),
        }))
        .await
        .expect("connect")
    else {
        panic!("connect");
    };
    assert_eq!(s.state, api::LinkState::Live as i32);
    let far_pid = two.far_pid().expect("remote daemon");

    let Response::HostsSubscribe(sub) = c
        .call(Request::HostsSubscribe(api::HostSubscribe {
            name: "devbox".into(),
            subscribe: Some(api::TerminalSubscribe {
                pane: 0,
                ..api::TerminalSubscribe::default()
            }),
        }))
        .await
        .expect("subscribe")
    else {
        panic!("subscribe");
    };
    assert_eq!(sub.mode, api::SubscribeMode::Checkpoint as i32);
    let mut replica = Replica::new(0);
    replica.begin(&sub).expect("begin");
    let mut seen = Seen::default();

    two.type_far("echo one-$((1+1))");
    pump(&mut rx, &mut replica, &mut seen, "showed one-2", |r, _| {
        r.text().contains("\none-2")
    })
    .await;
    assert_eq!(seen.checkpoints, 1);

    // Cut SSH. While the link is down the remote pane keeps running and its
    // mirror keeps journaling.
    two.cut_ssh();
    two.type_far("echo two-$((2+2))");
    pump(
        &mut rx,
        &mut replica,
        &mut seen,
        "resumed with two-4",
        |r, _| r.text().contains("\ntwo-4"),
    )
    .await;
    assert_eq!(seen.checkpoints, 1, "the resume sent a new checkpoint");
    assert!(
        seen.ends.is_empty(),
        "the subscription ended: {:?}",
        seen.ends
    );
    assert_eq!(two.far_pid(), Some(far_pid), "the remote daemon restarted");
    let status = host_state(&c).await;
    assert_eq!(status.state, api::LinkState::Live as i32, "{status:?}");

    // The replica, built from one checkpoint and the journal alone, matches
    // the remote mirror.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let far = normalized(&far_text(&c).await);
        let near = normalized(&replica.text());
        if !far.is_empty() && near.ends_with(&far) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replica\n{near:#?}\nremote mirror\n{far:#?}"
        );
        pump(&mut rx, &mut replica, &mut seen, "settled", |_, _| true).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A restart of the host's daemon is not a lost link: the journal is
    // gone, and the subscription ends saying so.
    Two::kill(&far_pid.to_string());
    pump(&mut rx, &mut replica, &mut seen, "ended", |_, s| {
        !s.ends.is_empty()
    })
    .await;
    assert_eq!(
        seen.ends[0].reason,
        api::EndReason::HostRestarted as i32,
        "{:?}",
        seen.ends
    );
    assert_eq!(seen.ends[0].subscription, sub.id);
    assert_ne!(two.far_pid(), Some(far_pid), "a new remote daemon runs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_topology_follows_changes_across_a_reconnect() {
    let two = Two::new();
    let mut c = two.client().await;
    let mut rx = c.pushed().expect("pushed");
    c.call(Request::HostsConnect(api::HostConnect {
        name: "devbox".into(),
        destination: "devbox".into(),
        dream: Some(
            PathBuf::from(EXE)
                .with_file_name("dream")
                .display()
                .to_string(),
        ),
        tmux: Some(tmux_program().display().to_string()),
        tmux_socket: Some(two.far.socket().into()),
        tmux_socket_path: None,
        auto_reconnect: Some(true),
    }))
    .await
    .expect("connect");
    let Response::HostsWatch(first) = c
        .call(Request::HostsWatch(api::HostRef {
            name: "devbox".into(),
        }))
        .await
        .expect("watch")
    else {
        panic!("watch");
    };
    let names = |u: &api::TopologyUpdate| -> Vec<String> {
        u.topology
            .as_ref()
            .map(|t| t.windows.iter().map(|w| w.name.clone()).collect())
            .unwrap_or_default()
    };
    let next_with = async |rx: &mut tokio::sync::mpsc::Receiver<Pushed>, after: u64, name: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "no topology with window {name}");
            let Ok(Some(Pushed::Event { topic, payload, .. })) =
                tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
            else {
                continue;
            };
            if topic != "tmux.topology" {
                continue;
            }
            let u = api::TopologyUpdate::decode(payload.as_slice()).expect("update");
            assert!(u.revision > after, "revisions must increase");
            if names(&u).iter().any(|n| n == name) {
                return u;
            }
        }
    };
    two.far
        .run_checked(&["new-window", "-d", "-n", "alpha", "-t", "main"])
        .expect("window");
    let a = next_with(&mut rx, first.revision, "alpha").await;

    two.cut_ssh();
    two.far
        .run_checked(&["new-window", "-d", "-n", "beta", "-t", "main"])
        .expect("window");
    let b = next_with(&mut rx, a.revision, "beta").await;
    assert!(b.revision > a.revision);
}

/// Make the stand-in host a Linux x86-64 machine with its own home, with
/// version `old` installed, and server archives for this version in
/// `artifacts`. Every installed `dream` runs the real one.
fn fake_host_with(two: &Two, old: &str) -> PathBuf {
    fake_host_env(two, old, "")
}

/// Like `fake_host_with`, with `env` (shell lines) in the host's SSH
/// sessions.
fn fake_host_env(two: &Two, old: &str, env: &str) -> PathBuf {
    let root = &two.root;
    let home = root.join("far/home");
    let fakebin = root.join("far/fakebin");
    std::fs::create_dir_all(&fakebin).expect("dirs");
    let write_exe = |path: &std::path::Path, body: &str| {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, body).expect("write");
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
    };
    write_exe(
        &fakebin.join("uname"),
        "#!/bin/sh\ncase \"$1\" in -s) echo Linux;; -m) echo x86_64;; esac\n",
    );
    let dream = PathBuf::from(EXE).with_file_name("dream");
    let wrapper = format!("#!/bin/sh\nexec '{}' \"$@\"\n", dream.display());
    let base = home.join(".local/share/dreamer");
    write_exe(&base.join(format!("versions/{old}/bin/dream")), &wrapper);
    std::fs::write(
        base.join(format!("versions/{old}/VERSION")),
        format!("version {old}\n"),
    )
    .expect("VERSION");
    let _ = std::fs::remove_file(base.join("current"));
    std::os::unix::fs::symlink(format!("versions/{old}"), base.join("current")).expect("link");
    // The stand-in ssh, now with the host's home and uname.
    write_exe(
        &root.join("ssh"),
        &format!(
            r#"#!/bin/sh
while [ "$1" != -- ]; do shift; done
echo $$ > '{pid}'
export HOME='{home}' PATH='{fakebin}':"$PATH"
export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'
export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'
{env}
exec /bin/sh -c "exec $3"
"#,
            pid = root.join("bridge-pid").display(),
            home = home.display(),
            fakebin = fakebin.display(),
            rt = root.join("far/rt").display(),
            st = root.join("far/st").display(),
            daemon = EXE,
        ),
    );
    // This version's archive for the host.
    let version = env!("CARGO_PKG_VERSION");
    let artifacts = root.join("artifacts");
    let name = format!("dreamer-server-{version}-x86_64-unknown-linux-gnu");
    let stage = artifacts.join("stage").join(&name);
    write_exe(&stage.join("bin/dream"), &wrapper);
    std::fs::write(stage.join("VERSION"), format!("version {version}\n")).expect("VERSION");
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
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(&tarball)
        .output()
        .or_else(|_| Command::new("sha256sum").arg(&tarball).output())
        .expect("sha256");
    let sum = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("sum")
        .to_owned();
    std::fs::write(
        artifacts.join(format!("{name}.tar.gz.sha256")),
        format!("{sum}  {name}.tar.gz\n"),
    )
    .expect("sha file");
    artifacts
}

async fn plan_id(c: &Client, artifacts: &std::path::Path) -> String {
    let Response::HostsInstallPlan(p) = c
        .call(Request::HostsInstallPlan(api::InstallPlanParams {
            name: "devbox".into(),
            artifacts: Some(artifacts.display().to_string()),
        }))
        .await
        .expect("plan")
    else {
        panic!("plan");
    };
    assert_eq!(p.target, "x86_64-unknown-linux-gnu");
    p.plan_id
}

async fn upgrade(
    c: &Client,
    artifacts: &std::path::Path,
    allow_older: bool,
) -> Result<api::UpgradeResult, client::ClientError> {
    let plan_id = plan_id(c, artifacts).await;
    match c
        .call(Request::HostsUpgrade(api::UpgradeParams {
            name: "devbox".into(),
            artifacts: Some(artifacts.display().to_string()),
            plan_id,
            allow_older,
        }))
        .await?
    {
        Response::HostsUpgrade(r) => Ok(r),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_restarts_the_hosts_daemon_and_views_recover_from_a_checkpoint() {
    let two = Two::new();
    let artifacts = fake_host_with(&two, "0.0.9");
    let mut c = two.client().await;
    let mut rx = c.pushed().expect("pushed");
    let Response::HostsConnect(s) = c
        .call(Request::HostsConnect(api::HostConnect {
            name: "devbox".into(),
            destination: "devbox".into(),
            // No dream: the installed one is used.
            dream: None,
            tmux: Some(tmux_program().display().to_string()),
            tmux_socket: Some(two.far.socket().into()),
            tmux_socket_path: None,
            auto_reconnect: Some(true),
        }))
        .await
        .expect("connect")
    else {
        panic!("connect");
    };
    let pid_before = s.remote_pid.expect("remote pid");

    let Response::HostsSubscribe(sub) = c
        .call(Request::HostsSubscribe(api::HostSubscribe {
            name: "devbox".into(),
            subscribe: Some(api::TerminalSubscribe {
                pane: 0,
                ..api::TerminalSubscribe::default()
            }),
        }))
        .await
        .expect("subscribe")
    else {
        panic!("subscribe");
    };
    let mut replica = Replica::new(0);
    replica.begin(&sub).expect("begin");
    let mut seen = Seen::default();
    two.type_far("echo before-$((40+2))");
    pump(
        &mut rx,
        &mut replica,
        &mut seen,
        "showed before-42",
        |r, _| r.text().contains("\nbefore-42"),
    )
    .await;

    let r = upgrade(&c, &artifacts, false).await.expect("upgrade");
    assert!(r.restarted, "{r:?}");
    assert_eq!(r.from.as_deref(), Some("0.0.9"));
    assert_eq!(r.to, env!("CARGO_PKG_VERSION"));
    assert!(r.note.contains("restarted"), "{}", r.note);
    assert_eq!(r.pid_before, Some(pid_before));
    assert!(r.pid_after.is_some_and(|p| p != pid_before), "{r:?}");

    // The view's relay says the daemon restarted; the pane kept running.
    pump(
        &mut rx,
        &mut replica,
        &mut seen,
        "the relay ended",
        |_, s| !s.ends.is_empty(),
    )
    .await;
    assert_eq!(seen.ends[0].reason, api::EndReason::HostRestarted as i32);
    two.far
        .run_checked(&["has-session", "-t", "main"])
        .expect("the remote tmux kept running");
    // Following again starts from a fresh checkpoint that still shows it.
    let Response::HostsSubscribe(again) = c
        .call(Request::HostsSubscribe(api::HostSubscribe {
            name: "devbox".into(),
            subscribe: Some(api::TerminalSubscribe {
                pane: 0,
                ..api::TerminalSubscribe::default()
            }),
        }))
        .await
        .expect("subscribe again")
    else {
        panic!("subscribe");
    };
    assert_eq!(again.mode, api::SubscribeMode::Checkpoint as i32);
    let mut fresh = Replica::new(0);
    fresh.begin(&again).expect("begin");
    let before = seen.checkpoints;
    pump(
        &mut rx,
        &mut fresh,
        &mut seen,
        "a fresh checkpoint",
        |r, s| s.checkpoints > before && r.text().contains("\nbefore-42"),
    )
    .await;

    // The same version again: nothing happens.
    let r = upgrade(&c, &artifacts, false).await.expect("no-op");
    assert!(!r.restarted, "{r:?}");

    // Going back to an older version needs saying so.
    std::fs::write(
        two.root.join(format!(
            "far/home/.local/share/dreamer/versions/{}/VERSION",
            env!("CARGO_PKG_VERSION")
        )),
        "version 99.0.0\n",
    )
    .expect("pretend newer");
    let e = upgrade(&c, &artifacts, false)
        .await
        .expect_err("older refused");
    assert!(e.to_string().contains("newer"), "{e}");
    let r = upgrade(&c, &artifacts, true).await.expect("allowed");
    assert!(r.restarted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incompatible_host_is_refused_in_the_handshake_and_an_upgrade_fixes_it() {
    let two = Two::new();
    // The host's daemon is another build.
    let artifacts = fake_host_env(
        &two,
        "0.0.9",
        "export DREAMER_TEST_BUILD_FAMILY=another-build",
    );
    let c = two.client().await;
    let connect = api::HostConnect {
        name: "devbox".into(),
        destination: "devbox".into(),
        dream: None,
        tmux: Some(tmux_program().display().to_string()),
        tmux_socket: Some(two.far.socket().into()),
        tmux_socket_path: None,
        auto_reconnect: Some(true),
    };
    let e = c
        .call(Request::HostsConnect(connect.clone()))
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("build family another-build"), "{e}");
    let s = host_state(&c).await;
    assert_eq!(s.failure, api::HostFailure::Incompatible as i32, "{s:?}");
    assert_eq!(s.next_retry_unix_ms, None, "never retried by itself");
    // Refused before Welcome: nothing of the host's was ever taken in.
    assert_eq!(s.remote_pid, None);
    assert!(s.remote.is_none());
    let detail = s.detail.unwrap_or_default();
    assert!(detail.contains("dream hosts upgrade devbox"), "{detail}");
    let old = two.far_pid().expect("the other build's daemon");

    // A matching build installed over it: the old daemon is stopped by its
    // pid, and the host connects.
    let artifacts2 = fake_host_env(&two, "0.0.9", "");
    assert_eq!(artifacts, artifacts2);
    let r = upgrade(&c, &artifacts, false).await.expect("upgrade");
    assert!(r.restarted, "{r:?}");
    let s = host_state(&c).await;
    assert_eq!(s.state, api::LinkState::Live as i32, "{s:?}");
    assert_ne!(s.remote_pid, Some(old));
    assert_eq!(
        s.remote.map(|v| v.build_family),
        Some(wire::Identity::current("test").build_family)
    );
}

async fn connect_devbox(two: &Two, c: &Client) -> api::HostStatus {
    let Response::HostsConnect(s) = c
        .call(Request::HostsConnect(api::HostConnect {
            name: "devbox".into(),
            destination: "devbox".into(),
            dream: Some(
                PathBuf::from(EXE)
                    .with_file_name("dream")
                    .display()
                    .to_string(),
            ),
            tmux: Some(tmux_program().display().to_string()),
            tmux_socket: Some(two.far.socket().into()),
            tmux_socket_path: None,
            auto_reconnect: Some(true),
        }))
        .await
        .expect("connect")
    else {
        panic!("connect");
    };
    s
}

async fn subscribe_devbox(c: &Client) -> api::Subscription {
    let Response::HostsSubscribe(s) = c
        .call(Request::HostsSubscribe(api::HostSubscribe {
            name: "devbox".into(),
            subscribe: Some(api::TerminalSubscribe {
                pane: 0,
                ..api::TerminalSubscribe::default()
            }),
        }))
        .await
        .expect("subscribe")
    else {
        panic!("subscribe");
    };
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_link_outlives_every_local_client_and_ssh_cuts_find_the_same_daemon() {
    let two = Two::new();
    // A client (the GUI) connects the host and follows a pane, then goes
    // away entirely.
    let first = {
        let mut c = two.client().await;
        let mut rx = c.pushed().expect("pushed");
        let s = connect_devbox(&two, &c).await;
        let sub = subscribe_devbox(&c).await;
        let mut replica = Replica::new(0);
        replica.begin(&sub).expect("begin");
        let mut seen = Seen::default();
        two.type_far("echo first-$((1+1))");
        pump(&mut rx, &mut replica, &mut seen, "first-2", |r, _| {
            r.text().contains("\nfirst-2")
        })
        .await;
        (s.remote_instance.clone(), sub.epoch)
    };
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The pane keeps working with no client at all.
    two.type_far("echo unseen-$((2+3))");

    // A new client finds the host still live on the same link, and the
    // remote mirror still in its first epoch: nothing was recaptured.
    let mut c = two.client().await;
    let mut rx = c.pushed().expect("pushed");
    let s = host_state(&c).await;
    assert_eq!(s.state, api::LinkState::Live as i32, "{s:?}");
    assert_eq!(s.remote_instance, first.0);
    let sub = subscribe_devbox(&c).await;
    assert_eq!(sub.epoch, first.1, "the remote mirror was recaptured");
    let mut replica = Replica::new(0);
    replica.begin(&sub).expect("begin");
    let mut seen = Seen::default();
    pump(&mut rx, &mut replica, &mut seen, "unseen-5", |r, _| {
        r.text().contains("\nunseen-5")
    })
    .await;

    // SSH cut. Output typed after it can only arrive over a new link, and
    // that link reaches the very same remote daemon.
    two.cut_ssh();
    two.type_far("echo after-$((3+4))");
    pump(
        &mut rx,
        &mut replica,
        &mut seen,
        "after-7 through the resumed relay",
        |r, _| r.text().contains("\nafter-7"),
    )
    .await;
    let s = host_state(&c).await;
    assert_eq!(s.state, api::LinkState::Live as i32, "{s:?}");
    assert_eq!(s.remote_instance, first.0, "a different remote daemon");
    assert!(seen.ends.is_empty(), "{:?}", seen.ends);
}

/// A web server on the "host's" loopback: every request gets `body`.
async fn serve(listener: tokio::net::TcpListener, body: std::sync::Arc<Vec<u8>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    while let Ok((mut s, _)) = listener.accept().await {
        let body = body.clone();
        tokio::spawn(async move {
            let (mut req, mut b) = (Vec::new(), [0u8; 1024]);
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => req.extend_from_slice(&b[..n]),
                }
            }
            let head = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(&body).await;
            let _ = s.shutdown().await;
        });
    }
}

/// `GET /` from 127.0.0.1:`port`: the response's body.
async fn get(port: u32) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port as u16)).await?;
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut all = Vec::new();
    s.read_to_end(&mut all).await?;
    let at = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no response head"))?;
    Ok(all.split_off(at + 4))
}

async fn forward(c: &Client, port: u16) -> Result<api::HostForwarded, client::ClientError> {
    match c
        .call(Request::HostsForward(api::HostForward {
            name: "devbox".into(),
            port: u32::from(port),
        }))
        .await?
    {
        Response::HostsForward(f) => Ok(f),
        _ => panic!("forward"),
    }
}

/// Whether anything accepts on 127.0.0.1:`port`.
async fn accepting(port: u32) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port as u16))
        .await
        .is_ok()
}

/// An address of this machine that is not the loopback, when it has one.
fn outside_address() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forward_reaches_the_hosts_loopback_and_closes_with_its_last_hold() {
    let two = Two::new();
    let mut c = two.client().await;
    let mut rx = c.pushed().expect("pushed");
    assert_eq!(
        connect_devbox(&two, &c).await.state,
        api::LinkState::Live as i32
    );
    // The "host" here is this machine: its server listens on the loopback
    // only, and the forward reaches it through the host's daemon.
    let server = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("server");
    let port = server.local_addr().expect("addr").port();
    let mut body = Vec::with_capacity(8 << 20);
    let mut x = 0x2545_f491_u32;
    while body.len() < 8 << 20 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        body.extend_from_slice(&x.to_le_bytes());
    }
    let body = std::sync::Arc::new(body);
    tokio::spawn(serve(server, body.clone()));

    // A connection with an agent's reach to hosts, but not web.forward.
    let agent = two.client().await;
    let _ = agent
        .call(Request::GrantsNarrow(api::Grant {
            methods: vec!["hosts.*".into(), "web.open".into()],
            all_panes: true,
            ..api::Grant::default()
        }))
        .await
        .expect("narrow");
    let e = forward(&agent, port).await.expect_err("no web.forward");
    assert!(e.to_string().contains("hosts.forward"), "{e}");
    let open = Request::HostsCall(api::HostCall {
        name: "devbox".into(),
        method: "web.open".into(),
        payload: Request::WebOpen(api::WebOpen {
            url: format!("localhost:{port}"),
            ..api::WebOpen::default()
        })
        .encode(),
    });
    let e = agent.call(open).await.expect_err("a page on the loopback");
    assert!(e.to_string().contains("web.forward"), "{e}");

    // The owner's forward: a loopback port of this machine, and only that.
    let f = forward(&c, port).await.expect("forward");
    assert_ne!(f.local_port, u32::from(port));
    match outside_address() {
        Some(ip) => assert!(
            tokio::net::TcpStream::connect((ip, f.local_port as u16))
                .await
                .is_err(),
            "the forward answers on {ip}"
        ),
        None => println!("no address besides the loopback: outside reach not checked"),
    }

    // Four 8 MiB pages at once, byte for byte, while the host's terminal
    // keeps showing what it prints.
    let sub = subscribe_devbox(&c).await;
    let mut replica = Replica::new(0);
    replica.begin(&sub).expect("begin");
    let mut seen = Seen::default();
    two.type_far("echo ready-$((1+1))");
    pump(&mut rx, &mut replica, &mut seen, "ready-2", |r, _| {
        r.text().contains("\nready-2")
    })
    .await;
    let started = Instant::now();
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let downloads: Vec<_> = (0..4)
        .map(|_| {
            let (body, busy, port) = (body.clone(), busy.clone(), f.local_port);
            tokio::spawn(async move {
                let mut bytes = 0;
                while busy.load(std::sync::atomic::Ordering::SeqCst) || bytes == 0 {
                    let got = get(port).await.expect("page");
                    assert!(got == *body, "a page arrived changed ({} bytes)", got.len());
                    bytes += got.len();
                }
                bytes
            })
        })
        .collect();
    let mut worst = Duration::ZERO;
    for i in 0..10 {
        let t = Instant::now();
        two.type_far(&format!("echo mark-$(({i}+100))"));
        let want = format!("\nmark-{}", i + 100);
        pump(&mut rx, &mut replica, &mut seen, &want, |r, _| {
            r.text().contains(&want)
        })
        .await;
        worst = worst.max(t.elapsed());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    busy.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut bytes = 0;
    for d in downloads {
        bytes += d.await.expect("downloads");
    }
    println!(
        "forward: {bytes} bytes in {} ms; slowest terminal echo meanwhile {} ms",
        started.elapsed().as_millis(),
        worst.as_millis()
    );
    assert!(worst < Duration::from_secs(2), "terminal output waited");

    // Holds of one port share its listener, which closes with the last.
    let again = forward(&c, port).await.expect("second hold");
    assert_eq!(again.local_port, f.local_port);
    let unforward = |id| Request::HostsUnforward(api::HostForwardRef { forward: id });
    c.call(unforward(f.forward)).await.expect("release one");
    assert_eq!(
        get(f.local_port).await.expect("still held").len(),
        body.len()
    );
    c.call(unforward(again.forward))
        .await
        .expect("release last");
    assert!(
        !accepting(f.local_port).await,
        "the listener outlived its holds"
    );
    assert!(c.call(unforward(again.forward)).await.is_err());

    // A connection's holds go with it.
    let other = two.client().await;
    let held = forward(&other, port).await.expect("forward");
    assert!(accepting(held.local_port).await);
    drop(other);
    let deadline = Instant::now() + Duration::from_secs(5);
    while accepting(held.local_port).await {
        assert!(
            Instant::now() < deadline,
            "a closed connection's forward stayed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
