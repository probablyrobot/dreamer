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

/// A web server on the host's loopback: every request gets `body`.
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
            let head = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(&body).await;
            let _ = s.shutdown().await;
        });
    }
}

/// `GET /` from 127.0.0.1:`port`, as a page would: the response's body.
async fn get(port: u32) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port as u16))
        .await
        .expect("the forward's port");
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .expect("request");
    let mut all = Vec::new();
    s.read_to_end(&mut all).await.expect("response");
    let at = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    all.split_off(at + 4)
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
    let ghost = tokio::time::timeout(
        Duration::from_secs(30),
        c.call(Request::HostsCall(api::HostCall {
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
        })),
    )
    .await
    .expect("a call while the link is down returns within 30 s");
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

    // A web pane's page on the host's loopback (a server listening only
    // there), reached from this machine's loopback through the SSH
    // connection the coordinator holds, while the host's terminal keeps
    // printing on time.
    let server = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("server");
    let port = server.local_addr().expect("addr").port();
    let mut page =
        b"<!doctype html><title>far loopback</title><p>served on the host's loopback\n".to_vec();
    let mut x = 0x2545_f491_u32;
    while page.len() < 4 << 20 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        page.push(b'a' + (x % 26) as u8);
    }
    let page = std::sync::Arc::new(page);
    tokio::spawn(serve(server, page.clone()));
    let Response::HostsForward(f) = call(
        &c,
        Request::HostsForward(api::HostForward {
            name: "accept".into(),
            port: u32::from(port),
        }),
    )
    .await
    else {
        panic!("forward");
    };
    assert_ne!(f.local_port, u32::from(port));
    let t = Instant::now();
    // Pages are fetched, and checked byte for byte, until the echoes are
    // done.
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let pages = {
        let (busy, page) = (busy.clone(), page.clone());
        tokio::spawn(async move {
            let mut n = 0;
            while busy.load(std::sync::atomic::Ordering::SeqCst) || n == 0 {
                let got = get(f.local_port).await;
                assert!(got == *page, "the page arrived changed");
                n += got.len();
            }
            n
        })
    };
    let mut worst = Duration::ZERO;
    for i in 0..5 {
        let at = Instant::now();
        type_far(&format!("echo fwd-$(({i}+40))"));
        let want = format!("\nfwd-{}", i + 40);
        pump(&mut rx, &mut replica, &mut seen, &want, |r, _| {
            r.text().contains(&want)
        })
        .await;
        worst = worst.max(at.elapsed());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    busy.store(false, std::sync::atomic::Ordering::SeqCst);
    let bytes = pages.await.expect("pages");
    println!(
        "acceptance: {bytes} bytes of a page on the host's loopback in {} ms; slowest terminal echo meanwhile {} ms",
        t.elapsed().as_millis(),
        worst.as_millis()
    );
    assert!(worst < Duration::from_secs(2), "terminal output waited");
    let _ = call(
        &c,
        Request::HostsUnforward(api::HostForwardRef { forward: f.forward }),
    )
    .await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", f.local_port as u16))
            .await
            .is_err(),
        "the forward outlived its hold"
    );
    passed("a page on the host's loopback through a forward over SSH");

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
