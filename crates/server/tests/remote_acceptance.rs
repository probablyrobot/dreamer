//! Remote acceptance through real OpenSSH (spec 18.2). Run by
//! `cargo xtask remote-acceptance`, which starts an unprivileged `sshd` on
//! a loopback port with generated keys and its own configuration, gives
//! its sessions a private HOME and daemon directories (`SetEnv`), and
//! points `DREAMER_SSH` at an `ssh` wrapper with its own config and
//! `known_hosts`. The user's SSH configuration and keys are never used.
//!
//! Ignored in an ordinary test run: it needs that setup
//! (`DREAMER_ACCEPT_DIR`, `DREAMER_ACCEPT_ARTIFACTS`).

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

fn dir(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| {
        panic!("{var} is not set: run this through `cargo xtask remote-acceptance`")
    }))
}

fn passed(case: &str) {
    println!("acceptance: {case}: passed");
}

async fn call(c: &Client, r: Request) -> Response {
    tokio::time::timeout(Duration::from_secs(60), c.call(r))
        .await
        .expect("a reply within 60 s")
        .unwrap_or_else(|e| panic!("{e}"))
}

async fn host(c: &Client) -> api::HostStatus {
    let Response::HostsList(l) = call(c, Request::HostsList(api::Empty {})).await else {
        panic!("list");
    };
    l.hosts
        .into_iter()
        .find(|h| h.name == "accept")
        .expect("the accept host")
}

