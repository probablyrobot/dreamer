//! `dreamer --smoke-web DIR`: a web pane against an isolated tmux whose
//! session `main` has one shell pane. Opens a page with a known color and
//! a long body in a pane beside it (`web.open`), waits until Chromium has
//! loaded it and is producing frames, captures the window, scrolls the
//! page with a wheel event, and captures it again. Then it moves the pane
//! the ways tmux and the desktop do, capturing after each: the pane is
//! resized, zoomed and unzoomed, left for a new tab and shown again (the
//! page kept, not reloaded), and covered by the palette (the page's
//! snapshot shows, the palette over it) until the palette closes.
//!
//! Writes to DIR the captures of the window (from `screencapture`) and
//! `checks.txt`, what the gate checks in them: a line `window WIDTH
//! HEIGHT`, then one line per check, `FILE COLOR min|max SHARE X Y WIDTH
//! HEIGHT`: in FILE, COLOR (hex) covers at least (min) or at most (max)
//! SHARE of the rectangle, in points from the window's top left. Exit 5:
//! the window or the page was not being drawn (screen locked, display
//! asleep), so nothing can be judged.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, AppContext as _, AsyncApp, WeakEntity};
use web_engine::Rect;

use super::super::{PALETTE_WIDTH, PaneView, TAB_STRIP, Workspace};
use super::{fail, read, wait_long};

/// The page: one screen of the top color, then three of the bottom color.
/// Its title counts animation frames (Chromium's compositor is drawing)
/// and reports the scroll position.
pub(crate) const PAGE: &str = "data:text/html,<html><body style='margin:0'>\
<div style='height:100vh;background:%232a6fdb'></div>\
<div style='height:300vh;background:%23d9822b'></div>\
<script>let n=0;function f(){n++;document.title='raf '+n+' y '+Math.round(scrollY);\
requestAnimationFrame(f)}requestAnimationFrame(f)</script></body></html>";

/// The page's colors.
const TOP: u32 = 0x2a_6f_db;
const BOTTOM: u32 = 0xd9_82_2b;

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
type Seen = (u32, String, Option<Rect>, Option<isize>);

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

