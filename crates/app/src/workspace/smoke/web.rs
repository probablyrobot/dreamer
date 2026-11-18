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
//! Last, input, delivered through AppKit as a person's would be: a click
//! in the page's text field focuses the pane; keys and an input method's
//! composition type into the field (its value read back through
//! DevTools); Cmd+J shows the harness drawer (a shell here) from inside
//! the page and hides it again, focus going back to the page; and Cmd+A
//! selects the whole field through Chromium's own edit command. Copy,
//! cut, and paste are not pressed: they would use the system pasteboard,
//! which a check never touches. A window that is not the key window
//! cannot be typed into: exit 5.
//!
//! Then an app's own page: the example web app (`examples/apps/panes-web`,
//! granted `tmux.topology`) opens beside the shell pane. Its page lists
//! the panes through the bridge, and a call through it lists them again;
//! a call outside its grant is refused; a frame of another origin (a
//! sandboxed frame in it) has no `window.dreamer` and cannot reach the
//! bridge; a navigation to an origin outside its allow-list is refused;
//! and the plain web page has no `window.dreamer` either. Each passed check
//! is a `smoke: web app:` line.
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

/// The page: one screen of the top color, then three of the bottom color,
/// and a small text field that stays at its top left. Its title counts
/// animation frames (Chromium's compositor is drawing) and reports the
/// scroll position.
pub(crate) const PAGE: &str = "data:text/html,<html><body style='margin:0'>\
<input id=a style='position:fixed;left:16px;top:16px;width:160px;height:24px'>\
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

    input(any, &ws, cx, pane).await;
    app_page(any, &ws, cx, pane, &out, window).await;

    if let Err(e) = std::fs::write(out.join("checks.txt"), &checks.0) {
        fail(&format!("checks.txt: {e}"));
    }
    println!("smoke: web captures in {}", out.display());
    cx.update(|cx| cx.quit());
}

/// What the input checks type into the page: keys, then an input method
/// composing "日" and committing "日本".
const TYPED: &str = "hi日本";

/// macOS virtual key codes.
const KEY_A: u16 = 0x00;
const KEY_H: u16 = 0x04;
const KEY_X: u16 = 0x07;
const KEY_I: u16 = 0x22;
const KEY_J: u16 = 0x26;
const KEY_DELETE: u16 = 0x33;

/// Use pane `pane`'s page, on the UI thread.
fn with_view<T>(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    f: impl FnOnce(&web_engine::WebView) -> T,
) -> T {
    read(any, ws, cx, |w, cx| {
        let page = w.panes.get(&pane)?.pages(cx).into_iter().next()?;
        page.read(cx).view.as_ref().map(f)
    })
    .flatten()
    .unwrap_or_else(|| fail("the web page is gone"))
}

/// Press a key (with Command or not) as a person would, through AppKit.
fn press(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    keys: &[(u16, &str, bool)],
) -> Result<(), String> {
    for &(code, chars, command) in keys {
        with_view(any, ws, cx, pane, |v| v.press_key(code, chars, command))
            .map_err(|e| format!("a key press: {e}"))?;
    }
    Ok(())
}

/// The value of `expression` in the page, through DevTools.
async fn eval(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    expression: &str,
) -> Option<serde_json::Value> {
    let (tx, rx) = async_channel::bounded(1);
    with_view(any, ws, cx, pane, |v| {
        v.evaluate(expression, move |value| {
            let _ = tx.try_send(value);
        });
    });
    let timer = cx.background_executor().timer(THREE);
    futures_lite::future::or(async { rx.recv().await.ok().flatten() }, async {
        timer.await;
        None
    })
    .await
}

