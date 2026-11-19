//! Images of panes for `dream app preview --png`, made by this desktop
//! for the daemon (`apps.capture`).
//!
//! A thread of its own, with its own connection to the local daemon, waits
//! for capture requests (`apps.capture_wait`) and hands each to the UI
//! thread, which only finds the pane: the window that shows it, its
//! rectangle there, and for an app pane whether the frame drawn last has
//! the view tree version asked for (it waits for that on a timer, never
//! blocking). The capture itself runs on a thread of its own: on macOS it
//! records the window with `screencapture -l` (the same as the gate's
//! screenshots) and crops the pane's rectangle. A pane that shows a page
//! (a web pane, or a web app's) is the page's own image instead, from
//! Chromium (inspect.rs). A window that does not show the pane answers
//! why, so does a platform with no capture yet.
//!
//! A request that names a `web.*` method is run on the page of the pane
//! in the window that shows it (inspect.rs), and answered with its result.
//!
//! The same connection reports whether this desktop can draw web pages
//! (`web.component`, from webcomp.rs) whenever that changes, and again
//! after connecting: a `web.open` waits on it while Dreamer Web is being
//! added here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AppContext as _, Bounds, Entity, Pixels};

use crate::workspace::Workspace;

/// How long the pane's tree may take to reach the frame drawn.
const DRAWN_WAIT: Duration = Duration::from_secs(5);

/// What a desktop answers a request with.
#[derive(Debug, Default)]
pub(crate) struct Done {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// A web method's result, as the API's JSON.
    pub(crate) result: String,
}

/// The answer to a request, or why there is none.
type Answer = (u64, Result<Done, String>);

/// Serve captures for the daemon at `socket`, once per process.
pub(crate) fn serve(socket: PathBuf, cx: &mut gpui::App) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let (requests_tx, requests) = async_channel::unbounded::<api::AppCaptureRequest>();
    let (answers, answers_rx) = tokio::sync::mpsc::unbounded_channel::<Answer>();
    let (reports_tx, reports) = async_channel::unbounded::<api::WebComponentState>();
    let spawned = std::thread::Builder::new()
        .name("dreamer-capture".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(wait_for_requests(socket, requests_tx, answers_rx, reports));
        });
    if spawned.is_err() {
        return;
    }
    crate::webcomp::report_to(reports_tx);
    cx.spawn(async move |cx| {
        while let Ok(r) = requests.recv().await {
            let answers = answers.clone();
            cx.spawn(async move |cx| {
                let answer = match find(&r, cx).await {
                    Err(why) => Err(why),
                    Ok((_, page)) if !r.method.is_empty() => {
                        crate::inspect::answer(page, &r, cx).await
                    }
                    #[cfg(feature = "web")]
                    Ok((_, Some(page))) => crate::inspect::image(&page, Path::new(&r.path), cx)
                        .await
                        .map(|(width, height)| Done {
                            width,
                            height,
                            ..Done::default()
                        }),
                    Ok((target, _)) => {
                        match take(r.id, target, PathBuf::from(&r.path), answers.clone()) {
                            // The capture's thread answers.
                            Ok(()) => return,
                            Err(why) => Err(why),
                        }
                    }
                };
                let _ = answers.send((r.id, answer));
            })
            .detach();
        }
    })
    .detach();
}

