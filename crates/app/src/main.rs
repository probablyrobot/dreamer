//! Dreamer desktop client.
//!
//! ```text
//! dreamer [--smoke] [--fixture FILE]
//! dreamer [--smoke [--expect TEXT]] --attach %PANE [-L socket | --tmux-socket-path P] [--tmux PATH]
//! ```
//!
//! With no arguments (as when launched from Finder or a desktop launcher) it
//! follows the active pane of the default tmux server. `--fixture` paints a
//! VT fixture; `--smoke` alone paints the built-in renderer demo. With `--attach` it shows a live tmux pane: a
//! background owner thread follows the pane through `dreamerd` (started on
//! demand) and publishes frames only when the pane changed; the window
//! repaints only when woken by a new frame.
//!
//! `--smoke` exits once a frame has been presented, printing what was drawn.
//! `--smoke-repaint` then paints the same frame again and checks that every
//! row came from the shaping cache. Exit status 5 means the display did not
//! refresh at all (a locked screen), so paint checks proved nothing.
//! In attach mode it waits for a frame containing `--expect` and then
//! checks that 1.5 s of idle pane cause no view wakeups and no paints.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, font, prelude::*, px, rgb, size,
};
use replica::feed::Feed;
use terminal_ghostty::core::{FrameSnapshot, GridSize};
use terminal_ghostty::{Engine, EngineConfig};
use terminal_render::{
    InlineImage, TerminalElement, cell_metrics, last_paint_stats, paint_count, rgba_image,
};

/// Give up if no frame is presented within this time in smoke mode.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Exit status when the display does not refresh at all (for example a
/// locked screen), so paint-based checks prove nothing either way.
const INCONCLUSIVE: i32 = 5;
/// Idle time a live smoke run watches for unwanted wakeups.
const IDLE_CHECK: Duration = Duration::from_millis(1500);
const GRID: GridSize = GridSize { cols: 80, rows: 16 };

/// A stream exercising styles, colors, wide and combining text, emoji, box
/// drawing, and a bar cursor.
const DEMO: &[u8] = "\x1b[1mdreamer\x1b[0m renderer proof  \x1b[2mM0-08\x1b[0m\r\n\
\x1b[31mred \x1b[32mgreen \x1b[33myellow \x1b[34mblue \x1b[35mmagenta \x1b[36mcyan\x1b[0m \
\x1b[38;2;255;140;0mtruecolor\x1b[0m \x1b[48;5;24m bg256 \x1b[0m \x1b[7minverse\x1b[0m\r\n\
\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[4:3mcurly\x1b[0m \
\x1b[9mstrike\x1b[0m \x1b[2mfaint\x1b[0m\r\n\
wide: 世界 こんにちは 한국어   combining: e\u{301} n\u{303} a\u{308}\r\n\
emoji: 👍 👍🏽 👨\u{200d}👩\u{200d}👧 🚀 ✅   box: ┌──┬──┐ │▓▒░│ └──┴──┘\r\n\
grid check: |0123456789|ABCDEFGHIJ|世界世界世|\r\n\
\r\n\x1b[6 qcursor (bar) here:"
    .as_bytes();

#[derive(Debug, Clone, PartialEq)]
enum Smoke {
    Off,
    /// Exit after the first presented frame.
    First,
    /// Also paint the same frame again and check the shaping cache.
    Repaint,
    /// Live: wait for a frame containing this text, then check idleness.
    Expect(String),
}

struct Shell {
    smoke: Smoke,
    frame_seen: bool,
    frame: Arc<FrameSnapshot>,
    images: Vec<InlineImage>,
    /// Live frames applied to the view.
    live_frames: u64,
}

