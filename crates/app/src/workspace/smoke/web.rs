//! `dreamer --smoke-web-install` first, in a desktop of its own with an
//! empty data directory: Dreamer Web is added as a person would add it
//! ([`install`]). Each passed check is a `smoke: web install:` line.
//!
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
//! cannot be typed into or clicked: exit 5.
//!
//! Then an app's own page: the example web app (`examples/apps/panes-web`,
//! granted `tmux.topology`) opens beside the shell pane. Its page lists
//! the panes through the bridge, and a call through it lists them again;
//! a call outside its grant is refused; a frame of another origin (a
//! sandboxed frame in it) has no `window.dreamer` and cannot reach the
//! bridge; a navigation to an origin outside its allow-list is refused;
//! the plain web page has no `window.dreamer` either; and a download its
//! page starts is refused (it lacks `web.download`), with a status line
//! and no file. Each passed check is a `smoke: web app:` line.
//!
//! Then the browser, in the plain web pane, against a test site this
//! the gate serves on 127.0.0.1: `web.navigate` loads its login (a session
//! cookie) in the pane; Cmd+L pulls the address bar down, an address
//! typed without a scheme and Enter load the site; a link is followed;
//! Cmd+[ and Cmd+] go back and forward, and so do the mouse's side
//! buttons; Cmd+R loads the page again; Cmd+F finds words in it
//! (Chromium reports the matches) and Esc ends that; Cmd+= and Cmd+0
//! zoom; a download asks first, in a row over the page; Save writes the
//! file, its contents the site's, into the downloads folder (the gate's
//! `web.downloads.folder`, a private temporary directory: the smoke
//! refuses to run with a folder in the home directory); Cancel at the
//! prompt writes nothing; Cancel while a slow download is written leaves
//! no partial file; and a link that wants a new window opens in the same
//! pane. Each passed check is a `smoke: web browser:` line. `--smoke-web-restart` then runs in a new process with
//! the same profile, and checks that the login's session cookie is still
//! sent ([`restart`]).
//!
//! The web app's checks also include that its bridge is refused
//! `web.read` (its grant has no `web.inspect`), and that `apps.capture` of
//! its pane, as `dream app preview --png` asks, is its page's own image
//! (written as `app-page.png`).
//!
//! Last, an agent and a page, through the daemon's `web.*` methods on
//! connections narrowed to an agent's grant at `own`: the agent opens the
//! test site's form in a web pane of its own (`web.open`); without
//! `web.inspect` in the grant, `web.read` is refused; with it, `web.read`
//! reads the form's text and finds its field in the accessibility tree,
//! `web.type` fills the field by its node and presses Enter (the site
//! records the name sent), `web.click` follows a link by its selector, and
//! `web.screenshot` writes the page (`web-inspect.png`). Each passed check
//! is a `smoke: web inspect:` line.
//!
//! `--smoke-web-color DIR` runs in a process of its own, with `web.color`
//! as the gate wrote it ([`color`]): Display P3's red, live and as its
//! snapshot under the palette. Each passed check is a `smoke: web color:`
//! line.
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
            if let Some(problem) = p.not_drawn() {
                return Some(Err(problem));
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
            Some(Err(problem)) if problem != crate::web::BEING_ADDED => {
                fail(&format!("the web pane cannot draw its page: {problem}"))
            }
            _ if Instant::now() > deadline => {
                crate::stalled(any, cx, "never saw: the web page loaded in its pane").await
            }
            _ => {
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
    browser(any, &ws, cx, pane).await;
    inspect(any, &ws, cx, &out).await;

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
    // The window's focus listeners (the page's pane losing it) run as it
    // next draws: let them, before the click.
    let drawn = terminal_render::paint_count();
    let deadline = Instant::now() + TEN;
    while terminal_render::paint_count() < drawn + 2 && Instant::now() < deadline {
        let _ = cx.update_window(any, |_, window, _| window.refresh());
        cx.background_executor()
            .timer(Duration::from_millis(20))
            .await;
    }
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
    // The keys are queued as a person's are; the composition is not.
    let a = "document.getElementById('a').value";
    if let Err(e) = wait_value(any, ws, cx, pane, a, "hi").await {
        fail(&format!("typing into the page: {e}"));
    }
    if let Err(e) = with_view(any, ws, cx, pane, |v| v.compose("日", "日本")) {
        fail(&format!("the input method: {e}"));
    }
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
    let owner = Agent::start(socket.clone(), None);
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

    // Reading pages is not the bridge's without web.inspect in the grant.
    let inspect = eval(
        any,
        ws,
        cx,
        pane,
        &format!("window.dreamer.call('web.read', {{ pane: {web} }}).then(() => 'allowed', (e) => 'refused ' + e.code)"),
    )
    .await;
    if inspect.as_ref().and_then(serde_json::Value::as_str)
        != Some(format!("refused {}", api::code::FORBIDDEN).as_str())
    {
        fail(&format!(
            "web.read through the bridge, without web.inspect, was not refused: {inspect:?}"
        ));
    }
    println!("smoke: web app: web.read through its bridge, without web.inspect, is refused");

    // Its image, as `dream app preview --png` takes it: the page's own,
    // from Chromium.
    let shot = out.join("app-page.png");
    let captured = owner
        .call(api::Request::AppsCapture(api::AppCapture {
            pane,
            path: shot.display().to_string(),
            seq: 0,
            wait_ms: Some(20_000),
        }))
        .await;
    match captured {
        Ok(api::Response::AppsCapture(c)) if c.width > 100 && c.height > 100 && shot.is_file() => {
            println!(
                "smoke: web app: apps.capture took its page's image from Chromium ({}x{})",
                c.width, c.height
            );
        }
        other => fail(&format!("apps.capture of the web app's pane: {other:?}")),
    }

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

    // A download in its page is refused: the app does not hold
    // web.download. Nothing asks, and nothing is written.
    let folder = downloads_folder();
    let before = listing(cx, &folder).await;
    let _ = eval(
        any,
        ws,
        cx,
        pane,
        "(() => { const a = document.createElement('a'); \
         a.href = URL.createObjectURL(new Blob(['app'])); a.download = 'app-report.bin'; \
         document.body.appendChild(a); a.click(); return 'clicked'; })()",
    )
    .await;
    let deadline = Instant::now() + TEN;
    let status = loop {
        let seen = page_bar(any, ws, cx, pane, |b| {
            (b.status.as_ref().map(|s| s.0.clone()), b.downloads.len())
        });
        if let Some((_, asked)) = &seen
            && *asked > 0
        {
            fail("a download in the web app's page, without web.download, asked to be saved");
        }
        if let Some((Some(s), _)) = seen
            && s.contains("Download refused")
        {
            break s;
        }
        if Instant::now() > deadline {
            fail("a download in the web app's page was not refused with a status line");
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    };
    if listing(cx, &folder).await != before {
        fail("the web app's refused download wrote to the downloads folder");
    }
    println!("smoke: web app: a download in its page, without web.download, is refused: {status}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The downloads folder the smoke saves to: the gate's private
/// `web.downloads.folder`. Never one in the home directory: the smoke
/// fails rather than write to the user's own Downloads.
fn downloads_folder() -> PathBuf {
    let configured = crate::live::with(|l| l.downloads.clone())
        .flatten()
        .unwrap_or_else(|| fail("no web.downloads.folder set for the smoke's downloads"));
    let folder = workspace_config::downloads::folder(Some(&configured)).unwrap_or_else(|| {
        fail(&format!(
            "web.downloads.folder {configured:?} is not a folder"
        ))
    });
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.is_some_and(|h| folder.starts_with(h)) {
        fail(&format!(
            "the smoke saves downloads only to a private temporary folder, not {}",
            folder.display()
        ));
    }
    folder
}

/// The names in `dir`, sorted (none when it is not there), read off the UI
/// thread.
async fn listing(cx: &mut AsyncApp, dir: &Path) -> Vec<String> {
    let dir = dir.to_path_buf();
    cx.background_executor()
        .spawn(async move {
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .map(|r| {
                    r.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        })
        .await
}

/// The bar of pane `pane`'s (first) page: a web pane's, or an app's.
fn page_bar<T>(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    f: impl FnOnce(&crate::web::Bar) -> T,
) -> Option<T> {
    read(any, ws, cx, |w, cx| {
        let page = w.panes.get(&pane)?.pages(cx).into_iter().next()?;
        Some(f(&page.read(cx).bar))
    })
    .flatten()
}

/// Wait until pane `pane`'s page asks to save a download named `name`;
/// its id and the row's text.
async fn wait_asked(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    name: &str,
) -> u32 {
    let deadline = Instant::now() + TEN;
    loop {
        let asked = page_bar(any, ws, cx, pane, |b| {
            b.downloads
                .iter()
                .find(|d| d.name == name && d.state == crate::web::Saving::Asked)
                .map(|d| d.id)
        })
        .flatten();
        if let Some(id) = asked {
            return id;
        }
        if Instant::now() > deadline {
            let shown = page_bar(any, ws, cx, pane, |b| format!("{:?}", b.downloads));
            fail(&format!("no prompt to save {name}: {shown:?}"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// Answer download `id` in pane `pane`'s page as its row's Save or Cancel
/// would.
fn answer(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    id: u32,
    save: bool,
) {
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, cx| {
            if let Some(p) = w
                .panes
                .get(&pane)
                .and_then(|v| v.pages(cx).into_iter().next())
            {
                p.update(cx, |p, cx| p.answer_download(id, save, cx));
            }
        });
    });
}

/// Wait until pane `pane`'s page has no download left, and the downloads
/// folder holds exactly `names`.
async fn wait_settled(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    folder: &Path,
    names: &[&str],
    what: &str,
) {
    let deadline = Instant::now() + TEN;
    loop {
        let pending = page_bar(any, ws, cx, pane, |b| b.downloads.len()).unwrap_or(0);
        let now = listing(cx, folder).await;
        if pending == 0 && now == names {
            return;
        }
        if Instant::now() > deadline {
            fail(&format!(
                "{what}: {pending} download(s) left, and the folder holds {now:?}, not {names:?}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

/// What the test site's download holds (`xtask/src/web.rs` serves it).
pub(crate) const DOWNLOAD_BODY: &str = "dreamer web smoke: a saved download\n";

/// The browser's test site (`cargo xtask` serves it on 127.0.0.1 for the
/// smoke and names it in `DREAMER_SMOKE_WEB_SITE`): its host and port.
fn site() -> String {
    std::env::var("DREAMER_SMOKE_WEB_SITE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fail("no test site for the browser checks (DREAMER_SMOKE_WEB_SITE)"))
}

/// macOS virtual key codes the browser checks press.
const KEY_L: u16 = 0x25;
const KEY_R: u16 = 0x0F;
const KEY_F: u16 = 0x03;
const KEY_LEFT_BRACKET: u16 = 0x21;
const KEY_RIGHT_BRACKET: u16 = 0x1E;
const KEY_EQUAL: u16 = 0x18;
const KEY_ZERO: u16 = 0x1D;
const KEY_RETURN: u16 = 0x24;
const KEY_ESCAPE: u16 = 0x35;

/// Pane `pane`'s page: its title and its address.
fn where_is(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
) -> (String, String) {
    with_view(any, ws, cx, pane, |v| (v.title(), v.url()))
}

/// Wait until pane `pane`'s page has a title `title` starts, at most ten
/// seconds; the title when it does.
async fn wait_title(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    title: &str,
    what: &str,
) -> String {
    let deadline = Instant::now() + TEN;
    loop {
        let (now, url) = where_is(any, ws, cx, pane);
        if now.starts_with(title) {
            return now;
        }
        if Instant::now() > deadline {
            fail(&format!(
                "{what}: the page is {now:?} at {url}, not {title:?}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// The browser's bar in pane `pane`.
fn bar<T>(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    f: impl FnOnce(&crate::web::Bar) -> T,
) -> Option<T> {
    read(any, ws, cx, |w, cx| {
        Some(f(&w.panes.get(&pane)?.web_pane()?.read(cx).bar))
    })
    .flatten()
}

/// Whether the browser's bar in pane `pane` is as `check` wants.
fn bar_now(
    w: &Workspace,
    cx: &gpui::App,
    pane: u32,
    check: impl Fn(&crate::web::Bar) -> bool,
) -> bool {
    w.panes
        .get(&pane)
        .and_then(PaneView::web_pane)
        .is_some_and(|p| check(&p.read(cx).bar))
}

/// Click the middle of element `id` in pane `pane`'s page.
async fn click_element(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    id: &str,
) {
    let at = eval(
        any,
        ws,
        cx,
        pane,
        &format!("(r => [r.x + r.width / 2, r.y + r.height / 2])(document.getElementById('{id}').getBoundingClientRect())"),
    )
    .await;
    let xy = at
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)));
    let Some((x, y)) = xy else {
        fail(&format!("no element {id} in the page: {at:?}"));
    };
    if let Err(e) = with_view(any, ws, cx, pane, |v| v.click(x as f32, y as f32)) {
        // Another app took the keys (the user at the Mac): not this
        // check's to judge, as for typing.
        let key = cx
            .update_window(any, |_, window, _| window.is_window_active())
            .unwrap_or(false);
        if !key {
            println!("smoke: a click on {id}: the window is not the key window; not run");
            std::process::exit(crate::INCONCLUSIVE);
        }
        fail(&format!("a click on {id}: {e}"));
    }
}

/// One call to the daemon, from a thread of its own.
fn daemon_call(socket: std::path::PathBuf, request: api::Request) -> Result<api::Response, String> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let c = client::Client::connect(&socket, "smoke")
                .await
                .map_err(|e| e.to_string())?;
            c.call(request).await.map_err(|e| e.to_string())
        })
    })
    .join()
    .unwrap_or_else(|_| Err("the call's thread panicked".into()))
}

/// A connection to the daemon of its own, on a thread of its own, narrowed
/// to a grant (or the owner's, with none). Its calls are awaited, never
/// blocking the UI thread, which answers the daemon's `web.*` requests.
struct Agent(
    async_channel::Sender<(
        api::Request,
        async_channel::Sender<Result<api::Response, String>>,
    )>,
);

impl Agent {
    fn start(socket: PathBuf, grant: Option<api::Grant>) -> Agent {
        type Asked = (
            api::Request,
            async_channel::Sender<Result<api::Response, String>>,
        );
        let (tx, rx) = async_channel::unbounded::<Asked>();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let ready = async {
                    let c = client::Client::connect(&socket, "smoke-agent")
                        .await
                        .map_err(|e| e.to_string())?;
                    if let Some(g) = grant {
                        c.call(api::Request::GrantsNarrow(g))
                            .await
                            .map_err(|e| format!("grants.narrow: {e}"))?;
                    }
                    Ok::<_, String>(c)
                }
                .await;
                while let Ok((request, back)) = rx.recv().await {
                    let answer = match &ready {
                        Ok(c) => c.call(request).await.map_err(|e| e.to_string()),
                        Err(e) => Err(e.clone()),
                    };
                    let _ = back.send(answer).await;
                }
            });
        });
        Agent(tx)
    }

    async fn call(&self, request: api::Request) -> Result<api::Response, String> {
        let (back, answer) = async_channel::bounded(1);
        self.0
            .send((request, back))
            .await
            .map_err(|_| "the connection's thread ended".to_owned())?;
        answer
            .recv()
            .await
            .map_err(|_| "the connection's thread ended".to_owned())?
    }
}

/// What the form page's field is called (`/form` on the test site).
const FIELD: &str = "Your name";
/// What the agent types into it.
const NAME: &str = "Ada Lovelace";

/// An agent reads and drives a page (see the module's description): each
/// passed check is a `smoke: web inspect:` line.
async fn inspect(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp, out: &Path) {
    let host = site();
    let (socket, session) = read(any, ws, cx, |w, _| Some((w.socket.clone(), w.session?)))
        .flatten()
        .unwrap_or_else(|| fail("no daemon socket or session"));
    let level = api::ActLevel::Own as i32;
    let mut grant = api::Grant {
        methods: api::agent_methods(level),
        all_panes: true,
        level,
        ..api::Grant::default()
    };
    let blind = Agent::start(socket.clone(), Some(grant.clone()));
    grant.methods.push(api::INSPECT_WEB.to_owned());
    let agent = Agent::start(socket, Some(grant));
    // The agent opens the form in a window of its own: its own pane.
    let opened = agent
        .call(api::Request::WebOpen(api::WebOpen {
            url: format!("http://{host}/form"),
            session: Some(session),
            ..api::WebOpen::default()
        }))
        .await;
    let pane = match opened {
        Ok(api::Response::WebOpen(o)) => o.pane,
        other => fail(&format!("web.open as an agent: {other:?}")),
    };
    wait_long(
        any,
        ws,
        cx,
        "the form loaded in the agent's web pane",
        Duration::from_secs(30),
        move |w, cx| {
            w.page_of(pane, cx).is_some_and(|p| {
                let p = p.read(cx);
                p.view
                    .as_ref()
                    .is_some_and(|v| v.load_status().is_some() && v.title() == "form")
            })
        },
    )
    .await;

    let read_form = api::Request::WebRead(api::WebRead {
        pane,
        tree: true,
        wait_ms: None,
    });
    match blind.call(read_form.clone()).await {
        Err(e) if e.contains("not in this connection's grant") => {
            println!("smoke: web inspect: web.read without web.inspect is refused ({e})");
        }
        other => fail(&format!("web.read without web.inspect: {other:?}")),
    }

    let page = match agent.call(read_form).await {
        Ok(api::Response::WebRead(p)) => p,
        other => fail(&format!("web.read: {other:?}")),
    };
    let field = page
        .nodes
        .iter()
        .find(|n| n.role == "textbox" && n.name == FIELD && n.node != 0)
        .map(|n| n.node);
    let Some(field) = field.filter(|_| page.text.contains(FIELD)) else {
        fail(&format!(
            "web.read did not show the form's field: {:?} {:?}",
            page.text, page.nodes
        ));
    };
    println!(
        "smoke: web inspect: web.read read the form ({} nodes) and found its field, node {field}",
        page.nodes.len()
    );

    let typed = agent
        .call(api::Request::WebType(api::WebType {
            pane,
            node: Some(field),
            text: NAME.into(),
            submit: true,
            ..api::WebType::default()
        }))
        .await;
    if let Err(e) = typed {
        fail(&format!("web.type: {e}"));
    }
    let sent = format!("sent {NAME}");
    wait_title(any, ws, cx, pane, &sent, "web.type and Enter").await;
    println!("smoke: web inspect: web.type filled the field and Enter submitted the form ({sent})");

    let clicked = agent
        .call(api::Request::WebClick(api::WebClick {
            pane,
            selector: Some("#again".into()),
            ..api::WebClick::default()
        }))
        .await;
    if let Err(e) = clicked {
        fail(&format!("web.click: {e}"));
    }
    wait_title(any, ws, cx, pane, "form", "web.click").await;
    println!("smoke: web inspect: web.click on #again loaded the form again");

    let shot = out.join("web-inspect.png");
    let taken = agent
        .call(api::Request::WebScreenshot(api::WebScreenshot {
            pane,
            path: shot.display().to_string(),
            wait_ms: None,
        }))
        .await;
    match taken {
        Ok(api::Response::WebScreenshot(c)) if shot.is_file() => println!(
            "smoke: web inspect: web.screenshot wrote the page, {}x{}, to {}",
            c.width,
            c.height,
            shot.display()
        ),
        other => fail(&format!("web.screenshot: {other:?}")),
    }
}

/// The browser in the plain web pane `pane` (see the module's
/// description): each passed check is a `smoke: web browser:` line.
async fn browser(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp, pane: u32) {
    let host = site();
    let socket =
        read(any, ws, cx, |w, _| w.socket.clone()).unwrap_or_else(|| fail("no daemon socket"));
    // `dream browser --pane`: the daemon changes the pane's address, and
    // the page follows. The login sets a session cookie, checked after a
    // restart (`restart`).
    let login = format!("http://{host}/login");
    daemon_call(
        socket,
        api::Request::WebNavigate(api::WebNavigate {
            pane,
            url: login.clone(),
            ..api::WebNavigate::default()
        }),
    )
    .unwrap_or_else(|e| fail(&format!("web.navigate: {e}")));
    wait_title(any, ws, cx, pane, "logged in", "web.navigate").await;
    println!(
        "smoke: web browser: web.navigate loaded {login} in the pane, which set a session cookie"
    );

    // Focus the page, as a person would, so its keys are the browser's.
    if let Err(e) = with_view(any, ws, cx, pane, |v| v.click(40.0, 40.0)) {
        fail(&format!("a click in the page: {e}"));
    }
    wait_long(
        any,
        ws,
        cx,
        "the browser page focused",
        TEN,
        move |w, cx| page_focus(w, cx, pane, true),
    )
    .await;

    // Cmd+L pulls the address bar down with the keys; an address typed
    // without a scheme, and Enter, loads it.
    press(any, ws, cx, pane, &[(KEY_L, "l", true)]).unwrap_or_else(|e| fail(&e));
    wait_long(
        any,
        ws,
        cx,
        "the address bar after Cmd+L, with the keys",
        TEN,
        move |w, cx| {
            bar_now(w, cx, pane, |b| b.address.is_some() && b.keys)
                && w.focused == Some(pane)
                && !w.panes[&pane]
                    .web_pane()
                    .is_some_and(|p| p.read(cx).page_has_focus())
        },
    )
    .await;
    let typed = format!("{host}/");
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, cx| {
            if let Some(p) = w.panes.get(&pane).and_then(PaneView::web_pane) {
                p.update(cx, |p, cx| p.typed(&typed, cx));
            }
        });
    });
    press(any, ws, cx, pane, &[(KEY_RETURN, "\r", false)]).unwrap_or_else(|e| fail(&e));
    let first = wait_title(
        any,
        ws,
        cx,
        pane,
        "browser a ",
        "an address typed into the bar",
    )
    .await;
    if bar(any, ws, cx, pane, |b| b.address.is_none()) != Some(true) {
        fail("the address bar stayed after Enter");
    }
    println!(
        "smoke: web browser: Cmd+L, {typed:?} typed, and Enter loaded http://{typed} ({first})"
    );

    // A link.
    click_element(any, ws, cx, pane, "next").await;
    wait_title(any, ws, cx, pane, "browser b", "a link").await;
    println!("smoke: web browser: a click on a link loaded its page");

    // Back and forward by key, then by the mouse's side buttons.
    press(any, ws, cx, pane, &[(KEY_LEFT_BRACKET, "[", true)]).unwrap_or_else(|e| fail(&e));
    wait_title(any, ws, cx, pane, "browser a ", "Cmd+[").await;
    press(any, ws, cx, pane, &[(KEY_RIGHT_BRACKET, "]", true)]).unwrap_or_else(|e| fail(&e));
    wait_title(any, ws, cx, pane, "browser b", "Cmd+]").await;
    println!("smoke: web browser: Cmd+[ went back and Cmd+] forward");
    for (back, title) in [(true, "browser a "), (false, "browser b")] {
        if let Err(e) = with_view(any, ws, cx, pane, |v| v.click_side_button(40.0, 40.0, back)) {
            fail(&format!("a mouse side button: {e}"));
        }
        wait_title(any, ws, cx, pane, title, "a mouse side button").await;
    }
    println!("smoke: web browser: mouse buttons 4 and 5 went back and forward");
    press(any, ws, cx, pane, &[(KEY_LEFT_BRACKET, "[", true)]).unwrap_or_else(|e| fail(&e));
    let before = wait_title(any, ws, cx, pane, "browser a ", "back to the first page").await;

    // Reload: the page loads again (the site counts its loads).
    press(any, ws, cx, pane, &[(KEY_R, "r", true)]).unwrap_or_else(|e| fail(&e));
    let deadline = Instant::now() + TEN;
    let reloaded = loop {
        let (now, _) = where_is(any, ws, cx, pane);
        if now.starts_with("browser a ") && now != before {
            break now;
        }
        if Instant::now() > deadline {
            fail(&format!("Cmd+R did not load the page again ({now:?})"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    };
    println!("smoke: web browser: Cmd+R loaded the page again ({before} -> {reloaded})");

    // Find: Chromium highlights the matches; Esc ends it and the page has
    // the keys again.
    press(any, ws, cx, pane, &[(KEY_F, "f", true)]).unwrap_or_else(|e| fail(&e));
    wait_long(
        any,
        ws,
        cx,
        "find in the page after Cmd+F, with the keys",
        TEN,
        move |w, cx| bar_now(w, cx, pane, |b| b.find.is_some() && b.keys),
    )
    .await;
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, cx| {
            if let Some(p) = w.panes.get(&pane).and_then(PaneView::web_pane) {
                p.update(cx, |p, cx| p.typed("needle", cx));
            }
        });
    });
    let deadline = Instant::now() + TEN;
    let found = loop {
        if let Some(Some((n, at))) = bar(any, ws, cx, pane, |b| b.find.as_ref().and_then(|f| f.1))
            && n > 0
            && at > 0
        {
            break (n, at);
        }
        if Instant::now() > deadline {
            fail("find in the page found nothing");
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    };
    press(any, ws, cx, pane, &[(KEY_ESCAPE, "\u{1b}", false)]).unwrap_or_else(|e| fail(&e));
    wait_long(
        any,
        ws,
        cx,
        "find closed by Esc, the page focused",
        TEN,
        move |w, cx| {
            page_focus(w, cx, pane, true)
                && w.panes
                    .get(&pane)
                    .and_then(PaneView::web_pane)
                    .is_some_and(|p| p.read(cx).bar.find.is_none())
        },
    )
    .await;
    println!(
        "smoke: web browser: find highlighted {} matches of \"needle\" (match {} current); Esc closed it",
        found.0, found.1
    );

    // Zoom in and back.
    let ratio = "String(devicePixelRatio)";
    let base = eval(any, ws, cx, pane, ratio).await;
    press(any, ws, cx, pane, &[(KEY_EQUAL, "=", true)]).unwrap_or_else(|e| fail(&e));
    let deadline = Instant::now() + TEN;
    loop {
        let now = eval(any, ws, cx, pane, ratio).await;
        let more = match (&now, &base) {
            (Some(a), Some(b)) => {
                let f = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
                f(a).zip(f(b)).is_some_and(|(a, b)| a > b)
            }
            _ => false,
        };
        if more {
            break;
        }
        if Instant::now() > deadline {
            fail(&format!(
                "Cmd+= did not zoom the page ({base:?} -> {now:?})"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    press(any, ws, cx, pane, &[(KEY_ZERO, "0", true)]).unwrap_or_else(|e| fail(&e));
    if let Some(b) = base.as_ref().and_then(serde_json::Value::as_str)
        && let Err(e) = wait_value(any, ws, cx, pane, ratio, b).await
    {
        fail(&format!("Cmd+0 did not zoom the page back: {e}"));
    }
    println!("smoke: web browser: Cmd+= zoomed the page in and Cmd+0 back");

    // A download asks first, in a row over the page; nothing is written
    // until Save.
    let folder = downloads_folder();
    if !listing(cx, &folder).await.is_empty() {
        fail(&format!(
            "the downloads folder {} is not empty",
            folder.display()
        ));
    }
    click_element(any, ws, cx, pane, "dl").await;
    let id = wait_asked(any, ws, cx, pane, "report.bin").await;
    let asked =
        page_bar(any, ws, cx, pane, |b| format!("{:?}", b.downloads.first())).unwrap_or_default();
    if !listing(cx, &folder).await.is_empty() {
        fail("a download wrote to the folder before Save");
    }
    println!("smoke: web browser: a download asked first, and wrote nothing: {asked}");

    // Save: the file, with the site's contents, in the folder.
    answer(any, ws, cx, pane, id, true);
    wait_settled(any, ws, cx, pane, &folder, &["report.bin"], "Save").await;
    let saved = folder.join("report.bin");
    let got = cx
        .background_executor()
        .spawn(async move { std::fs::read(saved) })
        .await
        .unwrap_or_else(|e| fail(&format!("the saved download: {e}")));
    if got != DOWNLOAD_BODY.as_bytes() {
        fail(&format!(
            "the saved download holds {:?}",
            String::from_utf8_lossy(&got)
        ));
    }
    println!(
        "smoke: web browser: Save wrote report.bin ({} bytes, the site's) to {}",
        got.len(),
        folder.display()
    );

    // Cancel at the prompt: nothing more in the folder.
    click_element(any, ws, cx, pane, "dl").await;
    let id = wait_asked(any, ws, cx, pane, "report.bin").await;
    answer(any, ws, cx, pane, id, false);
    wait_settled(
        any,
        ws,
        cx,
        pane,
        &folder,
        &["report.bin"],
        "Cancel at the prompt",
    )
    .await;
    // Long enough for a file to appear if one were coming.
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    wait_settled(
        any,
        ws,
        cx,
        pane,
        &folder,
        &["report.bin"],
        "Cancel at the prompt",
    )
    .await;
    println!("smoke: web browser: Cancel at the prompt wrote nothing");

    // Cancel while a slow download is written: no partial file is left.
    click_element(any, ws, cx, pane, "slow").await;
    let id = wait_asked(any, ws, cx, pane, "slow.bin").await;
    answer(any, ws, cx, pane, id, true);
    let deadline = Instant::now() + TEN;
    let partial = loop {
        let writing = page_bar(any, ws, cx, pane, |b| {
            b.downloads
                .iter()
                .find(|d| d.id == id)
                .and_then(|d| match &d.state {
                    crate::web::Saving::Writing { received, part, .. } if *received > 0 => {
                        Some((*received, part.clone()))
                    }
                    _ => None,
                })
        })
        .flatten();
        if let Some(w) = writing {
            break w;
        }
        if Instant::now() > deadline {
            fail("the slow download never made progress");
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    };
    let during = listing(cx, &folder).await;
    if !during.iter().any(|n| n.starts_with("slow.bin")) {
        fail(&format!(
            "no partial file while slow.bin downloads: {during:?}"
        ));
    }
    answer(any, ws, cx, pane, id, false);
    wait_settled(
        any,
        ws,
        cx,
        pane,
        &folder,
        &["report.bin"],
        "Cancel while saving",
    )
    .await;
    println!(
        "smoke: web browser: Cancel after {} bytes of slow.bin removed its partial file ({}; the folder held {during:?})",
        partial.0,
        partial.1.display()
    );

    // A link that wants a new window opens in the same pane.
    let panes = read(any, ws, cx, |w, _| {
        w.current.as_ref().map(|t| t.panes.len())
    })
    .flatten()
    .unwrap_or(0);
    click_element(any, ws, cx, pane, "pop").await;
    wait_title(
        any,
        ws,
        cx,
        pane,
        "browser c",
        "a link that wants a new window",
    )
    .await;
    let now = read(any, ws, cx, |w, _| {
        w.current.as_ref().map(|t| t.panes.len())
    })
    .flatten()
    .unwrap_or(0);
    if now != panes {
        fail(&format!("a new window's link made panes: {panes} -> {now}"));
    }
    println!("smoke: web browser: a link that wants a new window opened in the same pane");

    // The plain page has no bridge (checked before), nor does any site.
    let plain = eval(any, ws, cx, pane, "typeof window.dreamer").await;
    if plain.as_ref().and_then(serde_json::Value::as_str) != Some("undefined") {
        fail(&format!(
            "a site in the browser has window.dreamer: {plain:?}"
        ));
    }
}

/// How long adding Dreamer Web from a local zip may take: copying,
/// unpacking, and checking a few hundred MB.
const INSTALL: Duration = Duration::from_secs(300);

/// `dreamer --smoke-web-install`, before `--smoke-web`, with an empty
/// private data directory, `DREAMER_WEB_URL` naming a directory that holds
/// the component's zip, and `web.component: ask`; copies already on the
/// system are not used (they would be the user's). The tab strip offers
/// Dreamer Web at the first start; a web pane opened without it shows
/// "Add Dreamer Web"; its button, pressed through its own click handler,
/// adds it with its progress; the offer is then gone; and the page loads
/// and draws in that pane, with no restart. The pane is closed at the
/// end, leaving the session as it was for `--smoke-web`, which runs with
/// the component this added.
pub(crate) async fn install(any: AnyWindowHandle, ws: WeakEntity<Workspace>, cx: &mut AsyncApp) {
    use crate::webcomp::{self, InPane, Status};
    wait_long(
        any,
        &ws,
        cx,
        "session main with its pane",
        Duration::from_secs(20),
        |w, _| !w.pane_ids().is_empty(),
    )
    .await;
    if web_component::check(&web_component::installed_dir()) != web_component::State::Missing {
        fail(&format!(
            "Dreamer Web is already in the data directory ({}): this check needs an empty one",
            web_component::installed_dir().display()
        ));
    }

    // The first start offers it in the tab strip.
    wait_long(
        any,
        &ws,
        cx,
        "the first-start offer of Dreamer Web (or its status looked at)",
        Duration::from_secs(20),
        |_, _| webcomp::offered().is_some() || webcomp::status() == Status::Ready,
    )
    .await;
    match webcomp::offered() {
        Some(Status::Missing(_)) => println!(
            "smoke: web install: with no Dreamer Web, the tab strip offers it at the first start (web.component: ask)"
        ),
        other => fail(&format!(
            "the tab strip does not offer Dreamer Web: {other:?}, status {:?}",
            webcomp::status()
        )),
    }

    // A web pane opened without it offers to add it.
    let before: Vec<u32> = read(any, &ws, cx, |w, _| w.pane_ids()).unwrap_or_default();
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            w.topology.web_open(api::WebOpen {
                url: PAGE.to_owned(),
                pane: before.first().copied(),
                ..api::WebOpen::default()
            });
        });
    });
    wait_long(
        any,
        &ws,
        cx,
        "a web pane waiting for Dreamer Web",
        Duration::from_secs(30),
        |w, cx| {
            w.panes
                .values()
                .any(|v| v.web_pane().is_some_and(|p| p.read(cx).wants_component()))
        },
    )
    .await;
    let pane = read(any, &ws, cx, |w, cx| {
        w.panes.iter().find_map(|(id, v)| {
            v.web_pane()
                .is_some_and(|p| p.read(cx).wants_component())
                .then_some(*id)
        })
    })
    .flatten()
    .unwrap_or_else(|| fail("the web pane went away"));
    match webcomp::in_pane_state() {
        InPane::Add(why) => println!(
            "smoke: web install: web pane %{pane} shows Add Dreamer Web instead of its page ({why})"
        ),
        other => fail(&format!(
            "web pane %{pane} does not offer to add Dreamer Web: {other:?}"
        )),
    }

    // Its button adds it, with the progress in the pane.
    cx.update(webcomp::press_add);
    let deadline = Instant::now() + INSTALL;
    let mut steps: Vec<String> = Vec::new();
    loop {
        let (status, shown) = cx.update(|_| (webcomp::status(), webcomp::in_pane_state()));
        if let InPane::Adding(step) = shown {
            let what = match step {
                web_component::install::Step::Looking => "looking",
                web_component::install::Step::Downloading { .. } => "downloading",
                web_component::install::Step::Unpacking => "unpacking",
                web_component::install::Step::Checking => "checking",
                web_component::install::Step::Installing => "installing",
            };
            if !steps.iter().any(|s| s == what) {
                steps.push(what.to_owned());
            }
        }
        match status {
            Status::Ready => break,
            Status::Failed(why) => fail(&format!("adding Dreamer Web failed: {why}")),
            _ if Instant::now() > deadline => fail(&format!(
                "Dreamer Web was not added within {} s (status {status:?})",
                INSTALL.as_secs()
            )),
            _ => {}
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    if !steps.iter().any(|s| s == "downloading") {
        fail(&format!(
            "the install showed no download from DREAMER_WEB_URL (steps {steps:?})"
        ));
    }
    let dir = web_component::installed_dir();
    if !matches!(web_component::check(&dir), web_component::State::Ready(_))
        || web_component::linked_to(&dir).is_some()
    {
        fail(&format!(
            "{} is not this build's Dreamer Web, installed there",
            dir.display()
        ));
    }
    println!(
        "smoke: web install: the pane's button added Dreamer Web ({}) into {}",
        steps.join(", "),
        dir.display()
    );
    if let Some(left) = webcomp::offered() {
        fail(&format!(
            "the first-start offer still shows once it was added: {left:?}"
        ));
    }
    println!("smoke: web install: the first-start offer is gone once it was added");

    // The page then loads in the same pane, with no restart.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match read(any, &ws, cx, web_pane).flatten() {
            Some(Ok((id, ..))) if id == pane => break,
            Some(Ok((id, ..))) => fail(&format!("the page loaded in pane %{id}, not %{pane}")),
            Some(Err(problem))
                if problem != crate::web::BEING_ADDED && Instant::now() > deadline =>
            {
                fail(&format!("the web pane cannot draw its page: {problem}"))
            }
            _ if Instant::now() > deadline => {
                crate::stalled(
                    any,
                    cx,
                    "never saw: the page loaded once Dreamer Web was added",
                )
                .await
            }
            _ => {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await
            }
        }
    }
    let Some((frames, _)) =
        wait_page(any, &ws, cx, "the page once added", TEN, |n, _| n > 30).await
    else {
        fail("the page never drew 30 animation frames once Dreamer Web was added");
    };
    println!(
        "smoke: web install: the page then loaded in pane %{pane} and Chromium drew {frames} frames, with no restart"
    );

    // The session as it was, for `--smoke-web`.
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            w.topology
                .act(api::action::Kind::KillPane(api::KillPane { pane }));
        });
    });
    wait_long(
        any,
        &ws,
        cx,
        "the web pane closed",
        Duration::from_secs(10),
        move |w, _| !w.pane_ids().contains(&pane),
    )
    .await;
    cx.update(|cx| cx.quit());
}

/// `dreamer --smoke-web-restart DIR`, after `--smoke-web` with the same
/// desktop state: a page of a site that asks for the cookie the first run's
/// login set, a session cookie, which came back from the browser's profile.
pub(crate) async fn restart(any: AnyWindowHandle, ws: WeakEntity<Workspace>, cx: &mut AsyncApp) {
    wait_long(
        any,
        &ws,
        cx,
        "session main with its pane",
        Duration::from_secs(20),
        |w, _| !w.pane_ids().is_empty(),
    )
    .await;
    let host = site();
    let before: Vec<u32> = read(any, &ws, cx, |w, _| w.pane_ids()).unwrap_or_default();
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            w.topology.web_open(api::WebOpen {
                url: format!("http://{host}/who"),
                pane: before.first().copied(),
                ..api::WebOpen::default()
            });
        });
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let pane = loop {
        let found = read(any, &ws, cx, |w, cx| {
            w.panes.iter().find_map(|(id, v)| {
                let p = v.web_pane()?.read(cx);
                (p.url.ends_with("/who") && p.view.as_ref()?.load_status().is_some()).then_some(*id)
            })
        })
        .flatten();
        if let Some(p) = found {
            break p;
        }
        if Instant::now() > deadline {
            crate::stalled(any, cx, "never saw: the browser page after a restart").await;
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    };
    let title = wait_title(any, &ws, cx, pane, "who ", "the page after a restart").await;
    if title != "who in" {
        fail(&format!(
            "the login's session cookie did not survive the restart ({title:?})"
        ));
    }
    println!(
        "smoke: web browser: after a restart the site still had the login's session cookie ({title})"
    );
    cx.update(|cx| cx.quit());
}

/// The color page: sRGB's red over its top half, Display P3's red (beyond
/// sRGB) over its bottom half. Its title counts animation frames as
/// [`PAGE`]'s does.
const COLOR_PAGE: &str = "data:text/html,<html><body style='margin:0;height:100vh;\
background:linear-gradient(rgb(255 0 0) 50%25,color(display-p3 1 0 0) 50%25)'>\
<script>let n=0;function f(){n++;document.title='raf '+n+' y '+Math.round(scrollY);\
requestAnimationFrame(f)}requestAnimationFrame(f)</script></body></html>";

/// How far apart two captured colors may be, per channel, and still be
/// the same color (the gate's tolerance, as in its checks of captures).
const TOLERANCE: u8 = 30;

/// `dreamer --smoke-web-color DIR`, in a process of its own (Chromium
/// takes `web.color` as it starts), with `web.color` as the gate set it:
/// the color page captured live (`color-live.png`) and as its snapshot
/// under the palette (`color-palette.png`), both written to DIR. The color
/// read from each half is the most common one where the page shows beside
/// the palette.
///
/// In either mode the page's sRGB red is sRGB's red as the window shows
/// it (Chromium's output is color matched, as the toolkit's is), and the
/// snapshot matches the live page there. Display P3's red is then clamped
/// to sRGB with `srgb`, and stays beyond sRGB with `display`. That last
/// pair needs a wide-gamut display; without one it is a `smoke: web
/// color not run:` line. (A snapshot is an sRGB image to the toolkit, so
/// with `display` its P3 half is clamped: reported, not judged.)
/// Each passed check is a `smoke: web color:` line.
pub(crate) async fn color(
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
                url: COLOR_PAGE.to_owned(),
                pane: w.pane_ids().first().copied(),
                ..api::WebOpen::default()
            });
        });
    });
    // Found by its address: the runs before this one left web panes in
    // the session.
    let deadline = Instant::now() + Duration::from_secs(30);
    let pane = loop {
        let found = read(any, &ws, cx, |w, cx| {
            w.panes.iter().find_map(|(id, v)| {
                let p = v.web_pane()?.read(cx);
                if !p.url.contains("display-p3") {
                    return None;
                }
                if let Some(problem) = p.not_drawn() {
                    return Some(Err(problem));
                }
                p.view.as_ref()?.load_status()?;
                Some(Ok(*id))
            })
        })
        .flatten();
        match found {
            Some(Ok(id)) => break id,
            Some(Err(problem)) if problem != crate::web::BEING_ADDED => {
                fail(&format!("the web pane cannot draw its page: {problem}"))
            }
            _ if Instant::now() > deadline => {
                crate::stalled(any, cx, "never saw: the color page loaded in its pane").await
            }
            _ => {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await
            }
        }
    };
    let frames = wait_frames(any, &ws, cx, pane, 30).await;
    // Zoomed, the page fills the window: most of it is beside the palette.
    let size = cx
        .update_window(any, |_, window, _| window.viewport_size())
        .unwrap_or_default();
    let window_w = f32::from(size.width);
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            w.topology
                .act(api::action::Kind::ZoomPane(api::ZoomPane { pane }));
        });
    });
    wait_long(any, &ws, cx, "the color page zoomed", TEN, |w, cx| {
        page(w, cx, pane).is_some_and(|(r, shown, _)| shown && r.width > window_w * 0.8)
    })
    .await;
    wait_frames(any, &ws, cx, pane, frames + 10).await;
    let mode = web_engine::color();
    let want = match crate::live::with(|l| l.web.color).unwrap_or_default() {
        workspace_config::WebColor::Srgb => web_engine::Color::Srgb,
        workspace_config::WebColor::Display => web_engine::Color::Display,
    };
    if mode != want {
        fail(&format!(
            "Chromium draws in {mode:?}, not in {want:?} as web.color says"
        ));
    }
    let Some((rect, _, _)) = read(any, &ws, cx, |w, cx| page(w, cx, pane)).flatten() else {
        fail("the color page has no place in the window");
    };
    let window = with_view(any, &ws, cx, pane, web_engine::WebView::window_number)
        .unwrap_or_else(|| fail("the color page's window has no number"));
    let screen = with_view(any, &ws, cx, pane, web_engine::WebView::screen_color_space)
        .unwrap_or_else(|| fail("the window's screen has no color space"));
    // Where the page shows beside the palette, live and as its snapshot.
    let palette_right = (window_w + PALETTE_WIDTH) / 2.0;
    let beside = Rect {
        x: rect.x.max(palette_right + 8.0),
        width: rect.x + rect.width - rect.x.max(palette_right + 8.0),
        ..rect
    };
    if beside.width < 16.0 {
        fail("the palette leaves none of the web pane beside it");
    }
    let live_png = out.join("color-live.png");
    capture(cx, window, &live_png).await;

    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| w.open_palette(window, cx));
    });
    wait_long(
        any,
        &ws,
        cx,
        "the color page's snapshot under the palette",
        TEN,
        |w, cx| page(w, cx, pane).is_some_and(|(_, shown, snapshot)| !shown && snapshot),
    )
    .await;
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    let palette_png = out.join("color-palette.png");
    capture(cx, window, &palette_png).await;
    let _ = cx.update_window(any, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| w.close_palette(window, cx));
    });

    // The page's two halves, as they show beside the palette.
    let half = beside.height / 2.0;
    let top = Rect {
        height: half,
        ..beside
    };
    let bottom = Rect {
        y: beside.y + half,
        height: half,
        ..beside
    };
    let (live_srgb, space) = most_common(&live_png, window_w, top);
    let (live_p3, _) = most_common(&live_png, window_w, bottom);
    let (snap_srgb, _) = most_common(&palette_png, window_w, top);
    let (snap_p3, _) = most_common(&palette_png, window_w, bottom);
    // sRGB's red as the window shows it, in the capture's color space.
    let captured = match space {
        crate::web::PngSpace::Icc(profile) => web_engine::ColorSpace::from_icc(&profile),
        crate::web::PngSpace::Srgb => web_engine::ColorSpace::srgb(),
        crate::web::PngSpace::Unnamed => None,
    };
    let mut red = [255, 0, 0, 255];
    if let Err(e) = web_engine::srgb_to(&mut red, 1, 1, captured.as_ref().unwrap_or(&screen)) {
        fail(&format!("sRGB's red in the capture's color space: {e}"));
    }
    let red = [red[0], red[1], red[2]];
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= TOLERANCE);
    let m = if mode == web_engine::Color::Display {
        "display"
    } else {
        "srgb"
    };
    println!(
        "smoke: web color {m}: live sRGB red {}, P3 red {}; snapshot {}, {}; sRGB's red as the window shows it {}; the screen is {}wide-gamut",
        hex(live_srgb),
        hex(live_p3),
        hex(snap_srgb),
        hex(snap_p3),
        hex(red),
        if screen.is_wide_gamut() { "" } else { "not " }
    );
    if !near(live_srgb, red) {
        fail(&format!(
            "the page's sRGB red shows as {}, not as the window shows sRGB's red ({}) within {TOLERANCE} per channel: Chromium's output is not color matched",
            hex(live_srgb),
            hex(red)
        ));
    }
    println!(
        "smoke: web color: {m}: the page's sRGB red is the window's ({}): color matched",
        hex(live_srgb)
    );
    if !near(snap_srgb, live_srgb) {
        fail(&format!(
            "the snapshot under the palette ({}) is not the live page ({}) within {TOLERANCE} per channel",
            hex(snap_srgb),
            hex(live_srgb)
        ));
    }
    println!(
        "smoke: web color: {m}: the snapshot under the palette matches the live page ({} and {})",
        hex(snap_srgb),
        hex(live_srgb)
    );
    if !screen.is_wide_gamut() {
        println!(
            "smoke: web color not run: {m}: the display is not wide-gamut, so Display P3's red cannot show beyond sRGB"
        );
    } else if mode == web_engine::Color::Display {
        if near(live_p3, red) {
            fail(&format!(
                "Display P3's red shows as sRGB's ({}): clamped, though web.color is display",
                hex(live_p3)
            ));
        }
        println!(
            "smoke: web color: display: Display P3's red is beyond sRGB live ({}, sRGB's red {}); its snapshot is the toolkit's sRGB ({})",
            hex(live_p3),
            hex(red),
            hex(snap_p3)
        );
    } else {
        if !near(live_p3, red) {
            fail(&format!(
                "Display P3's red shows as {}, not clamped to sRGB's red ({}) within {TOLERANCE} per channel",
                hex(live_p3),
                hex(red)
            ));
        }
        println!(
            "smoke: web color: srgb: Display P3's red is clamped to sRGB live ({})",
            hex(live_p3)
        );
    }
    println!("smoke: web color captures in {}", out.display());
    // The session as it was, for the next run.
    let _ = cx.update_window(any, |_, _, cx| {
        let _ = ws.update(cx, |w, _| {
            w.topology
                .act(api::action::Kind::KillPane(api::KillPane { pane }));
        });
    });
    wait_long(
        any,
        &ws,
        cx,
        "the color page's pane closed",
        TEN,
        move |w, _| !w.pane_ids().contains(&pane),
    )
    .await;
    cx.update(|cx| cx.quit());
}

