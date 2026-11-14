//! Taps through a real daemon (spec 12.8): raw, text, and meta views of
//! real tmux panes, and a tap that stops reading gets a gap while the pane
//! and another tap keep flowing. The journal is tiny, so a stalled tap
//! cannot catch up from it.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::sync::mpsc::Receiver;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-tap-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_JOURNAL_PANE_BYTES", "4096")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    async fn client(&self) -> Client {
        self.client_with(1024).await
    }

    async fn client_with(&self, pushed: usize) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect_with(&self.root.join("rt/dreamerd.sock"), "test", pushed).await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn tmux(&self, args: &[&str]) {
        self.tmux.run_checked(args).expect("tmux command");
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

async fn pane0(c: &Client) -> u32 {
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    t.panes[0].id
}

async fn tap(c: &Client, level: api::TapLevel, pane: u32) -> u64 {
    let Ok(Response::TapsSubscribe(s)) = c
        .call(Request::TapsSubscribe(api::TapSubscribe {
            level: level as i32,
            panes: vec![pane],
            interval_ms: Some(50),
            ..api::TapSubscribe::default()
        }))
        .await
    else {
        panic!("tap")
    };
    s.id
}

/// `taps.data` for `tap` until `done` says enough; all of it.
async fn collect(
    rx: &mut Receiver<Pushed>,
    tap: u64,
    mut done: impl FnMut(&[api::TapData]) -> bool,
) -> Vec<api::TapData> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut got = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "taps.data" => {
                let d = api::TapData::decode(payload.as_slice()).expect("tap data");
                if d.tap == tap {
                    got.push(d);
                    if done(&got) {
                        return got;
                    }
                }
            }
            Ok(Some(_)) => {}
            other => panic!("tap {tap} stopped short ({other:?}) after {got:?}"),
        }
    }
}

fn lines(got: &[api::TapData]) -> Vec<String> {
    got.iter().flat_map(|d| d.lines.iter().cloned()).collect()
}

#[tokio::test]
async fn raw_text_and_meta_views_of_a_pane() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    let pane = pane0(&c).await;
    let target = format!("%{pane}");
    let raw = tap(&c, api::TapLevel::Raw, pane).await;
    let text = tap(&c, api::TapLevel::Text, pane).await;
    let meta = tap(&c, api::TapLevel::Meta, pane).await;
    s.tmux(&[
        "send-keys",
        "-t",
        &target,
        "printf 'tap-%s\\n' one two",
        "Enter",
    ]);
    s.tmux(&[
        "send-keys",
        "-t",
        &target,
        "printf '\\033]2;tap-title\\007'",
        "Enter",
    ]);
    let mut seen = Vec::new();
    let mut want = [(raw, false), (text, false), (meta, false)];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while want.iter().any(|(_, ok)| !ok) {
        let Ok(Some(Pushed::Event { topic, payload, .. })) =
            tokio::time::timeout_at(deadline, rx.recv()).await
        else {
            panic!("missing tap output: {want:?} after {seen:?}")
        };
        if topic != "taps.data" {
            continue;
        }
        let d = api::TapData::decode(payload.as_slice()).expect("data");
        let all_raw: Vec<u8> = seen
            .iter()
            .filter(|x: &&api::TapData| x.tap == raw)
            .flat_map(|x| x.raw.clone())
            .chain(if d.tap == raw {
                d.raw.clone()
            } else {
                Vec::new()
            })
            .collect();
        let all_text: Vec<String> = lines(
            &seen
                .iter()
                .filter(|x: &&api::TapData| x.tap == text)
                .cloned()
                .chain((d.tap == text).then(|| d.clone()))
                .collect::<Vec<_>>(),
        );
        for (t, ok) in &mut want {
            *ok = *ok
                || (*t == raw && String::from_utf8_lossy(&all_raw).contains("tap-two"))
                || (*t == text && all_text.iter().any(|l| l == "tap-two"))
                || (*t == meta
                    && d.tap == meta
                    && d.meta.as_ref().and_then(|m| m.title.as_deref()) == Some("tap-title"));
        }
        seen.push(d);
    }
    // Text: committed lines in order, escapes gone, each once.
    let text_lines = lines(
        &seen
            .iter()
            .filter(|d| d.tap == text)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let one = text_lines
        .iter()
        .position(|l| l == "tap-one")
        .expect("tap-one");
    assert_eq!(text_lines[one + 1], "tap-two", "{text_lines:?}");
    assert_eq!(text_lines.iter().filter(|l| *l == "tap-two").count(), 1);
    assert!(text_lines.iter().all(|l| !l.contains('\u{1b}')));
}

#[tokio::test]
async fn a_tap_that_stops_reading_gets_a_gap_and_slows_nothing() {
    let s = Setup::new();
    let mut good = s.client().await;
    let mut good_rx = good.pushed().expect("pushed");
    let pane = pane0(&good).await;
    let target = format!("%{pane}");
    let good_tap = tap(&good, api::TapLevel::Text, pane).await;
    // This client holds one pushed item and then stops reading.
    let mut slow = s.client_with(1).await;
    let mut slow_rx = slow.pushed().expect("pushed");
    let slow_tap = tap(&slow, api::TapLevel::Raw, pane).await;
    s.tmux(&[
        "send-keys",
        "-t",
        &target,
        "seq 1 60000; echo seq-done",
        "Enter",
    ]);
    // The pane and the reading tap get through all of it.
    let got = collect(&mut good_rx, good_tap, |g| {
        lines(g).iter().any(|l| l == "seq-done")
    })
    .await;
    assert!(lines(&got).iter().any(|l| l == "seq-done"));
    let Ok(Response::TerminalRead(t)) = good
        .call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(0),
            endpoint: None,
        }))
        .await
    else {
        panic!("read")
    };
    assert!(
        t.lines.iter().any(|l| l == "seq-done"),
        "the pane itself is current"
    );
    // Now the slow tap reads: it says it missed output, and goes on.
    let got = collect(&mut slow_rx, slow_tap, |g| {
        g.iter().any(|d| d.gap.is_some())
            && String::from_utf8_lossy(&g.iter().flat_map(|d| d.raw.clone()).collect::<Vec<u8>>())
                .contains("seq-done")
    })
    .await;
    let gap = got.iter().find_map(|d| d.gap.clone()).expect("gap");
    assert_eq!(gap.reason, "lagging");
    drop(slow);
}