async fn wait_live(c: &Client, what: &str) -> api::HostStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let h = host(c).await;
        if h.state == api::LinkState::Live as i32 {
            return h;
        }
        assert!(Instant::now() < deadline, "{what}: never live: {h:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Default)]
struct Seen {
    checkpoints: usize,
    ends: Vec<api::SubscriptionEnd>,
}

async fn pump(
    rx: &mut tokio::sync::mpsc::Receiver<Pushed>,
    replica: &mut Replica,
    seen: &mut Seen,
    what: &str,
    done: impl Fn(&Replica, &Seen) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(replica, seen) {
        assert!(
            Instant::now() < deadline,
            "never {what}:\n{}",
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

async fn subscribe(c: &Client) -> api::Subscription {
    let Response::HostsSubscribe(s) = call(
        c,
        Request::HostsSubscribe(api::HostSubscribe {
            name: "accept".into(),
            subscribe: Some(api::TerminalSubscribe {
                pane: 0,
                ..api::TerminalSubscribe::default()
            }),
        }),
    )
    .await
    else {
        panic!("subscribe");
    };
    s
}

/// End the SSH session under the coordinator: kill the remote bridge
/// (the session's command), as a dropped connection would.
fn cut_ssh(home: &std::path::Path) {
    let pattern = format!(
        "{}/.local/share/dreamer/current/bin/dream bridge",
        home.display()
    );
    let _ = Command::new("pkill").args(["-9", "-f", &pattern]).status();
}

fn far_pid(accept: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(accept.join("far/rt/dreamerd.lock"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs `cargo xtask remote-acceptance` (a private sshd)"]
async fn remote_acceptance_through_real_openssh() {
    let accept = dir("DREAMER_ACCEPT_DIR");
    let artifacts = dir("DREAMER_ACCEPT_ARTIFACTS");
    let home = accept.join("home");
    let far = TmuxServer::start(100, 30, "/bin/sh").expect("remote tmux");
    far.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
        .expect("shell");
    std::fs::create_dir_all(accept.join("config")).expect("config");
    std::fs::write(
        accept.join("config/hosts.yaml"),
        format!(
            "hosts:\n  - {{name: accept, destination: accept, tmux: '{}', tmux_socket: '{}', reconnect: auto}}\n",
            tmux_program().display(),
            far.socket()
        ),
    )
    .expect("hosts.yaml");
    let mut daemon = Command::new(EXE)
        .args([
            "--background",
            "--tmux-socket",
            "dreamer-accept-local-unused",
        ])
        .env("DREAMER_RUNTIME_DIR", accept.join("rt"))
        .env("DREAMER_STATE_DIR", accept.join("st"))
        .env("DREAMER_CONFIG_DIR", accept.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .env("DREAMER_SSH", accept.join("ssh"))
        .env("DREAMER_TEST_BACKOFF_MS", "200")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dreamerd");
    let mut c = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&accept.join("rt/dreamerd.sock"), "acceptance").await {
                Ok(c) => break c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("{e}"),
            }
        }
    };
    let mut rx = c.pushed().expect("pushed");

    // The guard: the SSH session's home must be the private one, or
    // nothing is installed anywhere.
    let Response::HostsInstallPlan(plan) = call(
        &c,
        Request::HostsInstallPlan(api::InstallPlanParams {
            name: "accept".into(),
            artifacts: Some(artifacts.display().to_string()),
        }),
    )
    .await
    else {
        panic!("plan");
    };
    assert_eq!(
        PathBuf::from(&plan.home),
        home,
        "refusing: the SSH session's HOME is not the private one"
    );
    println!(
        "acceptance: host is {} {} ({})",
        plan.remote_os, plan.remote_arch, plan.target
    );
    passed("probe and reviewable plan through real ssh");

    let Response::HostsInstall(r) = call(
        &c,
        Request::HostsInstall(api::InstallParams {
            name: "accept".into(),
            artifacts: Some(artifacts.display().to_string()),
            plan_id: plan.plan_id.clone(),
        }),
    )
    .await
    else {
        panic!("install");
    };
    println!("acceptance: installed {} ({})", r.version, r.check);
    passed("install the server archive (checksum on the host, atomic switch)");

    let _ = call(
        &c,
        Request::HostsReconnect(api::HostRef {
            name: "accept".into(),
        }),
    )
    .await;
    let h = wait_live(&c, "after install").await;
    let instance = h.remote_instance.clone();
    passed("connect through system ssh: no PTY, no listener, the installed server");

    // Follow a pane; cut SSH; resume from the journal.
    let sub = subscribe(&c).await;
    let mut replica = Replica::new(0);
    replica.begin(&sub).expect("begin");
    let mut seen = Seen::default();
    let type_far = |line: &str| {
        far.run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        far.run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    };
    type_far("echo one-$((1+1))");
    pump(&mut rx, &mut replica, &mut seen, "one-2", |r, _| {
        r.text().contains("\none-2")
    })
    .await;
    cut_ssh(&home);
    type_far("echo two-$((2+2))");
    pump(
        &mut rx,
        &mut replica,
        &mut seen,
        "two-4 after the cut",
        |r, _| r.text().contains("\ntwo-4"),
    )
    .await;
    assert_eq!(seen.checkpoints, 1, "a resume sent a new checkpoint");
    assert!(seen.ends.is_empty(), "{:?}", seen.ends);
    assert_eq!(
        wait_live(&c, "after the cut").await.remote_instance,
        instance
    );
    passed("SSH interruption resumes from the journal without recapturing tmux");

    // Input sent while the link is down fails and is never replayed.
    cut_ssh(&home);
    let ghost = c
        .call(Request::HostsCall(api::HostCall {
            name: "accept".into(),
            method: "terminal.input".into(),
            payload: Request::TerminalInput(api::TerminalInput {
                pane: 0,
                event: Some(api::terminal_input::Event::Text(api::TextInput {
                    text: "echo GHOST\r".into(),
                })),
                ..api::TerminalInput::default()
            })
            .encode(),
        }))
        .await;
    wait_live(&c, "after the second cut").await;
    type_far("echo three-$((3+3))");
    pump(&mut rx, &mut replica, &mut seen, "three-6", |r, _| {
        r.text().contains("\nthree-6")
    })
    .await;
    if ghost.is_err() {
        assert!(
            !replica.text().contains("GHOST"),
            "input from a failed call was replayed"
        );
        passed("no automatic replay of uncertain input");
    } else {
        println!(
            "acceptance: no automatic replay of uncertain input: not run (the call went through before the cut took effect)"
        );
    }

    // A large transfer does not starve interactive calls: a new view's
    // checkpoint of a pane holding megabytes of history, sent as chunked
    // bulk data, while small calls go back and forth.
    type_far(
        "awk 'BEGIN{for(i=0;i<12000;i++){s=\"\";for(j=0;j<99;j++)s=s\"\\303\\251\";print s}; print \"DONE-BIG\"}'",
    );
    pump(&mut rx, &mut replica, &mut seen, "DONE-BIG", |r, _| {
        r.text().contains("\nDONE-BIG")
    })
    .await;
    let big = subscribe(&c).await;
    let mut viewer = Replica::new(0);
    viewer.begin(&big).expect("begin");
    let small = async {
        let mut worst = Duration::ZERO;
        for _ in 0..20 {
            let t = Instant::now();
            let _ = call(
                &c,
                Request::HostsCall(api::HostCall {
                    name: "accept".into(),
                    method: "daemon.version".into(),
                    payload: Request::DaemonVersion(api::Empty {}).encode(),
                }),
            )
            .await;
            worst = worst.max(t.elapsed());
        }
        worst
    };
    let worst = small.await;
    let mut seen_big = Seen::default();
    pump(
        &mut rx,
        &mut viewer,
        &mut seen_big,
        "the big checkpoint",
        |_, s| s.checkpoints > 0,
    )
    .await;
    println!(
        "acceptance: a {}-byte checkpoint alongside 20 small calls: slowest small call {} ms",
        big.checkpoint_bytes,
        worst.as_millis()
    );
    assert!(worst < Duration::from_secs(2), "interactive calls starved");
    passed("a large transfer does not starve interactive calls");

    // A restart of the host's daemon is told apart from a lost link.
    let before = seen_big.ends.len();
    if let Some(pid) = far_pid(&accept) {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    pump(
        &mut rx,
        &mut viewer,
        &mut seen_big,
        "the relay ended",
        |_, s| s.ends.len() > before,
    )
    .await;
    assert_eq!(
        seen_big.ends.last().map(|e| e.reason),
        Some(api::EndReason::HostRestarted as i32)
    );
    let h = wait_live(&c, "after the restart").await;
    assert_ne!(h.remote_instance, instance);
    passed("a host daemon restart is distinguished from a lost link");

    let _ = call(&c, Request::DaemonShutdown(api::Empty {})).await;
    if let Some(pid) = far_pid(&accept) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}