/// Wait until pane `pane`'s page has drawn more than `more_than`
/// animation frames (its title counts them), at most ten seconds; exit 5
/// (not run) if its frames stop coming. The count reached.
async fn wait_frames(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    more_than: u64,
) -> u64 {
    let deadline = Instant::now() + TEN;
    let mut last = (0, Instant::now());
    loop {
        let n = with_view(any, ws, cx, pane, |v| progress(&v.title()).map(|p| p.0));
        if let Some(n) = n {
            if n > more_than {
                return n;
            }
            if n != last.0 {
                last = (n, Instant::now());
            }
        }
        if last.1.elapsed() > THREE {
            println!(
                "smoke: Chromium stopped producing frames for the color page; the display is not refreshing; not run"
            );
            std::process::exit(crate::INCONCLUSIVE);
        }
        if Instant::now() > deadline {
            fail(&format!(
                "the color page drew {} animation frames, not more than {more_than}",
                last.0
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// The most common color in `rect` (points from the window's top left, a
/// few points at the edges left out) of the window capture at `path`,
/// and the color space the capture names.
fn most_common(path: &Path, window_w: f32, rect: Rect) -> ([u8; 3], crate::web::PngSpace) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let (w, h, rgba, space) = crate::web::decode_png(&bytes)
        .unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let scale = w as f32 / window_w.max(1.0);
    let inset = 4.0;
    let px = |v: f32, max: u32| ((v * scale).round().max(0.0) as u32).min(max);
    let (x0, x1) = (px(rect.x + inset, w), px(rect.x + rect.width - inset, w));
    let (y0, y1) = (px(rect.y + inset, h), px(rect.y + rect.height - inset, h));
    let mut counts: std::collections::HashMap<[u8; 3], usize> = std::collections::HashMap::new();
    for row in y0..y1 {
        for col in x0..x1 {
            let at = (row as usize * w as usize + col as usize) * 4;
            *counts
                .entry([rgba[at], rgba[at + 1], rgba[at + 2]])
                .or_default() += 1;
        }
    }
    let main = counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| c)
        .unwrap_or_else(|| {
            fail(&format!(
                "{}: the page's rectangle is empty",
                path.display()
            ))
        });
    (main, space)
}
