//! The `web.*` methods that read and drive a page (`web.read`,
//! `web.screenshot`, `web.click`, `web.type`), run in this desktop for the
//! daemon (see capture.rs), and the image of a page `apps.capture` takes.
//!
//! Each runs on the page through Chromium's DevTools protocol, with a
//! fixed set of its methods; nothing the caller sends becomes a DevTools
//! method or a script, and the clipboard is never used:
//! - **read:** the body's `innerText`, and with `tree` the accessibility
//!   tree (`Accessibility.getFullAXTree`), listed in document order
//!   without ignored, generic, and inline text nodes. A node's id is its
//!   element's DevTools backend node id, which stays the element's while
//!   the page is loaded.
//! - **click:** the element named by node id or CSS selector (its first
//!   match) is scrolled into view, and a left click goes to the middle of
//!   its first box (`Input.dispatchMouseEvent`), as a mouse would.
//! - **type:** the named element is focused (`DOM.focus`), the text
//!   inserted at the caret (`Input.insertText`), and with `submit` Enter
//!   pressed (`Input.dispatchKeyEvent`), which submits a form from its
//!   field.
//! - **screenshot:** `Page.captureScreenshot`, the page alone at the
//!   display's scale, written as a PNG file off the UI thread.
#![cfg_attr(not(feature = "web"), allow(dead_code))]

use serde_json::Value;

/// The most text and nodes `web.read` returns.
const MAX_TEXT: usize = 100_000;
const MAX_NODES: usize = 2_000;

/// Roles the tree leaves out: they only group or split text.
const SKIPPED: [&str; 4] = ["generic", "none", "InlineTextBox", "LineBreak"];

/// The accessibility tree of a `Accessibility.getFullAXTree` reply as
/// `web.read` lists it, and whether it was cut at [`MAX_NODES`].
fn tree(reply: &Value) -> (Vec<api::WebNode>, bool) {
    let nodes = reply
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let id = |n: &Value| n.get("nodeId").and_then(Value::as_str).map(str::to_owned);
    let by_id: std::collections::HashMap<String, &Value> =
        nodes.iter().filter_map(|n| Some((id(n)?, n))).collect();
    let text = |n: &Value, k: &str| {
        n.pointer(&format!("/{k}/value"))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    // Depth first from the roots, in the order the reply lists children.
    let mut stack: Vec<(&Value, u32)> = nodes
        .iter()
        .filter(|n| n.get("parentId").is_none())
        .rev()
        .map(|n| (n, 0))
        .collect();
    while let Some((n, depth)) = stack.pop() {
        let role = text(n, "role");
        let ignored = n.get("ignored").and_then(Value::as_bool).unwrap_or(false);
        let kept = !ignored && !SKIPPED.contains(&role.as_str());
        if kept {
            if out.len() == MAX_NODES {
                return (out, true);
            }
            out.push(api::WebNode {
                node: n
                    .get("backendDOMNodeId")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                depth,
                role,
                name: text(n, "name"),
                value: text(n, "value"),
            });
        }
        let below = depth + u32::from(kept);
        let children = n
            .get("childIds")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for c in children.iter().rev() {
            if let Some(c) = c.as_str().and_then(|c| by_id.get(c)) {
                stack.push((c, below));
            }
        }
    }
    (out, false)
}

/// The middle of the first box of `DOM.getContentQuads`'s reply that has
/// an area, in the page's CSS pixels.
fn middle(reply: &Value) -> Option<(f64, f64)> {
    reply
        .get("quads")?
        .as_array()?
        .iter()
        .filter_map(|q| {
            let p: Vec<f64> = q.as_array()?.iter().filter_map(Value::as_f64).collect();
            let [x1, y1, x2, y2, x3, y3, x4, y4] = p[..] else {
                return None;
            };
            let area = ((x1 * y2 - x2 * y1)
                + (x2 * y3 - x3 * y2)
                + (x3 * y4 - x4 * y3)
                + (x4 * y1 - x1 * y4))
                .abs()
                / 2.0;
            (area >= 1.0).then_some(((x1 + x2 + x3 + x4) / 4.0, (y1 + y2 + y3 + y4) / 4.0))
        })
        .next()
}

/// At most [`MAX_TEXT`] characters of `text`, and whether it was cut.
fn bounded(text: &str) -> (String, bool) {
    match text.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => (text[..at].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// A PNG's width and height, from its header.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.get(..8)? != b"\x89PNG\r\n\x1a\n" || png.get(12..16)? != b"IHDR" {
        return None;
    }
    let n = |at: usize| Some(u32::from_be_bytes(png.get(at..at + 4)?.try_into().ok()?));
    Some((n(16)?, n(20)?))
}

/// Write `png` (base64, as DevTools sends it) to `path`, replacing it
/// whole; its width and height.
fn write_png(data: &str, path: &std::path::Path) -> Result<(u32, u32), String> {
    let png = api::json::decode_base64(data)?;
    let size = png_size(&png).ok_or("the page's image is not a PNG")?;
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let tmp = path.with_extension(format!("tmp-{}.png", std::process::id()));
    std::fs::write(&tmp, &png).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)?;
    Ok(size)
}