/// Wait until `expression` in the page has the value `want`, at most ten
/// seconds; the value last seen if it never does.
async fn wait_value(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    expression: &str,
    want: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + TEN;
    loop {
        let got = eval(any, ws, cx, pane, expression).await;
        if got.as_ref().and_then(serde_json::Value::as_str) == Some(want) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("`{expression}` is {got:?}, not {want:?}"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

/// Whether pane `pane` is Dreamer's focused pane and its page has the
/// window's keys, as `want`.
fn page_focus(w: &Workspace, cx: &gpui::App, pane: u32, want: bool) -> bool {
    let page = w
        .panes
        .get(&pane)
        .and_then(PaneView::web_pane)
        .is_some_and(|p| p.read(cx).page_has_focus());
    (w.focused == Some(pane)) == want && page == want
}

/// A click focuses the page, keys and an input method type into it,
/// Dreamer's own shortcut works from inside it, and copy and paste are
/// Chromium's (see the module's description).
async fn input(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp, pane: u32) {
    let key = cx
        .update_window(any, |_, window, _| {
            window.activate_window();
            window.is_window_active()
        })
        .unwrap_or(false);
    if !key {
        println!("smoke: the window is not the key window; it cannot be typed into; not run");
        std::process::exit(crate::INCONCLUSIVE);
    }
    // Start from the shell pane, so the click has to move focus.
    let Some(shell) = read(any, ws, cx, |w, _| {
        w.pane_ids().into_iter().find(|p| *p != pane)
    })
    .flatten() else {
        fail("no shell pane beside the web pane");
    };
    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| w.click_pane(shell, window, cx));
    });
    wait_long(any, ws, cx, "the shell pane focused", TEN, move |w, cx| {
        w.focused == Some(shell) && page_focus(w, cx, pane, false)
    })
    .await;
    // The first field is at 16,16 (160x24) in the page.
    if let Err(e) = with_view(any, ws, cx, pane, |v| v.click(60.0, 28.0)) {
        fail(&format!("a click in the page: {e}"));
    }
    wait_long(
        any,
        ws,
        cx,
        "the clicked page focused",
        TEN,
        move |w, cx| page_focus(w, cx, pane, true),
    )
    .await;
    if let Err(e) = wait_value(any, ws, cx, pane, "document.activeElement.id", "a").await {
        fail(&format!("the click did not focus the field: {e}"));
    }
    println!("smoke: web input: a click in the page focused its pane and the field");

    // Keys, a correction, then an input method's composition.
    press(
        any,
        ws,
        cx,
        pane,
        &[
            (KEY_H, "h", false),
            (KEY_I, "i", false),
            (KEY_X, "x", false),
            (KEY_DELETE, "\u{7f}", false),
        ],
    )
    .unwrap_or_else(|e| fail(&e));
    if let Err(e) = with_view(any, ws, cx, pane, |v| v.compose("日", "日本")) {
        fail(&format!("the input method: {e}"));
    }
    let a = "document.getElementById('a').value";
    if let Err(e) = wait_value(any, ws, cx, pane, a, TYPED).await {
        fail(&format!("typing into the page: {e}"));
    }
    println!("smoke: web input: keys and an input method composition typed {TYPED:?}");

    // Cmd+J from inside the page shows the drawer and focuses it; again,
    // it hides and the page has the keys back.
    let drawer = |w: &Workspace| {
        let session = w.session?;
        w.current
            .as_ref()?
            .panes
            .iter()
            .find(|p| p.drawer_of == Some(session))
            .map(|p| p.id)
    };
    press(any, ws, cx, pane, &[(KEY_J, "j", true)]).unwrap_or_else(|e| fail(&e));
    wait_long(
        any,
        ws,
        cx,
        "the drawer shown and focused by Cmd+J from the page",
        Duration::from_secs(20),
        move |w, cx| {
            drawer(w).is_some_and(|d| w.focused == Some(d) && w.panes.contains_key(&d))
                && page_focus(w, cx, pane, false)
        },
    )
    .await;
    println!("smoke: web input: Cmd+J in the page showed and focused the drawer");
    press(any, ws, cx, pane, &[(KEY_J, "j", true)]).unwrap_or_else(|e| fail(&e));
    wait_long(
        any,
        ws,
        cx,
        "the drawer hidden and the page focused again",
        TEN,
        move |w, cx| {
            drawer(w).is_none_or(|d| !w.panes.contains_key(&d)) && page_focus(w, cx, pane, true)
        },
    )
    .await;
    println!("smoke: web input: Cmd+J hid the drawer; the page has the keys again");

    // Select all becomes Chromium's own edit command: the caret at the
    // field's end becomes the whole field selected. Copy, cut, and paste
    // take the same path but use the system pasteboard, which the check
    // leaves alone.
    let selection = "(f => document.activeElement.id + ' ' + f.selectionStart + ',' + f.selectionEnd)\
                     (document.getElementById('a'))";
    let n = TYPED.encode_utf16().count();
    let (caret, all) = (format!("a {n},{n}"), format!("a 0,{n}"));
    if let Err(e) = wait_value(any, ws, cx, pane, selection, &caret).await {
        fail(&format!("the field before select all: {e}"));
    }
    press(any, ws, cx, pane, &[(KEY_A, "a", true)]).unwrap_or_else(|e| fail(&e));
    if let Err(e) = wait_value(any, ws, cx, pane, selection, &all).await {
        fail(&format!("select all in the page: {e}"));
    }
    println!("smoke: web input: Cmd+A selected the whole field through Chromium's SelectAll");
}

