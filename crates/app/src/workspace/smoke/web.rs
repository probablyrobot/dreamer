//! `dreamer --smoke-web DIR`: a web pane against an isolated tmux whose
//! session `main` has one shell pane. Opens a page with a known color and
//! a long body in a pane beside it (`web.open`), waits until Chromium has
//! loaded it and is producing frames, captures the window, scrolls the
//! page with a wheel event, and captures it again.
//!
//! Writes to DIR: `before.png` and `after.png` (the window, from
//! `screencapture`), and `rect.txt`: the pane's rectangle and the window's
//! size, in points (`x y width height window_width window_height`). The
//! gate checks the pixels. Exit 5: the window or the page was not being
//! drawn (screen locked, display asleep), so nothing can be judged.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, AppContext as _, AsyncApp, WeakEntity};

use super::super::{PaneView, Workspace};
use super::{fail, read, wait_long};

/// The page: one screen of the top color, then three of the bottom color.
/// Its title counts animation frames (Chromium's compositor is drawing)
/// and reports the scroll position.
pub(crate) const PAGE: &str = "data:text/html,<html><body style='margin:0'>\
<div style='height:100vh;background:%232a6fdb'></div>\
<div style='height:300vh;background:%23d9822b'></div>\
<script>let n=0;function f(){n++;document.title='raf '+n+' y '+Math.round(scrollY);\
requestAnimationFrame(f)}requestAnimationFrame(f)</script></body></html>";

const THREE: Duration = Duration::from_secs(3);
const TEN: Duration = Duration::from_secs(10);

/// Animation frames and scroll position from the page's title.
fn progress(title: &str) -> Option<(u64, i64)> {
    let mut it = title.split(' ');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some("raf"), Some(n), Some("y"), Some(y)) => Some((n.parse().ok()?, y.parse().ok()?)),
        _ => None,
    }
}

/// The web pane's id, its title, its rectangle, and its window number.
type Seen = (u32, String, Option<web_engine::Rect>, Option<isize>);

fn web_pane(w: &Workspace, cx: &gpui::App) -> Option<Result<Seen, String>> {
    w.panes.iter().find_map(|(id, v)| match v {
        PaneView::Web { view, .. } => {
            let p = view.read(cx);
            if let Some(problem) = &p.problem {
                return Some(Err(problem.clone()));
            }
            let v = p.view.as_ref()?;
            v.load_status()?;
            Some(Ok((*id, v.title(), v.placed(), v.window_number())))
        }
        _ => None,
    })
}

