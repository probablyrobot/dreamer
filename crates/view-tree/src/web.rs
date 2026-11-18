//! Where a `web` node's page comes from, and which origin is the app's.
//!
//! An app's own origin is its declared local `url`'s (a dev server), or
//! else `dreamer-app://<app id>`, on which the desktop serves the
//! package's `web/` directory. Only pages on that origin get the bridge
//! (`window.dreamer.call`), which they reach at [`BRIDGE_PATH`] on their
//! own origin; a page may navigate only there and to the origins the
//! manifest allows.

use serde::{Deserialize, Serialize};

/// A `web` node's page as the app host resolved it: its address, the
/// app's origin and id, the package's `web/` directory, and the origins
/// it may navigate to besides its own. Empty until resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

/// The scheme the desktop serves an app package's `web/` directory on.
pub const SCHEME: &str = "dreamer-app";

/// Where a page on the app's origin posts its bridge calls.
pub const BRIDGE_PATH: &str = "/.dreamer/call";

/// The origin of `url`: `scheme://host[:port]`, lowercase, without a
/// default port. `None` for a URL without a host (`data:`, `about:`,
/// `file:`) or with user info.
pub fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.');
    if scheme.is_empty() || !scheme.chars().all(valid) {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    let authority = match (scheme.as_str(), authority.rsplit_once(':')) {
        ("http", Some((host, "80"))) | ("https", Some((host, "443"))) => host.to_owned(),
        _ => authority,
    };
    Some(format!("{scheme}://{authority}"))
}

/// Whether `url` is an address on this machine: `http` or `https` on
/// `localhost`, `127.0.0.1`, or `[::1]`, as a dev server is.
pub fn is_local(url: &str) -> bool {
    let Some(o) = origin(url) else { return false };
    let Some(host) = o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"))
    else {
        return false;
    };
    let host = match host.strip_prefix("[::1]") {
        Some(rest) => return rest.is_empty() || rest.starts_with(':'),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1")
}

/// The app's own origin: its declared `url`'s, else its package's.
pub fn app_origin(id: &str, url: Option<&str>) -> Option<String> {
    match url {
        Some(u) => origin(u),
        None => Some(format!("{SCHEME}://{id}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_scheme_host_and_port() {
        assert_eq!(
            origin("http://LocalHost:5173/a/b?c#d").as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(
            origin("https://example.com:443/").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            origin("dreamer-app://local.panes/index.html").as_deref(),
            Some("dreamer-app://local.panes")
        );
        for none in [
            "data:text/html,x",
            "about:blank",
            "file:///etc",
            "https://u@h/",
        ] {
            assert_eq!(origin(none), None, "{none}");
        }
    }

    #[test]
    fn local_means_this_machine() {
        for local in [
            "http://localhost:5173",
            "https://127.0.0.1/x",
            "http://[::1]:8080/",
        ] {
            assert!(is_local(local), "{local}");
        }
        for remote in [
            "http://localhost.example.com",
            "https://example.com",
            "file:///x",
        ] {
            assert!(!is_local(remote), "{remote}");
        }
    }
}
