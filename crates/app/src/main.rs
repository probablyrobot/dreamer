//! Dreamer desktop client.
//!
//! At this stage the binary only proves the GPUI baseline: it opens a window,
//! draws a frame, and, with `--smoke`, exits once that frame has been
//! presented. Exit status 0 means a window opened and rendered at least one
//! frame; any failure exits nonzero.

use std::process::ExitCode;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

/// Give up if no frame is presented within this time in smoke mode.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(20);

struct Shell {
    smoke: bool,
    frame_seen: bool,
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.smoke && !self.frame_seen {
            self.frame_seen = true;
            // Runs after this frame has been drawn and presented.
            window.on_next_frame(|window, cx| {
                let size = window.viewport_size();
                println!(
                    "smoke: frame presented ({}x{} logical px, scale {})",
                    f32::from(size.width),
                    f32::from(size.height),
                    window.scale_factor()
                );
                cx.quit();
            });
            cx.notify();
        }
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x14161a))
            .text_color(rgb(0xe5e7eb))
            .child("dreamer")
    }
}

fn main() -> ExitCode {
    let smoke = std::env::args().skip(1).any(|a| a == "--smoke");
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.), px(500.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Shell {
                    smoke,
                    frame_seen: false,
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
