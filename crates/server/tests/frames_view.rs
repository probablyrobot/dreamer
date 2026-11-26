//! A view on screen frames, through the real daemon: a pane followed on
//! frames is drawn exactly as a replica of the same pane draws it, input
//! sent from a view on frames is encoded by the pane's own modes, and a
//! view switches between records and frames under heavy output without a
//! fallback and ends where a replica is.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use api::terminal_input::Event;
use client::Client;
use replica::feed::{Feed, Frames};
use replica::screen::differences;
use replica::{Follower, RetryPolicy};
use terminal_ghostty::core::{CursorShape, FrameSnapshot};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

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
            "/tmp/drm-fv-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
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
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("rt/dreamerd.sock")
    }

    async fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.socket(), "test").await {
                Ok(_) => return,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn type_line(&self, line: &str) {
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    }

    fn feed(&self, frames: Frames) -> Feed {
        Feed::start_frames(self.socket(), replica::Via::Local, Some(0), None, frames).expect("feed")
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

fn shows(f: &FrameSnapshot, text: &str) -> bool {
    f.rows.iter().any(|r| r.text().contains(text))
}

/// The newest frame of `feed` once one satisfies `want`.
async fn frame_until(
    feed: &Feed,
    what: &str,
    want: impl Fn(&FrameSnapshot) -> bool,
) -> Arc<FrameSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last: Option<Arc<FrameSnapshot>> = None;
    loop {
        if let Some(f) = feed.take() {
            if want(&f) {
                return f;
            }
            last = Some(f);
        }
        if tokio::time::timeout_at(deadline, feed.wake.recv())
            .await
            .is_err()
        {
            panic!(
                "{what}: no such frame; feed {:?}, last frame:\n{}",
                feed.status(),
                last.map(|f| f.text_dump()).unwrap_or_default()
            );
        }
    }
}

/// Wait until a view on records and a view on frames both show `marker`
/// and would draw the same; both are taken from the same pane, so they
/// meet once the pane is quiet.
async fn same_picture(stream: &Feed, frames: &Feed, marker: &str) -> Arc<FrameSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut a = frame_until(stream, marker, |f| shows(f, marker)).await;
    let mut b = frame_until(frames, marker, |f| shows(f, marker)).await;
    loop {
        let diff = differences(&a, &b);
        if diff.is_empty() {
            return b;
        }
        if Instant::now() > deadline {
            panic!(
                "{marker}: the views never drew the same: {diff:#?}\nrecords:\n{}\nframes:\n{}",
                a.text_dump(),
                b.text_dump()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        a = stream.take().unwrap_or(a);
        b = frames.take().unwrap_or(b);
    }
}

#[tokio::test]
async fn a_pane_on_frames_is_drawn_as_its_replica_is() {
    let s = Setup::new();
    s.ready().await;
    let stream = s.feed(Frames::Off);
    let frames = s.feed(Frames::Always);
    // Styles, colors, wide characters and combining marks, then enough
    // lines to scroll, so the screen's top is a row of history.
    s.type_line(
        "printf '\\033[1mbold\\033[0m \\033[3;31mred\\033[0m \\033[38;5;208m256\\033[0m \\033[38;2;10;200;30;48;5;24mrgb\\033[0m \\033[4:3mcurly\\033[0m \\033[7minv\\033[0m 世界 e\\314\\201 👍\\n'; seq 1 40; echo ST''AGE-1",
    );
    let f = same_picture(&stream, &frames, "STAGE-1").await;
    assert!(f.view.offset > 0, "the screen scrolled into history");
    // A cursor shape the program sets.
    s.type_line("printf '\\033[6 q'; echo ST''AGE-2");
    let f = same_picture(&stream, &frames, "STAGE-2").await;
    assert_eq!(f.cursor.shape, CursorShape::Bar);
    // The alternate screen, with the mouse tracked: a view sends clicks
    // to the program then, and the wheel too.
    s.type_line("printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[HST''AGE-3\\n'");
    let f = same_picture(&stream, &frames, "STAGE-3").await;
    assert!(f.view.alt_screen && f.view.mouse_tracking, "{:?}", f.view);
    s.type_line("printf '\\033[?1000l\\033[?1006l\\033[?1049l\\033[2 q'; echo ST''AGE-4");
    let f = same_picture(&stream, &frames, "STAGE-4").await;
    assert!(!f.view.alt_screen && !f.view.mouse_tracking);
    assert!(frames.stats.screen_frames.load(Relaxed) > 0);
    assert_eq!(stream.stats.screen_frames.load(Relaxed), 0);
}

#[tokio::test]
async fn input_from_a_view_on_frames_is_encoded_by_the_pane_s_modes() {
    let s = Setup::new();
    s.ready().await;
    let feed = s.feed(Frames::Always);
    // Application cursor keys, SGR mouse reports, and bracketed paste;
    // the program reads a click, a paste, and Up (9 + 14 + 3 bytes).
    s.type_line(
        "printf '\\033[?1h\\033[?1000h\\033[?1006h\\033[?2004h'; stty raw -echo; printf 'I''1\\r\\n'; head -c 26 | tr '\\033' E; printf '\\r\\n'; stty sane; printf '\\033[?1l\\033[?1000l\\033[?1006l\\033[?2004l'",
    );
    let f = frame_until(&feed, "the program waiting", |f| shows(f, "I1")).await;
    assert!(
        f.view.mouse_tracking,
        "the frame carries the program's modes"
    );
    assert!(feed.stats.screen_frames.load(Relaxed) > 0, "on frames");
    feed.send(Event::Mouse(api::MouseInput {
        action: api::MouseAction::Press as i32,
        button: api::MouseButton::Left as i32,
        col: 3,
        row: 1,
        ..api::MouseInput::default()
    }));
    feed.send(Event::Paste(api::PasteInput {
        data: b"hi".to_vec(),
        ..api::PasteInput::default()
    }));
    feed.send(Event::Key(api::KeyInput {
        key: "ArrowUp".into(),
        ..api::KeyInput::default()
    }));
    frame_until(&feed, "the input as the program read it", |f| {
        shows(f, "E[<0;4;2ME[200~hiE[201~EOA")
    })
    .await;
    assert_eq!(feed.inputs_dropped.load(Relaxed), 0);
}

/// Step `f` until `done`, failing after 20 s.
async fn step_until(f: &mut Follower, what: &str, done: impl Fn(&mut Follower) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !done(f) {
        tokio::time::timeout_at(deadline, f.step())
            .await
            .unwrap_or_else(|_| panic!("{what}: timed out"))
            .unwrap_or_else(|e| panic!("{what}: {e}"));
    }
}

#[tokio::test]
async fn a_view_switches_between_records_and_frames_under_heavy_output() {
    let s = Setup::new();
    s.ready().await;
    let socket = s.socket();
    let mut f = Follower::start_frames(
        socket.clone(),
        replica::Via::Local,
        0,
        RetryPolicy::default(),
        None,
        Frames::On,
    )
    .await
    .expect("follow");
    assert!(f.frames_offered() && !f.on_frames());
    s.type_line("seq 1 300000; echo HEA''VY-DONE");
    for round in 0..10 {
        f.set_feed(true).await.expect("to frames");
        step_until(&mut f, "the full frame", |f| {
            f.on_frames() && f.screen().is_some_and(|v| v.number > 0)
        })
        .await;
        f.set_feed(false).await.expect("to records");
        step_until(&mut f, "the checkpoint", |f| !f.on_frames()).await;
        assert_eq!(f.checkpoints, 2 + round, "no fallback, no resnapshot");
    }
    // Ends where a replica that never switched is.
    let mut plain = Follower::start(socket, 0, RetryPolicy::default())
        .await
        .expect("follow");
    let done = |f: &mut Follower| {
        f.frame()
            .and_then(Result::ok)
            .is_some_and(|fr| shows(&fr, "HEAVY-DONE"))
    };
    step_until(&mut f, "the end of the output", done).await;
    step_until(&mut plain, "the end of the output", done).await;
    let (a, b) = (
        f.frame().expect("frame").expect("frame"),
        plain.frame().expect("frame").expect("frame"),
    );
    assert!(differences(&a, &b).is_empty(), "{:#?}", differences(&a, &b));
    assert_eq!(f.replica().text(), plain.replica().text());
}