/// The example web app, packaged: its manifest and its page.
const APP_TOML: &str = include_str!("../../../../../examples/apps/panes-web/app.toml");
const APP_HTML: &str = include_str!("../../../../../examples/apps/panes-web/web/index.html");

/// A frame of another origin in the app's page (sandboxed, so opaque):
/// whether it has `window.dreamer`, and what a request to the bridge gets.
const OTHER_ORIGIN: &str = r#"new Promise((done) => {
  addEventListener("message", (e) => done(String(e.data)), { once: true });
  const f = document.createElement("iframe");
  f.sandbox = "allow-scripts";
  f.srcdoc = "<script>fetch('/.dreamer/call', { method: 'POST', body: JSON.stringify({ method: 'tmux.topology' }) })" +
    ".then((r) => r.status, () => 'failed')" +
    ".then((s) => parent.postMessage(typeof window.dreamer + ' ' + s, '*'))<\/script>";
  document.body.append(f);
})"#;

/// The example web app beside the shell pane, under a grant of
/// `tmux.topology` (see the module's description). `web` is the plain
/// web pane.
async fn app_page(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    web: u32,
    out: &Path,
    window: isize,
) {
    let (socket, session, shell) = read(any, ws, cx, |w, _| {
        let shell = w.pane_ids().into_iter().find(|p| *p != web)?;
        Some((w.socket.clone(), w.session?, shell))
    })
    .flatten()
    .unwrap_or_else(|| fail("no session and shell pane for the web app"));
    let dir = std::env::temp_dir().join(format!("dreamer-smoke-web-app-{}", std::process::id()));
    let written = std::fs::create_dir_all(dir.join("web"))
        .and_then(|()| std::fs::write(dir.join("app.toml"), APP_TOML))
        .and_then(|()| std::fs::write(dir.join("web/index.html"), APP_HTML));
    if let Err(e) = written {
        fail(&format!("could not write the web app's package: {e}"));
    }
    let termapp = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("dreamer-termapp")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| fail("no dreamer-termapp beside dreamer"));
    let argv = vec![
        termapp.display().to_string(),
        dir.join("app.toml").display().to_string(),
    ];
    let launched = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let c = client::Client::connect(&socket, "smoke")
                .await
                .map_err(|e| e.to_string())?;
            match c
                .call(api::Request::AppsLaunch(api::LaunchApp {
                    app_id: "dreamer.panes-web".into(),
                    grant: Some(api::Grant {
                        methods: vec![api::method::TMUX_TOPOLOGY.into()],
                        ..api::Grant::default()
                    }),
                    argv,
                    session,
                    split: Some(shell),
                    ..api::LaunchApp::default()
                }))
                .await
            {
                Ok(api::Response::AppsLaunch(l)) => Ok(l.pane),
                Ok(_) => Err("unexpected answer".to_owned()),
                Err(e) => Err(e.to_string()),
            }
        })
    })
    .join()
    .unwrap_or_else(|_| Err("the launch thread panicked".into()));
    let pane = launched.unwrap_or_else(|e| fail(&format!("could not launch the web app: {e}")));
    let origin = "dreamer-app://dreamer.panes-web";
    wait_long(
        any,
        ws,
        cx,
        "the web app's page loaded in its pane",
        Duration::from_secs(30),
        move |w, cx| {
            let Some(PaneView::App { view, .. }) = w.panes.get(&pane) else {
                return false;
            };
            view.read(cx).pages().first().is_some_and(|p| {
                let p = p.read(cx);
                p.url == format!("{origin}/index.html")
                    && p.view.as_ref().is_some_and(|v| v.load_status().is_some())
            })
        },
    )
    .await;
    println!("smoke: the web app's page loaded in app pane %{pane}");

    // The page lists the panes itself, through its bridge.
    let status = "document.getElementById('status').textContent";
    let deadline = Instant::now() + TEN;
    loop {
        let shown = eval(any, ws, cx, pane, status).await;
        let text = shown
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if text.ends_with(" panes") {
            println!("smoke: web app: its page lists the panes through the bridge ({text})");
            break;
        }
        if Instant::now() > deadline {
            fail(&format!(
                "the web app's page never listed the panes: {shown:?}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    let listed = eval(
        any,
        ws,
        cx,
        pane,
        "window.dreamer.call('tmux.topology', {}).then((t) => t.panes.some((p) => p.id === ID))"
            .replace("ID", &pane.to_string())
            .as_str(),
    )
    .await;
    if listed != Some(serde_json::Value::Bool(true)) {
        fail(&format!(
            "tmux.topology through the bridge did not list the app's own pane: {listed:?}"
        ));
    }
    println!("smoke: web app: tmux.topology through the bridge lists pane %{pane}");
    capture(cx, window, &out.join("app.png")).await;

    // Outside the grant: refused, with the daemon's code.
    let refused = eval(
        any,
        ws,
        cx,
        pane,
        "window.dreamer.call('terminal.list', {}).then(() => 'allowed', (e) => 'refused ' + e.code + ' ' + e.message)",
    )
    .await;
    let refused = refused
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !refused.starts_with(&format!("refused {}", api::code::FORBIDDEN)) {
        fail(&format!(
            "terminal.list, outside the app's grant, was not refused: {refused:?}"
        ));
    }
    println!("smoke: web app: terminal.list, outside its grant, is refused ({refused})");

    // Another origin, inside the page: no bridge.
    let other = eval(any, ws, cx, pane, OTHER_ORIGIN).await;
    let other = other
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !other.starts_with("undefined ") || other.ends_with(" 200") {
        fail(&format!(
            "a frame of another origin reached the bridge: {other:?}"
        ));
    }
    let plain = eval(any, ws, cx, web, "typeof window.dreamer").await;
    if plain.as_ref().and_then(serde_json::Value::as_str) != Some("undefined") {
        fail(&format!("the plain web page has window.dreamer: {plain:?}"));
    }
    println!(
        "smoke: web app: another origin has no bridge (a sandboxed frame: {other}; the plain web page: undefined)"
    );

    // A navigation outside the allow-list is refused; the page stays.
    let blocked = "https://example.invalid/";
    let _ = eval(
        any,
        ws,
        cx,
        pane,
        &format!("location.href = '{blocked}'; 'asked'"),
    )
    .await;
    let deadline = Instant::now() + TEN;
    loop {
        let refused = read(any, ws, cx, move |w, cx| {
            let Some(PaneView::App { view, .. }) = w.panes.get(&pane) else {
                return None;
            };
            let page = view.read(cx).pages().into_iter().next()?;
            Some(page.read(cx).blocked())
        })
        .flatten()
        .unwrap_or_default();
        if refused.iter().any(|u| u == blocked) {
            break;
        }
        if Instant::now() > deadline {
            fail(&format!(
                "the navigation to {blocked} was not refused: {refused:?}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    if let Err(e) = wait_value(any, ws, cx, pane, "location.origin", origin).await {
        fail(&format!("the page left its origin: {e}"));
    }
    println!("smoke: web app: a navigation to {blocked}, outside its allow-list, is refused");
    let _ = std::fs::remove_dir_all(&dir);
}