impl Shell {
    fn report_frame(window: &mut Window) {
        let terminal_font = font("Menlo");
        let metrics = cell_metrics(window, &terminal_font, px(14.0), 1.2);
        let size = window.viewport_size();
        println!(
            "smoke: frame presented ({}x{} logical px, scale {})",
            f32::from(size.width),
            f32::from(size.height),
            window.scale_factor()
        );
        println!(
            "smoke: cell {}x{} px, baseline {} px",
            f32::from(metrics.width),
            f32::from(metrics.height),
            f32::from(metrics.ascent)
        );
        println!("smoke: painted {:?}", last_paint_stats());
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(self.smoke, Smoke::First | Smoke::Repaint) && !self.frame_seen {
            self.frame_seen = true;
            let this = cx.entity();
            let repaint = self.smoke == Smoke::Repaint;
            let handle = window.window_handle();
            window.on_next_frame(move |window, cx| {
                Shell::report_frame(window);
                if !repaint {
                    cx.quit();
                    return;
                }
                let first = last_paint_stats();
                // Paint the same frame again: every row must come from the
                // shaping cache.
                let before = paint_count();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_millis(300)).await;
                    this.update(cx, |_, cx| cx.notify());
                    let _ = handle.update(cx, |_, window, _| window.refresh());
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    let painted = paint_count() - before;
                    let again = last_paint_stats();
                    let code = if painted == 0 {
                        println!("smoke: the display did not refresh (screen locked or asleep?); repaint checks not run");
                        INCONCLUSIVE
                    } else {
                        println!("smoke: repainted {again:?}");
                        let cached = again.shaped_lines == 0 && again.reused_lines == first.shaped_lines;
                        if cached { 0 } else { 3 }
                    };
                    cx.update(|cx| cx.quit());
                    std::process::exit(code);
                })
                .detach();
            });
            cx.notify();
        }
        div()
            .size_full()
            .bg(rgb(0x14161a))
            .p_2()
            .child(TerminalElement {
                frame: Arc::clone(&self.frame),
                font: font("Menlo"),
                font_size: px(14.0),
                line_height: 1.2,
                images: self.images.clone(),
            })
    }
}

/// The display backend GPUI will use, by its own selection rule: Wayland
/// when `WAYLAND_DISPLAY` is set, else X11 when `DISPLAY` is set, else a
/// headless platform that draws nothing.
fn display_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        return "macOS (Metal)";
    }
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set("WAYLAND_DISPLAY") {
        "Wayland"
    } else if set("DISPLAY") {
        "X11"
    } else {
        "headless"
    }
}

fn demo_image() -> Option<InlineImage> {
    let (w, h) = (96u32, 48u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 200, 255]);
        }
    }
    Some(InlineImage {
        image: rgba_image(w, h, rgba)?,
        col: 66,
        row: 7,
        cols: 12,
        rows: 4,
    })
}

fn load_frame(bytes: &[u8]) -> Result<Arc<FrameSnapshot>, String> {
    let mut config = EngineConfig::new(GRID);
    config.capture_replies = false;
    let mut engine = Engine::new(config).map_err(|e| e.to_string())?;
    engine.feed(bytes).map_err(|e| e.to_string())?;
    engine.frame().map_err(|e| e.to_string())
}