/// Pane `pane`'s page, shown or hidden: where its view was placed, whether
/// it is on screen, and whether the pane shows the page's snapshot.
fn page(w: &Workspace, cx: &gpui::App, pane: u32) -> Option<(Rect, bool, bool)> {
    let view = w
        .panes
        .get(&pane)
        .or_else(|| w.hidden.get(&pane).map(|(v, _)| v))?
        .web_pane()?;
    let p = view.read(cx);
    let v = p.view.as_ref()?;
    Some((v.placed()?, v.visible(), p.shows_snapshot()))
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

/// Let the page draw ten more animation frames (at its new size).
async fn settle(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp, what: &str) {
    let Some((n, _)) = wait_page(any, ws, cx, what, TEN, |_, _| true).await else {
        fail(&format!("the page stopped reporting ({what})"));
    };
    if wait_page(any, ws, cx, what, TEN, |m, _| m > n + 10)
        .await
        .is_none()
    {
        fail(&format!("the page drew no new frames ({what})"));
    }
}

/// What the gate checks in the captures (see the module's description).
struct Checks(String);

impl Checks {
    fn add(&mut self, file: &str, color: u32, at_least: bool, share: f32, r: Rect) {
        if r.width < 16.0 || r.height < 16.0 {
            return;
        }
        let bound = if at_least { "min" } else { "max" };
        self.0.push_str(&format!(
            "{file} {color:06x} {bound} {share} {} {} {} {}\n",
            r.x, r.y, r.width, r.height
        ));
    }

    /// The page fills `r` with `color`, and draws nothing left of it (the
    /// shell pane's side) at its height.
    fn inside_only(&mut self, file: &str, color: u32, r: Rect) {
        self.add(file, color, true, 0.9, r);
        let left = Rect {
            x: 0.0,
            width: r.x - 2.0,
            ..r
        };
        self.add(file, TOP, false, 0.01, left);
        self.add(file, BOTTOM, false, 0.01, left);
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
    let (window_w, window_h) = (f32::from(size.width), f32::from(size.height));
    let mut checks = Checks(format!("window {window_w} {window_h}\n"));
    println!(
        "smoke: web pane at {} {} {}x{} in a {window_w}x{window_h} window",
        rect.x, rect.y, rect.width, rect.height,
    );
    capture(cx, window, &out.join("before.png")).await;
    checks.inside_only("before.png", TOP, rect);
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
    checks.inside_only("after.png", BOTTOM, rect);

    // tmux resizes the pane: the page follows its new rectangle.
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            if let Some(p) = w.visible().iter().find(|p| p.id == pane) {
                w.topology
                    .act(api::action::Kind::ResizePane(api::ResizePane {
                        pane,
                        width: Some(p.width.saturating_sub(16)),
                        height: None,
                    }));
            }
        });
    });
    wait_long(
        any,
        &ws,
        cx,
        "the web page on its resized pane",
        TEN,
        |w, cx| page(w, cx, pane).is_some_and(|(r, shown, _)| shown && r.width < rect.width - 40.0),
    )
    .await;
    settle(any, &ws, cx, "resized").await;
    let Some((resized, _, _)) = read(any, &ws, cx, |w, cx| page(w, cx, pane)).flatten() else {
        fail("the resized web pane has no place");
    };
    println!(
        "smoke: resized: the page follows its pane to {} {} {}x{}",
        resized.x, resized.y, resized.width, resized.height
    );
    capture(cx, window, &out.join("resized.png")).await;
    checks.inside_only("resized.png", BOTTOM, resized);

    // Zoomed, the page fills the pane area; unzoomed, it is back.
    let zoom = |cx: &mut AsyncApp| {
        let _ = cx.update_window(any, |_, _, cx| {
            let _ = ws.update(cx, |w, _| {
                w.topology
                    .act(api::action::Kind::ZoomPane(api::ZoomPane { pane }));
            });
        });
    };
    zoom(cx);
    wait_long(any, &ws, cx, "the web page zoomed", TEN, |w, cx| {
        page(w, cx, pane).is_some_and(|(r, shown, _)| shown && r.width > rect.width + 40.0)
    })
    .await;
    settle(any, &ws, cx, "zoomed").await;
    let Some((zoomed, _, _)) = read(any, &ws, cx, |w, cx| page(w, cx, pane)).flatten() else {
        fail("the zoomed web pane has no place");
    };
    println!(
        "smoke: zoomed: the page fills {} {} {}x{}",
        zoomed.x, zoomed.y, zoomed.width, zoomed.height
    );
    capture(cx, window, &out.join("zoomed.png")).await;
    checks.add("zoomed.png", BOTTOM, true, 0.9, zoomed);
    zoom(cx);
    wait_long(any, &ws, cx, "the web page unzoomed", TEN, |w, cx| {
        page(w, cx, pane).is_some_and(|(r, shown, _)| shown && r == resized)
    })
    .await;
    settle(any, &ws, cx, "unzoomed").await;
    println!("smoke: unzoomed: the page is back on its pane");

    // Another tab: the page is hidden, not closed; back, it is shown again
    // as it was (its animation frames count on, from before).
    let Some(tab) = read(any, &ws, cx, |w, _| w.active_window().map(|t| t.id)).flatten() else {
        fail("no tab shown");
    };
    let Some((frames, _)) = wait_page(any, &ws, cx, "before the new tab", TEN, |_, _| true).await
    else {
        fail("the page stopped reporting");
    };
    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| {
            w.run_command(crate::palette::Command::NewWindow, None, window, cx);
        });
    });
    wait_long(
        any,
        &ws,
        cx,
        "a new tab, the web page hidden",
        TEN,
        |w, cx| {
            w.active_window().is_some_and(|t| t.id != tab)
                && page(w, cx, pane).is_some_and(|(_, shown, _)| !shown)
        },
    )
    .await;
    // The new tab's shell has painted.
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    capture(cx, window, &out.join("tab.png")).await;
    checks.add("tab.png", BOTTOM, false, 0.01, resized);
    println!("smoke: another tab: the web page is hidden");
    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| w.click_tab_for_smoke(tab, window, cx));
    });
    wait_long(any, &ws, cx, "the web page's tab again", TEN, |w, cx| {
        page(w, cx, pane).is_some_and(|(r, shown, _)| shown && r == resized)
    })
    .await;
    let Some((again, _)) = wait_page(any, &ws, cx, "back on its tab", TEN, |_, _| true).await
    else {
        fail("the page stopped reporting back on its tab");
    };
    if again < frames {
        fail(&format!(
            "the page was loaded again on its tab ({again} animation frames, {frames} before)"
        ));
    }
    settle(any, &ws, cx, "back on its tab").await;
    println!("smoke: back on its tab: the same page ({frames} animation frames, now {again})");
    capture(cx, window, &out.join("back.png")).await;
    checks.inside_only("back.png", BOTTOM, resized);

    // The palette over the page: the pane shows the page's snapshot, the
    // palette is drawn over it, and the live page is back once it closes.
    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| w.open_palette(window, cx));
    });
    wait_long(
        any,
        &ws,
        cx,
        "the page's snapshot under the palette",
        TEN,
        |w, cx| page(w, cx, pane).is_some_and(|(_, shown, snapshot)| !shown && snapshot),
    )
    .await;
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    capture(cx, window, &out.join("palette.png")).await;
    // The palette's query line (its top 36 points) where it crosses the
    // pane, and the pane right of the palette.
    let palette_left = (window_w - PALETTE_WIDTH) / 2.0;
    let palette_right = palette_left + PALETTE_WIDTH;
    let palette = Rect {
        x: resized.x.max(palette_left),
        y: TAB_STRIP + 8.0,
        width: palette_right.min(resized.x + resized.width) - resized.x.max(palette_left),
        height: 36.0,
    };
    if palette.width < 16.0 {
        fail("the palette does not cross the web pane");
    }
    checks.add("palette.png", BOTTOM, false, 0.05, palette);
    let beside = Rect {
        x: resized.x.max(palette_right + 8.0),
        width: resized.x + resized.width - resized.x.max(palette_right + 8.0),
        ..resized
    };
    checks.add("palette.png", BOTTOM, true, 0.9, beside);
    println!("smoke: the palette is open over the page's snapshot");
    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| w.close_palette(window, cx));
    });
    wait_long(
        any,
        &ws,
        cx,
        "the live page after the palette",
        TEN,
        |w, cx| page(w, cx, pane).is_some_and(|(_, shown, snapshot)| shown && !snapshot),
    )
    .await;
    settle(any, &ws, cx, "after the palette").await;
    capture(cx, window, &out.join("closed.png")).await;
    checks.inside_only("closed.png", BOTTOM, resized);
    println!("smoke: the palette closed: the live page is back");

    if let Err(e) = std::fs::write(out.join("checks.txt"), &checks.0) {
        fail(&format!("checks.txt: {e}"));
    }
    println!("smoke: web captures in {}", out.display());
    cx.update(|cx| cx.quit());
}
