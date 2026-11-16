//! The protocol matrix (spec 15, M7-03), end to end: a program in a real
//! tmux pane writes each sequence; tmux carries it in control mode to the
//! daemon's mirror; a client replica restores the mirror's checkpoint and
//! follows its stream; the replica's frame must show the feature. Every
//! row of `docs/terminal-compatibility.md`'s output table cites a test
//! here, and a feature without one is not claimed.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client::Client;
use terminal_ghostty::core::{Attrs, CellWidth, Color, CursorShape, FrameSnapshot, Rgb, Underline};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new(name: &str) -> Setup {
        let root = PathBuf::from(format!("/tmp/drm-proto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
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

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.root.join("rt/dreamerd.sock"), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    /// A new window that writes `bytes` (octal-escaped, so the shell passes
    /// them through untouched) once a replica follows it: the live path,
    /// not a cold attach, which starts from tmux's own capture (see
    /// `tmux-bootstrap.md`). The pane's replica, following.
    async fn emit(&self, bytes: &[u8]) -> replica::feed::Feed {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (script, go) = (
            self.root.join(format!("emit-{n}.sh")),
            self.root.join(format!("go-{n}")),
        );
        let escaped: String = bytes.iter().map(|b| format!("\\{b:03o}")).collect();
        std::fs::write(
            &script,
            format!(
                "printf 'WAITING\\r\\n'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nprintf '{escaped}'\nexec sleep 600\n",
                go.display()
            ),
        )
        .expect("script");
        let out = self
            .tmux
            .run_checked(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                &format!("/bin/sh {}", script.display()),
            ])
            .expect("new-window");
        let pane: u32 = out.trim().trim_start_matches('%').parse().expect("pane id");
        let feed = replica::feed::Feed::start(self.root.join("rt/dreamerd.sock"), Some(pane), None)
            .expect("feed");
        frame_until(&feed, "the pane waiting", |f| {
            f.text_dump().contains("WAITING")
        })
        .await;
        std::fs::write(&go, "").expect("go");
        feed
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

/// The first frame from the replica that satisfies `want`.
async fn frame_until(
    feed: &replica::feed::Feed,
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
                "{what}: never seen; feed {:?}; last frame:\n{}",
                feed.status(),
                last.map(|f| f.text_dump()).unwrap_or_default()
            );
        }
    }
}

/// The cell holding `text` on the row that has it.
fn cell<'a>(f: &'a FrameSnapshot, text: &str) -> Option<&'a terminal_ghostty::core::Cell> {
    f.rows
        .iter()
        .flat_map(|r| r.cells.iter())
        .find(|c| &*c.text == text)
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

#[tokio::test]
async fn colors_and_text_attributes_reach_the_replica() {
    let s = Setup::new("sgr");
    let _c = s.client().await;
    let feed = s
        .emit(
            concat!(
                "\x1b[38;2;10;20;30mt\x1b[0m ",    // truecolor foreground
                "\x1b[48;2;200;100;50mb\x1b[0m ",  // truecolor background
                "\x1b[38;5;200mp\x1b[0m ",         // 256-color palette
                "\x1b[31mr\x1b[0m ",               // 16-color
                "\x1b[1;3;9;53ms\x1b[0m ",         // bold, italic, strike, overline
                "\x1b[2;7mf\x1b[0m ",              // faint, inverse
                "\x1b[4:3;58;2;250;0;0mu\x1b[0m ", // curly underline, colored
                "\x1b[4:2md\x1b[0m \x1b[4:4mo\x1b[0m \x1b[4:5mh\x1b[0m\r\n", // double, dotted, dashed
                "END-SGR\r\n"
            )
            .as_bytes(),
        )
        .await;
    let f = frame_until(&feed, "the SGR line", |f| f.text_dump().contains("END-SGR")).await;
    let style = |t: &str| cell(&f, t).unwrap_or_else(|| panic!("no {t}")).style;
    assert_eq!(style("t").fg, Color::Rgb(rgb(10, 20, 30)));
    assert_eq!(style("b").bg, Color::Rgb(rgb(200, 100, 50)));
    assert_eq!(style("p").fg, Color::Palette(200));
    assert_eq!(style("r").fg, Color::Palette(1));
    let a = style("s").attrs;
    for bit in [
        Attrs::BOLD,
        Attrs::ITALIC,
        Attrs::STRIKETHROUGH,
        Attrs::OVERLINE,
    ] {
        assert!(a.has(bit), "attribute {bit:#x} lost: {a:?}");
    }
    let a = style("f").attrs;
    assert!(a.has(Attrs::FAINT) && a.has(Attrs::INVERSE), "{a:?}");
    let u = style("u");
    assert_eq!(u.underline, Underline::Curly);
    assert_eq!(u.underline_color, Color::Rgb(rgb(250, 0, 0)));
    assert_eq!(style("d").underline, Underline::Double);
    assert_eq!(style("o").underline, Underline::Dotted);
    assert_eq!(style("h").underline, Underline::Dashed);
}