#[derive(Debug, Default)]
struct Args {
    smoke: bool,
    repaint: bool,
    expect: Option<String>,
    fixture: Option<String>,
    attach: Option<u32>,
    /// `dreamerd` options for a daemon started on demand.
    daemon: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--smoke" => a.smoke = true,
            "--smoke-repaint" => {
                a.smoke = true;
                a.repaint = true;
            }
            "--expect" => a.expect = Some(value("--expect")?),
            "--fixture" => a.fixture = Some(value("--fixture")?),
            "--attach" => {
                let p = value("--attach")?;
                a.attach = Some(
                    p.trim_start_matches('%')
                        .parse()
                        .map_err(|_| format!("`{p}` is not a pane id"))?,
                );
            }
            "-L" | "--tmux-socket" => {
                a.daemon.push("--tmux-socket".into());
                a.daemon.push(value("-L")?);
            }
            "--tmux-socket-path" | "--tmux" => {
                a.daemon.push(arg.clone());
                a.daemon.push(value(arg)?);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dreamer: {e}");
            return ExitCode::from(2);
        }
    };
    let frame = match &args.fixture {
        Some(path) => std::fs::read(path)
            .map_err(|e| format!("{path}: {e}"))
            .and_then(|b| load_frame(&b)),
        None if args.attach.is_some() || !args.smoke => {
            load_frame(b"\x1b[2mconnecting to dreamerd...\x1b[0m")
        }
        None => load_frame(DEMO),
    };
    let frame = match frame {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backend = display_backend();
    if args.smoke {
        println!("smoke: backend {backend}");
        if backend == "headless" {
            // A headless platform would "present" frames without drawing.
            eprintln!("smoke: no display server in the environment; nothing would be drawn");
            return ExitCode::from(4);
        }
    }
    let images: Vec<InlineImage> = if args.smoke && args.fixture.is_none() && args.attach.is_none()
    {
        demo_image().into_iter().collect()
    } else {
        Vec::new()
    };
    let smoke = match (args.smoke, args.attach, &args.expect) {
        (false, _, _) => Smoke::Off,
        (true, Some(_), Some(text)) => Smoke::Expect(text.clone()),
        (true, Some(_), None) => Smoke::Expect(String::new()),
        (true, None, _) if args.repaint => Smoke::Repaint,
        (true, None, _) => Smoke::First,
    };
    // Launched with nothing to show (Finder, a desktop launcher): follow the
    // active pane of the default tmux server.
    let follow = match (args.attach, args.smoke, &args.fixture) {
        (Some(p), _, _) => Some(Some(p)),
        (None, false, None) => Some(None),
        _ => None,
    };
    let feed = match follow {
        Some(pane) => {
            let launch = client::Launch::new(args.daemon.clone());
            match Feed::start(launch.socket.clone(), pane, Some(launch)) {
                Ok(f) => Some(Arc::new(f)),
                Err(e) => {
                    eprintln!("error: cannot start the pane feed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(900.), px(420.)), cx);
        let smoke_mode = smoke.clone();
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Shell {
                    smoke: smoke_mode,
                    frame_seen: false,
                    frame,
                    images,
                    live_frames: 0,
                })
            },
        );
        let window = match opened {
            Ok(w) => w,
            Err(e) => {
                eprintln!("error: could not open a window: {e:#}");
                std::process::exit(1);
            }
        };
        if let Some(feed) = feed {
            // Wake only when the feed has a newer frame; never poll.
            cx.spawn(async move |cx| {
                while feed.wake.recv().await.is_ok() {
                    let Some(frame) = feed.take() else { continue };
                    let text = frame.text_dump();
                    let done = window.update(cx, |shell, window, cx| {
                        shell.frame = frame;
                        shell.live_frames += 1;
                        cx.notify();
                        match &shell.smoke {
                            Smoke::Expect(want)
                                if !shell.frame_seen && text.contains(want.as_str()) =>
                            {
                                shell.frame_seen = true;
                                window.refresh();
                                true
                            }
                            _ => false,
                        }
                    });
                    if done.unwrap_or(false) {
                        let feed = feed.clone();
                        cx.spawn(async move |cx| idle_check(feed, cx).await)
                            .detach();
                    }
                }
            })
            .detach();
        }
        if smoke != Smoke::Off {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(SMOKE_TIMEOUT).await;
                eprintln!("smoke: no matching frame presented within {SMOKE_TIMEOUT:?}");
                std::process::exit(2);
            })
            .detach();
        }
        cx.activate(true);
    });
    ExitCode::SUCCESS
}

/// After the expected frame: an idle pane must cause no view wakeups and
/// no paints.
async fn idle_check(feed: Arc<Feed>, cx: &mut gpui::AsyncApp) {
    // Let the presentation of the expected frame finish first.
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    cx.update(|cx| {
        if let Some(w) = cx.windows().first() {
            let _ = w.update(cx, |_, window, _| Shell::report_frame(window));
        }
    });
    println!(
        "smoke: live frames painted: {} paints so far",
        paint_count()
    );
    let wakes = feed.stats.view_wakeups.load(Ordering::Relaxed);
    let paints = paint_count();
    cx.background_executor().timer(IDLE_CHECK).await;
    let wakes = feed.stats.view_wakeups.load(Ordering::Relaxed) - wakes;
    let paints = paint_count() - paints;
    println!(
        "smoke: live pane idle {:.1}s: {wakes} view wakeups, {paints} paints ({} frames built, {} owner wakeups)",
        IDLE_CHECK.as_secs_f32(),
        feed.stats.frames.load(Ordering::Relaxed),
        feed.stats.owner_wakeups.load(Ordering::Relaxed)
    );
    let code = if wakes == 0 && paints == 0 { 0 } else { 3 };
    cx.update(|cx| cx.quit());
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn attach_arguments_pass_tmux_options_to_the_daemon() {
        let a = parse(&["--attach".into(), "%3".into(), "-L".into(), "x".into()]).expect("parse");
        assert_eq!(a.attach, Some(3));
        assert_eq!(a.daemon, ["--tmux-socket", "x"]);
        assert!(parse(&["--attach".into(), "nope".into()]).is_err());
    }
}