#[cfg(feature = "web")]
pub(crate) use engine::{answer, image};

#[cfg(not(feature = "web"))]
pub(crate) async fn answer(
    _: Option<gpui::Entity<crate::web::WebPane>>,
    _: &api::AppCaptureRequest,
    _: &mut gpui::AsyncApp,
) -> Result<crate::capture::Done, String> {
    Err("this build of Dreamer has no web engine".into())
}

#[cfg(feature = "web")]
mod engine {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use gpui::{AsyncApp, Entity};
    use serde_json::{Value, json};

    use super::{bounded, middle, tree, write_png};
    use crate::capture::Done;
    use crate::web::WebPane;

    /// How long one DevTools call, or the page's load, may take.
    const CALL: Duration = Duration::from_secs(10);

    /// Run the web method `r` asks for on `page`.
    pub(crate) async fn answer(
        page: Option<Entity<WebPane>>,
        r: &api::AppCaptureRequest,
        cx: &mut AsyncApp,
    ) -> Result<Done, String> {
        let page = page.ok_or_else(|| format!("pane %{} shows no web page", r.pane))?;
        let params: Value = serde_json::from_str(&r.params).map_err(|e| e.to_string())?;
        let request = api::Request::from_json(&r.method, params).map_err(|e| e.to_string())?;
        loaded(&page, cx).await?;
        let result = match request {
            api::Request::WebRead(p) => api::Response::WebRead(read(&page, p.tree, cx).await?),
            api::Request::WebScreenshot(p) => {
                let (width, height) = image(&page, Path::new(&p.path), cx).await?;
                api::Response::WebScreenshot(api::AppCaptured {
                    path: p.path,
                    width,
                    height,
                })
            }
            api::Request::WebClick(p) => {
                let target = target(&page, p.node, p.selector.as_deref(), cx)
                    .await?
                    .ok_or("name the element to click: node or selector")?;
                click(&page, &target, cx).await?;
                api::Response::WebClick(api::Empty {})
            }
            api::Request::WebType(p) => {
                if let Some(t) = target(&page, p.node, p.selector.as_deref(), cx).await? {
                    call(&page, "DOM.focus", t, cx).await?;
                }
                if !p.text.is_empty() {
                    call(&page, "Input.insertText", json!({ "text": p.text }), cx).await?;
                }
                if p.submit {
                    enter(&page, cx).await?;
                }
                api::Response::WebType(api::Empty {})
            }
            other => return Err(format!("{} is not a web method", other.method())),
        };
        Ok(Done {
            result: result.to_json().to_string(),
            ..Done::default()
        })
    }