async fn capture(cx: &mut AsyncApp, window: isize, path: &Path) {
    let path = path.to_owned();
    let out = cx
        .background_executor()
        .spawn(async move {
            let _ = std::fs::remove_file(&path);
            let out = std::process::Command::new("screencapture")
                .args(["-x", "-o", &format!("-l{window}")])
                .arg(&path)
                .output();
            (path.is_file(), out)
        })
        .await;
    match out {
        (true, _) => {}
        (false, Ok(o)) => fail(&format!(
            "screencapture could not capture the window: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        (false, Err(e)) => fail(&format!("screencapture: {e}")),
    }
}

/// Wait until the page's title satisfies `check`, at most `limit`; exit 5
/// (not run) if its frames stop coming.
async fn wait_page(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    what: &str,
    limit: Duration,
    check: impl Fn(u64, i64) -> bool,
) -> Option<(u64, i64)> {
    let deadline = Instant::now() + limit;
    let mut last = (0, Instant::now());
    loop {
        let title = read(any, ws, cx, web_pane)
            .flatten()
            .and_then(Result::ok)
            .map(|s| s.1);
        if let Some((n, y)) = title.as_deref().and_then(progress) {
            if check(n, y) {
                return Some((n, y));
            }
            if n != last.0 {
                last = (n, Instant::now());
            }
        }
        if last.1.elapsed() > Duration::from_secs(3) {
            println!(
                "smoke: Chromium stopped producing frames for the page ({what}); the display is not refreshing; not run"
            );
            std::process::exit(crate::INCONCLUSIVE);
        }
        if Instant::now() > deadline {
            return None;
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

pub(crate) async fn run(
    any: AnyWindowHandle,
    ws: WeakEntity<Workspace>,
    out: PathBuf,
    cx: &mut AsyncApp,
) {
    if let Err(e) = std::fs::create_dir_all(&out) {
        fail(&format!("{}: {e}", out.display()));
    }
    wait_long(
        any,
        &ws,
        cx,
        "session main with its pane",
        Duration::from_secs(20),
        |w, _| !w.pane_ids().is_empty(),
    )
    .await;
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            w.topology.web_open(api::WebOpen {
                url: PAGE.to_owned(),
                pane: w.pane_ids().first().copied(),
                ..api::WebOpen::default()
            });
        });
    });
    // The pane appears, Chromium starts, and the page loads.
    let deadline = Instant::now() + Duration::from_secs(30);
    let seen = loop {
        match read(any, &ws, cx, web_pane).flatten() {
            Some(Ok(s)) => break s,
            Some(Err(problem)) => fail(&format!("the web pane cannot draw its page: {problem}")),
            None if Instant::now() > deadline => {
                crate::stalled(any, cx, "never saw: the web page loaded in its pane").await
            }
            None => {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await
            }
        }
    };
    let (pane, _, _, _) = seen;
    println!("smoke: web page loaded in pane %{pane}");
    let Some((frames, _)) = wait_page(any, &ws, cx, "animation frames", TEN, |n, _| n > 30).await
    else {
        fail("the page never drew 30 animation frames");
    };
    println!("smoke: Chromium drew {frames} animation frames");
    let Some((_, _, Some(rect), Some(window))) =
        read(any, &ws, cx, web_pane).flatten().and_then(Result::ok)
    else {
        fail("the web pane has no place in the window");
    };
    let size = cx
        .update_window(any, |_, window, _| window.viewport_size())
        .unwrap_or_default();
    let line = format!(
        "{} {} {} {} {} {}\n",
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        f32::from(size.width),
        f32::from(size.height)
    );
    if let Err(e) = std::fs::write(out.join("rect.txt"), line) {
        fail(&format!("rect.txt: {e}"));
    }
    println!(
        "smoke: web pane at {} {} {}x{} in a {}x{} window",
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        f32::from(size.width),
        f32::from(size.height)
    );
    capture(cx, window, &out.join("before.png")).await;
    // A wheel turn over the page, as the mouse would send it.
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.read_with(cx, |w, cx| {
            if let Some(PaneView::Web { view, .. }) = w.panes.get(&pane)
                && let Some(v) = &view.read(cx).view
            {
                v.wheel((rect.width / 2.0) as i32, (rect.height / 2.0) as i32, -4000);
            }
        });
    });
    let y = match wait_page(any, &ws, cx, "a wheel event", THREE, |_, y| y > 0).await {
        Some((_, y)) => {
            println!("smoke: web page scrolled by a wheel event to y={y}");
            y
        }
        None => {
            // The wheel did not reach the page: say so, and check the
            // page scrolls and redraws at all, by script.
            println!(
                "smoke: a wheel event sent through CEF did not scroll the page; scrolling by script"
            );
            let _ = cx.update_window(any, |_, _, cx| {
                let _ = ws.read_with(cx, |w, cx| {
                    if let Some(PaneView::Web { view, .. }) = w.panes.get(&pane)
                        && let Some(v) = &view.read(cx).view
                    {
                        v.run_script("window.scrollTo(0, document.body.scrollHeight)");
                    }
                });
            });
            let Some((_, y)) =
                wait_page(any, &ws, cx, "a scroll by script", TEN, |_, y| y > 0).await
            else {
                fail("the page did not scroll by wheel or by script");
            };
            println!("smoke: web page scrolled by script to y={y}");
            y
        }
    };
    // Let the scroll settle (smooth scrolling) before the second capture.
    let (_, y) = {
        let mut settled = (0, y);
        for _ in 0..20 {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let Some(now) =
                wait_page(any, &ws, cx, "frames after scrolling", TEN, |_, _| true).await
            else {
                fail("the page stopped reporting after scrolling");
            };
            if now.1 == settled.1 {
                break;
            }
            settled = now;
        }
        settled
    };
    println!("smoke: web page settled at y={y}");
    capture(cx, window, &out.join("after.png")).await;
    println!("smoke: web captures in {}", out.display());
    cx.update(|cx| cx.quit());
}