/// Ask the daemon for requests, hand them over, and send the answers and
/// Dreamer Web's status; connect again after a loss.
async fn wait_for_requests(
    socket: PathBuf,
    requests: async_channel::Sender<api::AppCaptureRequest>,
    mut answers: tokio::sync::mpsc::UnboundedReceiver<Answer>,
    reports: async_channel::Receiver<api::WebComponentState>,
) {
    let mut backoff = Duration::from_millis(500);
    // The status last reported, to report again on a new connection.
    let mut latest: Option<api::WebComponentState> = None;
    loop {
        let c = match client::Client::connect(&socket, "desktop-capture").await {
            Ok(c) => std::sync::Arc::new(c),
            Err(_) => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        backoff = Duration::from_millis(500);
        if let Some(state) = latest.clone() {
            report(&c, state).await;
        }
        let next = |c: std::sync::Arc<client::Client>| {
            Box::pin(async move {
                c.call(api::Request::AppsCaptureWait(api::AppCaptureWait {
                    wait_ms: Some(20_000),
                }))
                .await
            })
        };
        let mut waiting = next(c.clone());
        loop {
            tokio::select! {
                r = &mut waiting => match r {
                    Ok(api::Response::AppsCaptureWait(r)) => {
                        if r.id != 0 && requests.send(r).await.is_err() {
                            return;
                        }
                        waiting = next(c.clone());
                    }
                    Ok(_) => waiting = next(c.clone()),
                    // A daemon without captures: ask again much later
                    // (it may be replaced by a newer one).
                    Err(client::ClientError::Rpc(_)) => {
                        backoff = Duration::from_secs(60);
                        break;
                    }
                    Err(_) => break,
                },
                r = reports.recv(), if !reports.is_closed() => {
                    if let Ok(state) = r {
                        latest = Some(state.clone());
                        report(&c, state).await;
                    }
                }
                a = answers.recv() => {
                    let Some((id, answer)) = a else { return };
                    let done = match answer {
                        Ok(d) => api::AppCaptureDone {
                            id,
                            error: None,
                            width: d.width,
                            height: d.height,
                            result: d.result,
                        },
                        Err(why) => api::AppCaptureDone {
                            id,
                            error: Some(why),
                            ..api::AppCaptureDone::default()
                        },
                    };
                    let _ = c.call(api::Request::AppsCaptureDone(done)).await;
                }
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// Report Dreamer Web's status on the capture connection. A daemon without
/// `web.component` has nothing that waits on it.
async fn report(c: &client::Client, state: api::WebComponentState) {
    let _ = c.call(api::Request::WebComponent(state)).await;
}

/// Where to capture: the window's bounds on the screen and the pane's
/// rectangle in it, in points.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Target {
    window: Bounds<Pixels>,
    pane: Bounds<Pixels>,
}

/// The page a pane shows, if it shows one (see [`Workspace::page_of`]).
type Page = Option<Entity<crate::web::WebPane>>;

/// Find the window showing `r.pane` and wait (on a timer) until its frame
/// has the tree version asked for; where it is, and its page.
async fn find(
    r: &api::AppCaptureRequest,
    cx: &mut gpui::AsyncApp,
) -> Result<(Target, Page), String> {
    let started = std::time::Instant::now();
    loop {
        let mut found = None;
        let windows = cx.update(|cx| cx.windows());
        for any in windows {
            let Some(handle) = any.downcast::<Workspace>() else {
                continue;
            };
            let shown = handle.update(cx, |ws, window, cx| {
                ws.pane_shown(r.pane, cx)
                    .map(|(pane, painted)| (window.bounds(), pane, painted, ws.page_of(r.pane, cx)))
            });
            if let Ok(Some(s)) = shown {
                found = Some((any, s));
                break;
            }
        }
        // A window just opened in tmux reaches this window's topology a
        // moment later.
        let Some((any, (window, pane, painted, page))) = found else {
            if started.elapsed() > DRAWN_WAIT {
                return Err(format!("pane %{} is not shown in a Dreamer window", r.pane));
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            continue;
        };
        if painted.unwrap_or(u64::MAX) >= r.seq {
            // One more frame, so what was drawn is on the screen.
            let _ = cx.update_window(any, |_, window, _| window.refresh());
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            return Ok((Target { window, pane }, page));
        }
        if started.elapsed() > DRAWN_WAIT {
            return Err(format!(
                "pane %{} did not draw version {} of its view within {} s",
                r.pane,
                r.seq,
                DRAWN_WAIT.as_secs()
            ));
        }
        let _ = cx.update_window(any, |_, window, _| window.refresh());
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// Capture `target` to `path` on a thread of its own; it answers.
fn take(
    id: u64,
    target: Target,
    path: PathBuf,
    answers: tokio::sync::mpsc::UnboundedSender<Answer>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("dreamer-capture-shot".into())
        .spawn(move || {
            let done = capture(target, &path).map(|(width, height)| Done {
                width,
                height,
                ..Done::default()
            });
            let _ = answers.send((id, done));
        })
        .map(drop)
        .map_err(|e| format!("could not start the capture: {e}"))
}

#[cfg(target_os = "macos")]
fn capture(target: Target, path: &Path) -> Result<(u32, u32), String> {
    let id = window_number(target.window)?;
    let shot =
        std::env::temp_dir().join(format!("dreamer-capture-{}-{id}.png", std::process::id()));
    let _ = std::fs::remove_file(&shot);
    let out = std::process::Command::new("screencapture")
        .args(["-x", "-o", &format!("-l{id}")])
        .arg(&shot)
        .output()
        .map_err(|e| format!("screencapture: {e}"))?;
    if !shot.is_file() {
        return Err(format!(
            "the window could not be captured: {} (allow Dreamer, or the terminal that started it, \
             to record the screen in System Settings > Privacy & Security > Screen & System Audio \
             Recording)",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let image = std::fs::read(&shot).map_err(|e| format!("{}: {e}", shot.display()));
    let _ = std::fs::remove_file(&shot);
    let (w, h, px) = crop(&image?, target)?;
    write_png(path, w, h, &px)?;
    Ok((w, h))
}

#[cfg(not(target_os = "macos"))]
fn capture(_: Target, _: &Path) -> Result<(u32, u32), String> {
    Err("this desktop captures panes on macOS only".into())
}

/// The number of this process's window at `bounds` (points, top-left
/// origin) in the window server's list; the closest one when several.
#[cfg(target_os = "macos")]
fn window_number(bounds: Bounds<Pixels>) -> Result<u32, String> {
    let script = format!(
        "ObjC.import('CoreGraphics');\
         const all = ObjC.deepUnwrap(ObjC.castRefToObject(\
           $.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));\
         all.filter(x => x.kCGWindowOwnerPID == {pid} && x.kCGWindowLayer == 0)\
            .map(x => [x.kCGWindowNumber, x.kCGWindowBounds.X, x.kCGWindowBounds.Y,\
                       x.kCGWindowBounds.Width, x.kCGWindowBounds.Height].join(' ')).join('\\n')",
        pid = std::process::id()
    );
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    let want = [
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    ];
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let v: Vec<f32> = l
                .split_whitespace()
                .filter_map(|n| n.parse().ok())
                .collect();
            let [n, x, y, w, h] = v[..] else { return None };
            let off = (x - want[0]).abs()
                + (y - want[1]).abs()
                + (w - want[2]).abs()
                + (h - want[3]).abs();
            Some((n as u32, off))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(n, _)| n)
        .ok_or_else(|| "this desktop's window is not on the screen".to_owned())
}

/// The pane's rectangle of a capture of its whole window, as RGBA rows.
#[cfg(any(target_os = "macos", test))]
fn crop(image: &[u8], target: Target) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(image));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => return Err(format!("unexpected capture format {other:?}")),
    };
    let window_w = f32::from(target.window.size.width).max(1.0);
    let scale = info.width as f32 / window_w;
    let at = |v: Pixels| (f32::from(v) * scale).round().max(0.0) as u32;
    let x0 = at(target.pane.origin.x).min(info.width);
    let y0 = at(target.pane.origin.y).min(info.height);
    let x1 = at(target.pane.origin.x + target.pane.size.width).min(info.width);
    let y1 = at(target.pane.origin.y + target.pane.size.height).min(info.height);
    if x1 <= x0 || y1 <= y0 {
        return Err("the pane is outside its window's capture".into());
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let stride = info.width as usize * channels;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in y0..y1 {
        let row = &buf[y as usize * stride..][..stride];
        for x in x0..x1 {
            let p = &row[x as usize * channels..][..channels];
            out.extend_from_slice(&[p[0], p[1], p[2], if channels == 4 { p[3] } else { 255 }]);
        }
    }
    Ok((w, h, out))
}

/// Write RGBA pixels as a PNG, replacing `path` whole.
#[cfg(any(target_os = "macos", test))]
fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}.png", std::process::id()));
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    {
        let file = std::fs::File::create(&tmp).map_err(|e| fail(&e))?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| fail(&e))?;
        writer.write_image_data(rgba).map_err(|e| fail(&e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| fail(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn a_capture_is_cropped_to_the_pane_at_the_capture_scale() {
        // A 4x2-point window captured at 2x: 8x4 pixels, each pixel's red
        // its column and green its row.
        let (w, h) = (8u32, 4u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }
        let dir = std::env::temp_dir().join(format!("drm-capture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let whole = dir.join("whole.png");
        write_png(&whole, w, h, &rgba).expect("write");
        let target = Target {
            window: Bounds::new(point(px(100.), px(50.)), size(px(4.), px(2.))),
            pane: Bounds::new(point(px(1.), px(1.)), size(px(2.), px(1.))),
        };
        let (cw, ch, px) = crop(&std::fs::read(&whole).expect("read"), target).expect("crop");
        assert_eq!((cw, ch), (4, 2));
        // Columns 2..6, rows 2..4.
        assert_eq!(&px[..4], &[2, 2, 0, 255]);
        assert_eq!(&px[px.len() - 4..], &[5, 3, 0, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