    /// One DevTools call on the page; its result.
    async fn call(
        page: &Entity<WebPane>,
        method: &str,
        params: Value,
        cx: &mut AsyncApp,
    ) -> Result<Value, String> {
        let (tx, rx) = async_channel::bounded(1);
        let sent = cx.update(|cx| {
            page.read(cx).view.as_ref().map(|v| {
                v.call(method, &params, move |r| {
                    let _ = tx.try_send(r);
                });
            })
        });
        if sent.is_none() {
            return Err("the page is not open".into());
        }
        let timer = cx.background_executor().timer(CALL);
        futures_lite::future::or(
            async {
                rx.recv()
                    .await
                    .unwrap_or_else(|_| Err("the page closed".into()))
            },
            async {
                timer.await;
                Err(format!("the page did not answer {method}"))
            },
        )
        .await
        .map_err(|e| format!("{method}: {e}"))
    }

    /// Wait until the page has loaded once, at most [`CALL`].
    async fn loaded(page: &Entity<WebPane>, cx: &mut AsyncApp) -> Result<(), String> {
        let started = Instant::now();
        loop {
            let state = cx.update(|cx| {
                let p = page.read(cx);
                match (&p.view, p.not_drawn()) {
                    (Some(v), _) => Ok(v.load_status().is_some()),
                    (None, Some(why)) => Err(why),
                    (None, None) => Ok(false),
                }
            })?;
            if state {
                return Ok(());
            }
            if started.elapsed() > CALL {
                return Err("the page did not load".into());
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    async fn read(
        page: &Entity<WebPane>,
        with_tree: bool,
        cx: &mut AsyncApp,
    ) -> Result<api::WebPage, String> {
        let r = call(
            page,
            "Runtime.evaluate",
            json!({
                "expression": "(document.body || document.documentElement).innerText",
                "returnByValue": true,
            }),
            cx,
        )
        .await?;
        let (text, mut truncated) = bounded(
            r.pointer("/result/value")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let (url, title) = cx.update(|cx| {
            page.read(cx)
                .view
                .as_ref()
                .map(|v| (v.url(), v.title()))
                .unwrap_or_default()
        });
        let mut nodes = Vec::new();
        if with_tree {
            let reply = call(page, "Accessibility.getFullAXTree", json!({}), cx).await?;
            let cut;
            (nodes, cut) = tree(&reply);
            truncated |= cut;
        }
        Ok(api::WebPage {
            url,
            title,
            text,
            nodes,
            truncated,
        })
    }

    /// The element `node` or `selector` names, as DevTools names a node in
    /// a call's parameters; `None` when neither is given.
    async fn target(
        page: &Entity<WebPane>,
        node: Option<u64>,
        selector: Option<&str>,
        cx: &mut AsyncApp,
    ) -> Result<Option<Value>, String> {
        match (node, selector) {
            (Some(_), Some(_)) => Err("name the element by node or by selector, not both".into()),
            (Some(0), None) => Err("node 0 is not an element".into()),
            (Some(n), None) => Ok(Some(json!({ "backendNodeId": n }))),
            (None, Some(s)) => {
                // The selector is a string in the expression, never code.
                let expression = format!("document.querySelector({})", Value::from(s));
                let r = call(
                    page,
                    "Runtime.evaluate",
                    json!({ "expression": expression }),
                    cx,
                )
                .await?;
                if let Some(e) = r.pointer("/exceptionDetails/exception/description") {
                    let why = e.as_str().unwrap_or_default().lines().next().unwrap_or("");
                    return Err(format!("selector `{s}`: {why}"));
                }
                let id = r
                    .pointer("/result/objectId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("no element matches `{s}`"))?;
                Ok(Some(json!({ "objectId": id })))
            }
            (None, None) => Ok(None),
        }
    }

    async fn click(
        page: &Entity<WebPane>,
        target: &Value,
        cx: &mut AsyncApp,
    ) -> Result<(), String> {
        call(page, "DOM.scrollIntoViewIfNeeded", target.clone(), cx).await?;
        let quads = call(page, "DOM.getContentQuads", target.clone(), cx).await?;
        let (x, y) = middle(&quads).ok_or("the element has no box on the page to click")?;
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let mut event = json!({ "type": kind, "x": x, "y": y });
            if kind != "mouseMoved" {
                event["button"] = json!("left");
                event["buttons"] = json!(buttons);
                event["clickCount"] = json!(1);
            }
            call(page, "Input.dispatchMouseEvent", event, cx).await?;
        }
        Ok(())
    }

    async fn enter(page: &Entity<WebPane>, cx: &mut AsyncApp) -> Result<(), String> {
        let key = json!({
            "key": "Enter",
            "code": "Enter",
            "windowsVirtualKeyCode": 13,
            "nativeVirtualKeyCode": 13,
        });
        let mut down = key.clone();
        down["type"] = json!("keyDown");
        down["text"] = json!("\r");
        down["unmodifiedText"] = json!("\r");
        call(page, "Input.dispatchKeyEvent", down, cx).await?;
        let mut up = key;
        up["type"] = json!("keyUp");
        call(page, "Input.dispatchKeyEvent", up, cx).await?;
        Ok(())
    }

    /// The page as Chromium draws it, written to `path` as a PNG; its
    /// width and height.
    pub(crate) async fn image(
        page: &Entity<WebPane>,
        path: &Path,
        cx: &mut AsyncApp,
    ) -> Result<(u32, u32), String> {
        loaded(page, cx).await?;
        let r = call(
            page,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            cx,
        )
        .await?;
        let data = r
            .get("data")
            .and_then(Value::as_str)
            .ok_or("no image in the reply")?
            .to_owned();
        let path = PathBuf::from(path);
        cx.background_executor()
            .spawn(async move { write_png(&data, &path) })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_tree_lists_meaningful_nodes_in_document_order_with_their_depth() {
        let ax = json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "Form" },
              "childIds": ["2", "5"], "backendDOMNodeId": 1 },
            { "nodeId": "2", "parentId": "1", "role": { "value": "generic" },
              "childIds": ["3", "4"], "backendDOMNodeId": 7 },
            { "nodeId": "3", "parentId": "2", "role": { "value": "textbox" },
              "name": { "value": "Name" }, "value": { "value": "Ada" }, "backendDOMNodeId": 9 },
            { "nodeId": "4", "parentId": "2", "ignored": true, "role": { "value": "button" } },
            { "nodeId": "5", "parentId": "1", "role": { "value": "button" },
              "name": { "value": "Send" }, "childIds": ["6"], "backendDOMNodeId": 11 },
            { "nodeId": "6", "parentId": "5", "role": { "value": "InlineTextBox" } },
        ]});
        let (nodes, cut) = tree(&ax);
        assert!(!cut);
        let got: Vec<(u64, u32, &str, &str, &str)> = nodes
            .iter()
            .map(|n| {
                (
                    n.node,
                    n.depth,
                    n.role.as_str(),
                    n.name.as_str(),
                    n.value.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (1, 0, "RootWebArea", "Form", ""),
                (9, 1, "textbox", "Name", "Ada"),
                (11, 1, "button", "Send", ""),
            ]
        );
    }

    #[test]
    fn a_click_goes_to_the_middle_of_the_first_box_with_an_area() {
        let quads = json!({ "quads": [
            [5, 5, 5, 5, 5, 5, 5, 5],
            [10, 20, 30, 20, 30, 40, 10, 40],
        ]});
        assert_eq!(middle(&quads), Some((20.0, 30.0)));
        assert_eq!(middle(&json!({ "quads": [] })), None);
    }

    #[test]
    fn text_is_cut_at_its_bound_on_a_character() {
        let long = "é".repeat(MAX_TEXT + 3);
        let (text, cut) = bounded(&long);
        assert!(cut);
        assert_eq!(text.chars().count(), MAX_TEXT);
        assert_eq!(bounded("short"), ("short".to_owned(), false));
    }
}
