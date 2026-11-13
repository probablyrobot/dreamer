//! Pressure (spec 16.3, 16.5): a pane flooding output and a subscriber that
//! never reads must not delay input to another pane.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response, terminal_input::Event};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    /// Two panes: %0 and %1, both running /bin/sh.
    fn new(env: &[(&str, &str)]) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-pr-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        tmux.run_checked(&["split-window", "-t", "main", "/bin/sh"])
            .expect("split");
        let mut cmd = Command::new(EXE);
        cmd.args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let daemon = cmd.spawn().expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("rt/dreamerd.sock")
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.socket(), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn type_into(&self, pane: u32, line: &str) {
        let target = format!("%{pane}");
        self.tmux
            .run_checked(&["send-keys", "-t", &target, "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", &target, "Enter"])
            .expect("enter");
    }

    /// Whether the daemon's mirror of `pane` shows a line equal to `line`.
    async fn mirror_shows(&self, c: &Client, pane: u32, line: &str) -> bool {
        matches!(
            c.call(Request::TerminalRead(api::ReadParams {
                pane,
                history: Some(5),
                endpoint: None,
            }))
            .await,
            Ok(Response::TerminalRead(t)) if t.lines.iter().any(|l| l.trim_end() == line)
        )
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Type `echo K<n>` into `pane` through the daemon and time how long until
/// the daemon's mirror shows the output: input delivery plus the echo's
/// way back, the latency a person would feel.
async fn round_trips(s: &Setup, c: &Client, pane: u32, n: usize) -> Vec<Duration> {
    let mut times = Vec::new();
    for i in 0..n {
        let marker = format!("RT{i}X");
        let started = Instant::now();
        c.call(Request::TerminalInput(api::TerminalInput {
            pane,
            event: Some(Event::Text(api::TextInput {
                // Quotes split the marker so the echoed command never matches.
                text: format!("echo RT{i}''X\r"),
            })),
            ..api::TerminalInput::default()
        }))
        .await
        .expect("input");
        let deadline = started + Duration::from_secs(20);
        while !s.mirror_shows(c, pane, &marker).await {
            assert!(Instant::now() < deadline, "{marker} never came back");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        times.push(started.elapsed());
    }
    times.sort();
    times
}

fn p95(times: &[Duration]) -> Duration {
    times[(times.len() * 95 / 100).min(times.len() - 1)]
}

#[tokio::test]
async fn a_flooding_pane_and_a_stalled_subscriber_do_not_delay_another_pane() {
    // One mirror worker: both panes share an owner thread, the hard case.
    let s = Setup::new(&[("DREAMER_MIRROR_WORKERS", "1")]);
    let c = s.client().await;
    // Both panes live first.
    s.type_into(0, "echo A''READY");
    s.type_into(1, "echo B''READY");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(s.mirror_shows(&c, 0, "AREADY").await && s.mirror_shows(&c, 1, "BREADY").await) {
        assert!(Instant::now() < deadline, "panes never came up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let quiet = round_trips(&s, &c, 1, 20).await;

    // Pane %0 floods, and a subscriber to it never reads its stream.
    let stalled = Client::connect_with(&s.socket(), "stalled", 1)
        .await
        .expect("stalled client");
    let _sub = stalled
        .call(Request::TerminalSubscribe(api::TerminalSubscribe {
            pane: 0,
            ..api::TerminalSubscribe::default()
        }))
        .await
        .expect("subscribe");
    s.type_into(
        0,
        "yes 0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz",
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    // A large paste into the busy pane at the same time.
    let paster = s.client().await;
    let paste = tokio::spawn(async move {
        let data = vec![b'p'; 8 << 20];
        let t = Instant::now();
        let r = client::paste(&paster, 0, None, &data).await;
        (r.is_ok(), t.elapsed())
    });
    let before = ingested(&c, 0).await;
    let t = Instant::now();
    let busy = round_trips(&s, &c, 1, 20).await;
    let rate = (ingested(&c, 0).await - before) as f64 / t.elapsed().as_secs_f64() / 1e6;
    s.type_into(0, "\u{3}");
    let (pasted, paste_time) = paste.await.expect("paste task");

    println!(
        "pressure: pane %1 input round trip p95 {:?} quiet, {:?} with %0 flooding ({rate:.1} MB/s ingested), a stalled subscriber, and an 8 MiB paste to %0 ({} in {paste_time:?}) (max {:?})",
        p95(&quiet),
        p95(&busy),
        if pasted { "delivered" } else { "failed" },
        busy.last().copied().unwrap_or_default()
    );
    assert!(
        rate > 1.0,
        "pane %0 was not really flooding ({rate:.2} MB/s)"
    );
    assert!(
        p95(&busy) < Duration::from_millis(250).max(p95(&quiet) * 5),
        "input to a quiet pane was delayed by a busy one: p95 {:?} (quiet {:?})",
        p95(&busy),
        p95(&quiet)
    );
}

async fn ingested(c: &Client, pane: u32) -> u64 {
    let Ok(Response::TerminalMirrors(m)) = c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
    else {
        return 0;
    };
    m.mirrors
        .iter()
        .find(|x| x.pane == pane)
        .map_or(0, |x| x.ingested_bytes)
}
