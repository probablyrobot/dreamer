//! Remote terminal streams through the coordinator (spec 11.2, 18.2): a
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