#[tokio::test]
async fn wide_characters_emoji_and_combining_marks_keep_their_cells() {
    let s = Setup::new("wide");
    let _c = s.client().await;
    let feed = s
        .emit("日本 👍 e\u{301}x \x1b[?2027h🇯🇵\r\nEND-WIDE\r\n".as_bytes())
        .await;
    let f = frame_until(&feed, "the wide line", |f| {
        f.text_dump().contains("END-WIDE")
    })
    .await;
    let row = f
        .rows
        .iter()
        .find(|r| r.text().starts_with("日本"))
        .expect("the wide row");
    let widths: Vec<CellWidth> = row.cells.iter().take(4).map(|c| c.width).collect();
    assert_eq!(
        widths,
        [
            CellWidth::Wide,
            CellWidth::SpacerTail,
            CellWidth::Wide,
            CellWidth::SpacerTail
        ],
        "日本: two cells each"
    );
    assert_eq!(cell(&f, "👍").map(|c| c.width), Some(CellWidth::Wide));
    assert!(
        cell(&f, "e\u{301}").is_some(),
        "a combining mark joins its base cell"
    );
    assert_eq!(
        cell(&f, "🇯🇵").map(|c| c.width),
        Some(CellWidth::Wide),
        "a flag is one wide cell"
    );
    assert!(
        row.text().starts_with("日本 👍 e\u{301}x 🇯🇵"),
        "{}",
        row.text()
    );
}

#[tokio::test]
async fn cursor_shapes_the_alternate_screen_and_hyperlinks_reach_the_replica() {
    let s = Setup::new("modes");
    let _c = s.client().await;
    // A blinking bar cursor, a hyperlink, then the alternate screen.
    let feed = s
        .emit(
            concat!(
                "\x1b[5 q",
                "\x1b]8;;https://example.com/x\x1b\\LINK\x1b]8;;\x1b\\\r\n",
                "MAIN-SCREEN\r\n",
            )
            .as_bytes(),
        )
        .await;
    let f = frame_until(&feed, "the main screen", |f| {
        f.text_dump().contains("MAIN-SCREEN")
    })
    .await;
    assert_eq!(f.cursor.shape, CursorShape::Bar);
    assert!(f.cursor.blinking);
    let y = f
        .rows
        .iter()
        .position(|r| r.text().starts_with("LINK"))
        .and_then(|y| u16::try_from(y).ok())
        .expect("the LINK row");
    let link = feed.link_at(1, y).await.expect("a link over LINK");
    assert_eq!(link.uri, "https://example.com/x");

    let feed = s.emit("\x1b[?1049hON-ALT\r\n".as_bytes()).await;
    let f = frame_until(&feed, "the alternate screen", |f| {
        f.view.alt_screen && f.text_dump().contains("ON-ALT")
    })
    .await;
    assert_eq!(
        f.view.total_rows, f.view.len,
        "the alternate screen has no history"
    );
}

#[tokio::test]
async fn titles_and_working_directories_reach_every_view() {
    let s = Setup::new("title");
    let _c = s.client().await;
    let feed = s
        .emit("\x1b]2;the-title\x07\x1b]7;file://host/tmp/where\x07END-TITLE\r\n".as_bytes())
        .await;
    frame_until(&feed, "the title line", |f| {
        f.text_dump().contains("END-TITLE")
    })
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let m = feed.meta();
        if m.title.as_deref() == Some("the-title")
            && m.cwd.as_deref() == Some("file://host/tmp/where")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never saw the title and cwd: {m:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
