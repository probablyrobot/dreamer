//! An app's own pages: where they may navigate, the files of the
//! package's `web/` directory served on the app's origin, and the bridge
//! (`window.dreamer.call`) pages on that origin get.
//!
//! The bridge is a script added to every document the view loads, which
//! defines `window.dreamer` only when the document is on the app's origin.
//! Its calls are `fetch` requests to [`BRIDGE_PATH`] on that same origin,
//! which never reach the network: the engine answers them, and only for a
//! request a frame on the app's origin made ([`AppPage::bridge`] then
//! passes the call on). A page on any other origin has no `window.dreamer`,
//! and its requests to the path are refused.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub use view_tree::web::{BRIDGE_PATH, SCHEME, origin};

/// The answer to a bridge call: the result's JSON, or the error's code and
/// message.
pub type Answer = Result<String, (i32, String)>;

/// One call a page made through its bridge.
pub struct BridgeCall {
    pub method: String,
    /// Its parameters as JSON.
    pub params: String,
    /// Where the answer goes; called once, from any thread.
    pub reply: Box<dyn FnOnce(Answer) + Send>,
}

impl std::fmt::Debug for BridgeCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeCall")
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// A page of an app's own, and what holds it to the app.
#[derive(Clone)]
pub struct AppPage {
    /// The app's id: the page gets a browser profile of the app's own.
    pub app: String,
    /// The app's origin: only pages there get the bridge.
    pub origin: String,
    /// The package's `web/` directory, served on `dreamer-app://<app>`.
    pub dir: Option<PathBuf>,
    /// Origins the page may also navigate to.
    pub allow: Vec<String>,
    /// Passes each bridge call on; called on one of Chromium's threads.
    pub bridge: Arc<dyn Fn(BridgeCall) + Send + Sync>,
}

impl std::fmt::Debug for AppPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppPage")
            .field("app", &self.app)
            .field("origin", &self.origin)
            .field("dir", &self.dir)
            .field("allow", &self.allow)
            .finish_non_exhaustive()
    }
}

impl AppPage {
    /// Whether a frame may navigate to `url`: the app's origin, an origin
    /// it allows, or an empty document (`about:blank`, `about:srcdoc`).
    pub fn may_navigate(&self, url: &str) -> bool {
        if url == "about:blank" || url == "about:srcdoc" {
            return true;
        }
        origin(url).is_some_and(|o| o == self.origin || self.allow.contains(&o))
    }

    /// Whether `url` is the bridge, asked for by a frame at `frame_url`
    /// for `initiator` (the origin that made the request): all three on
    /// the app's origin.
    pub fn is_bridge(&self, url: &str, frame_url: &str, initiator: &str) -> bool {
        self.is_bridge_url(url)
            && origin(frame_url).is_some_and(|o| o == self.origin)
            && initiator.trim_end_matches('/') == self.origin
    }

    /// Whether `url` is the bridge's address on the app's origin.
    pub fn is_bridge_url(&self, url: &str) -> bool {
        origin(url).is_some_and(|o| o == self.origin)
            && path_of(url, &self.origin) == Some(BRIDGE_PATH)
    }

    /// The file of the package's `web/` directory that `url` names, if
    /// the app serves its directory and `url` is on its origin there.
    pub fn file(&self, url: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        if self.origin != format!("{SCHEME}://{}", self.app) {
            return None;
        }
        file_in(dir, path_of(url, &self.origin)?)
    }

    /// The script every document gets: `window.dreamer`, on the app's
    /// origin only.
    pub fn bridge_script(&self) -> String {
        let origin = serde_json::to_string(&self.origin).unwrap_or_default();
        let path = serde_json::to_string(BRIDGE_PATH).unwrap_or_default();
        format!(
            r#"(() => {{
  if (location.origin !== {origin}) return;
  const call = async (method, params) => {{
    const r = await fetch({path}, {{
      method: "POST",
      headers: {{ "content-type": "application/json" }},
      body: JSON.stringify({{ method: String(method), params: params === undefined ? {{}} : params }}),
    }});
    const v = await r.json();
    if (v.error) {{
      const e = new Error(v.error.message);
      e.code = v.error.code;
      throw e;
    }}
    return v.result;
  }};
  Object.defineProperty(window, "dreamer", {{ value: Object.freeze({{ call }}) }});
}})();"#
        )
    }
}

/// The path of `url` on `origin` (without query or fragment), if `url` is
/// on it.
fn path_of<'a>(url: &'a str, origin: &str) -> Option<&'a str> {
    let rest = url.get(origin.len()..)?;
    let on = url.get(..origin.len())?.eq_ignore_ascii_case(origin);
    if !on || !(rest.is_empty() || rest.starts_with(['/', '?', '#'])) {
        return None;
    }
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    Some(if path.is_empty() { "/" } else { path })
}

