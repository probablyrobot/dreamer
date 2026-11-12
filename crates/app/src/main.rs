//! Dreamer desktop client.
//!
//! At this stage (M0) the binary proves the GPUI baseline and the renderer:
//! it feeds a VT stream (a built-in demo, or `--fixture FILE`) through the
//! terminal engine and paints the resulting frame. With `--smoke` it exits
//! once a frame has been presented, printing the cell metrics and paint
//! counts; exit status 0 means a window opened and rendered.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, font, prelude::*, px, rgb, size,
};
use terminal_ghostty::core::{FrameSnapshot, GridSize};
use terminal_ghostty::{Engine, EngineConfig};
use terminal_render::{InlineImage, TerminalElement, cell_metrics, last_paint_stats, rgba_image};

/// Give up if no frame is presented within this time in smoke mode.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(20);
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

struct Shell {
    smoke: bool,
    frame_seen: bool,
    frame: Arc<FrameSnapshot>,
    images: Vec<InlineImage>,
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal_font = font("Menlo");
        if self.smoke && !self.frame_seen {
            self.frame_seen = true;
            let metrics = cell_metrics(window, &terminal_font, px(14.0), 1.2);
            window.on_next_frame(move |window, cx| {
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
                cx.quit();
            });
            cx.notify();
        }
        div()
            .size_full()
            .bg(rgb(0x14161a))
            .p_2()
            .child(TerminalElement {
                frame: Arc::clone(&self.frame),
                font: terminal_font,
                font_size: px(14.0),
                line_height: 1.2,
                images: self.images.clone(),
            })
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

fn load_frame(fixture: Option<&str>) -> Result<Arc<FrameSnapshot>, String> {
    let bytes = match fixture {
        Some(path) => std::fs::read(path).map_err(|e| format!("{path}: {e}"))?,
        None => DEMO.to_vec(),
    };
    let mut config = EngineConfig::new(GRID);
    config.capture_replies = false;
    let mut engine = Engine::new(config).map_err(|e| e.to_string())?;
    engine.feed(&bytes).map_err(|e| e.to_string())?;
    engine.frame().map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let smoke = args.iter().any(|a| a == "--smoke");
    let fixture = args
        .iter()
        .position(|a| a == "--fixture")
        .and_then(|i| args.get(i + 1).cloned());
    let frame = match load_frame(fixture.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let images: Vec<InlineImage> = if fixture.is_none() {
        demo_image().into_iter().collect()
    } else {
        Vec::new()
    };

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(900.), px(420.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Shell {
                    smoke,
                    frame_seen: false,
                    frame,
                    images,
                })
            },
        );
        if let Err(e) = opened {
            eprintln!("error: could not open a window: {e:#}");
            std::process::exit(1);
        }
        if smoke {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(SMOKE_TIMEOUT).await;
                eprintln!("smoke: no frame presented within {SMOKE_TIMEOUT:?}");
                std::process::exit(2);
            })
            .detach();
        }
        cx.activate(true);
    });
    ExitCode::SUCCESS
}