/// The file `path` (from a URL, percent-encoded) names in `dir`: plain
/// names only, and not through a link out of `dir`.
fn file_in(dir: &Path, path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(path.trim_start_matches('/'))?;
    let rel = Path::new(&decoded);
    if decoded.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let file = root.join(rel).canonicalize().ok()?;
    (file.starts_with(&root) && file.is_file()).then_some(file)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The content type of a served file, by its extension.
pub fn mime(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("html" | "htm") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// A bridge request's body: `{"method": ..., "params": ...}`, as the
/// method's name and its parameters' JSON.
pub fn bridge_request(body: &[u8]) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("not a bridge call: {e}"))?;
    let method = v
        .get("method")
        .and_then(serde_json::Value::as_str)
        .filter(|m| !m.is_empty())
        .ok_or("a bridge call names its method")?;
    let params = v.get("params").cloned().unwrap_or(serde_json::json!({}));
    Ok((method.to_owned(), params.to_string()))
}

/// The JSON a page's bridge call gets back: `{"result": ...}` or
/// `{"error": {"code", "message"}}`.
pub fn bridge_reply(answer: &Answer) -> String {
    let v = match answer {
        Ok(result) => {
            let result: serde_json::Value =
                serde_json::from_str(result).unwrap_or(serde_json::Value::Null);
            serde_json::json!({ "result": result })
        }
        Err((code, message)) => {
            serde_json::json!({ "error": { "code": code, "message": message } })
        }
    };
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(dir: Option<PathBuf>) -> AppPage {
        AppPage {
            app: "test.web".into(),
            origin: "dreamer-app://test.web".into(),
            dir,
            allow: vec!["https://example.com".into()],
            bridge: Arc::new(|_| {}),
        }
    }

    #[test]
    fn a_page_navigates_only_to_its_origin_and_the_allowed_ones() {
        let p = page(None);
        assert!(p.may_navigate("dreamer-app://test.web/other.html"));
        assert!(p.may_navigate("https://example.com/docs"));
        assert!(p.may_navigate("about:srcdoc"));
        for no in [
            "https://example.org/",
            "http://example.com/",
            "dreamer-app://other.app/index.html",
            "data:text/html,hi",
            "file:///etc/passwd",
        ] {
            assert!(!p.may_navigate(no), "{no}");
        }
    }

    #[test]
    fn only_the_apps_own_frames_reach_the_bridge() {
        let p = page(None);
        let own = "dreamer-app://test.web/index.html";
        let bridge = "dreamer-app://test.web/.dreamer/call";
        assert!(p.is_bridge(bridge, own, "dreamer-app://test.web"));
        // Another origin's frame, or a request it started, gets nothing.
        assert!(!p.is_bridge(bridge, "https://example.com/", "dreamer-app://test.web"));
        assert!(!p.is_bridge(bridge, own, "https://example.com"));
        assert!(!p.is_bridge(bridge, "about:srcdoc", "null"));
        assert!(!p.is_bridge("dreamer-app://test.web/x", own, "dreamer-app://test.web"));
        assert!(!p.is_bridge(
            "https://example.com/.dreamer/call",
            own,
            "dreamer-app://test.web"
        ));
    }

    #[test]
    fn served_files_stay_in_the_web_directory() {
        let root = std::env::temp_dir().join(format!("web-site-{}", std::process::id()));
        let dir = root.join("web");
        std::fs::create_dir_all(dir.join("a b")).expect("dir");
        std::fs::write(dir.join("index.html"), "x").expect("file");
        std::fs::write(dir.join("a b/c.js"), "x").expect("file");
        std::fs::write(root.join("secret.txt"), "x").expect("file");
        let p = page(Some(dir.clone()));
        let at = |path: &str| p.file(&format!("dreamer-app://test.web{path}"));
        assert!(at("/index.html?x=1#y").is_some_and(|f| f.ends_with("index.html")));
        assert!(at("/a%20b/c.js").is_some_and(|f| f.ends_with("a b/c.js")));
        for no in ["/../secret.txt", "/%2e%2e/secret.txt", "/", "/missing.html"] {
            assert!(at(no).is_none(), "{no}");
        }
        assert!(p.file("dreamer-app://other.app/index.html").is_none());
        assert_eq!(mime(Path::new("x/app.MJS")), "text/javascript");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bridge_calls_and_answers_are_json() {
        assert_eq!(
            bridge_request(br#"{"method":"tmux.topology"}"#),
            Ok(("tmux.topology".into(), "{}".into()))
        );
        assert!(bridge_request(b"{}").is_err());
        assert_eq!(
            bridge_reply(&Ok(r#"{"panes":[]}"#.into())),
            r#"{"result":{"panes":[]}}"#
        );
        assert_eq!(
            bridge_reply(&Err((-32005, "no".into()))),
            r#"{"error":{"code":-32005,"message":"no"}}"#
        );
    }
}
