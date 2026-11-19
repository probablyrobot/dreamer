//! The browser: which addresses a web pane shows, what a typed address
//! means, and its profile (the cookies, storage, and cache of web panes),
//! kept in the desktop's state directory apart from the user's own
//! browser, so logins survive restarts. `dream browser clear` empties it.
//!
//! Chromium holds the profile while a desktop runs it (its process lock,
//! `SingletonLock`, names the holding process), so it is never removed
//! from under a running desktop: the request is left beside it instead,
//! and the desktop empties the profile before Chromium next opens it.

use std::path::{Path, PathBuf};

/// The longest URL a web pane takes.
pub const WEB_URL_MAX: usize = 8192;

/// Whether a web pane may show `url`: `http`, `https`, `file`, and `data`
/// URLs, and `about:blank`; no scripts as URLs, no control characters.
pub fn web_url_allowed(url: &str) -> bool {
    if url.len() > WEB_URL_MAX || url.chars().any(char::is_control) {
        return false;
    }
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    url == "about:blank"
        || ["http://", "https://", "file://", "data:"]
            .iter()
            .any(|p| lower.starts_with(p))
}

/// The URL a typed address means, for the browser's address bar and
/// `dream browser`: a URL a web pane may show stays as it is; an absolute
/// path is a `file:` URL; `localhost`, a loopback address, or anything with
/// a port is `http:`; a host name with a dot is `https:`. Anything else (a
/// search, another scheme) is refused with why: there is no search engine.
pub fn web_address(typed: &str) -> Result<String, String> {
    let t = typed.trim();
    if t.is_empty() {
        return Err("type an address".into());
    }
    if web_url_allowed(t) {
        return Ok(t.to_owned());
    }
    let refused = || Err(format!("`{t}` is not a web address"));
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refused();
    }
    if t.starts_with('/') {
        return Ok(format!("file://{t}"));
    }
    let host_end = t.find(['/', '?', '#']).unwrap_or(t.len());
    let host_port = &t[..host_end];
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, _)) if !h.ends_with(']') && h.contains(':') => (host_port, None),
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    // `mailto:x`, `chrome://…`: a scheme, not a host and port.
    if port.is_some_and(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return refused();
    }
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '[' | ']' | ':'));
    if !host_ok {
        return refused();
    }
    let local = host == "localhost" || host.starts_with("127.") || host == "[::1]";
    let scheme = if local || port.is_some() {
        "http"
    } else if host.contains('.') && !host.starts_with('.') && !host.ends_with('.') {
        "https"
    } else {
        return refused();
    };
    let url = format!("{scheme}://{t}");
    if web_url_allowed(&url) {
        Ok(url)
    } else {
        refused()
    }
}

/// An `http` or `https` URL whose host is the loopback (`localhost`,
/// `127.x.y.z`, or `[::1]`): what comes before the port, the port (the
/// scheme's default when none is written), the scheme's default port, and
/// what follows the port. `None` for anything else, and for a URL with
/// user information.
fn loopback(url: &str) -> Option<(&str, u16, u16, &str)> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let (start, default) = if lower.starts_with("http://") {
        (7, 80)
    } else if lower.starts_with("https://") {
        (8, 443)
    } else {
        return None;
    };
    let end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| start + i);
    let authority = &url[start..end];
    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']')?;
        authority.split_at(close + 1)
    } else {
        match authority.rfind(':') {
            Some(i) => authority.split_at(i),
            None => (authority, ""),
        }
    };
    let lower = host.to_ascii_lowercase();
    let v4 = lower.strip_prefix("127.").is_some_and(|r| {
        r.split('.').count() == 3 && r.split('.').all(|o| o.parse::<u8>().is_ok())
    });
    if !(lower == "localhost" || lower == "[::1]" || v4) {
        return None;
    }
    let port = match port.strip_prefix(':') {
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
        None if port.is_empty() => default,
        None => return None,
    };
    Some((&url[..start + host.len()], port, default, &url[end..]))
}

/// The port of a URL on the loopback: a page that a web pane in a remote
/// host's session reaches through a forward to that host.
pub fn loopback_port(url: &str) -> Option<u16> {
    loopback(url).map(|(_, port, _, _)| port)
}

/// The same loopback URL on another port, written without one when that
/// is the scheme's default. `None` when `url` is not on the loopback.
pub fn on_port(url: &str, port: u16) -> Option<String> {
    let (before, _, default, after) = loopback(url)?;
    Some(if port == default {
        format!("{before}{after}")
    } else {
        format!("{before}:{port}{after}")
    })
}

/// Chromium's profile directory: `web/` in the desktop's state directory.
pub fn web_dir() -> Option<PathBuf> {
    crate::desktop_state::state_dir().map(|d| d.join("web"))
}

/// The browser's own profile, inside Chromium's.
pub const PROFILE: &str = "browser";
/// Left beside the profile when it cannot be emptied now.
const CLEAR_ASKED: &str = "browser.clear";
/// Chromium's process lock in its profile directory: a link to
/// `HOST-PID`.
const LOCK: &str = "SingletonLock";

/// What `dream browser clear` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Cleared {
    /// The profile is empty now.
    Now,
    /// A desktop has it open (this process id): emptied when Chromium
    /// next starts.
    Later(u32),
}

/// The process holding Chromium's profile directory `web`, if one is alive.
fn holder(web: &Path, alive: impl Fn(u32) -> bool) -> Option<u32> {
    let target = std::fs::read_link(web.join(LOCK)).ok()?;
    let pid = target.to_str()?.rsplit_once('-')?.1.parse().ok()?;
    alive(pid).then_some(pid)
}

/// Empty the browser's profile in Chromium's profile directory `web`, or,
/// while a live process holds it, ask for that to happen when Chromium
/// next starts.
pub fn clear(web: &Path, alive: impl Fn(u32) -> bool) -> Result<Cleared, String> {
    if let Some(pid) = holder(web, alive) {
        std::fs::write(web.join(CLEAR_ASKED), b"")
            .map_err(|e| format!("{}: {e}", web.display()))?;
        return Ok(Cleared::Later(pid));
    }
    remove(&web.join(PROFILE))?;
    let _ = std::fs::remove_file(web.join(CLEAR_ASKED));
    Ok(Cleared::Now)
}

/// Before Chromium starts on `web`: empty the browser's profile if that
/// was asked for while it ran.
pub fn clear_if_asked(web: &Path) -> Result<bool, String> {
    if !web.join(CLEAR_ASKED).exists() {
        return Ok(false);
    }
    remove(&web.join(PROFILE))?;
    std::fs::remove_file(web.join(CLEAR_ASKED)).map_err(|e| format!("{}: {e}", web.display()))?;
    Ok(true)
}

fn remove(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLEAR_ASKED, Cleared, LOCK, PROFILE, clear, clear_if_asked, loopback_port, on_port,
        web_address,
    };

    #[test]
    fn loopback_urls_move_to_another_port_and_nothing_else_does() {
        assert_eq!(loopback_port("http://localhost:3000/a?b#c"), Some(3000));
        assert_eq!(loopback_port("https://LOCALHOST/"), Some(443));
        assert_eq!(loopback_port("http://127.0.0.1"), Some(80));
        assert_eq!(loopback_port("http://[::1]:8080/x"), Some(8080));
        for not in [
            "http://example.com:3000/",
            "http://localhost.example.com/",
            "http://127.0.0.1.nip.io/",
            "http://user@localhost:3000/",
            "http://localhost:0/",
            "http://localhost:99999/",
            "http://localhost:x/",
            "file:///tmp/a",
            "ftp://localhost/",
        ] {
            assert_eq!(loopback_port(not), None, "{not}");
        }
        assert_eq!(
            on_port("http://localhost:3000/a?b", 49152).as_deref(),
            Some("http://localhost:49152/a?b")
        );
        assert_eq!(
            on_port("http://[::1]/x", 5000).as_deref(),
            Some("http://[::1]:5000/x")
        );
        assert_eq!(
            on_port("http://localhost:49152/", 80).as_deref(),
            Some("http://localhost/")
        );
        assert_eq!(on_port("https://example.com/", 1), None);
    }

    #[test]
    fn a_typed_address_becomes_the_url_it_means_or_is_refused() {
        for (typed, url) in [
            ("https://example.com/a", "https://example.com/a"),
            ("  example.com ", "https://example.com"),
            (
                "news.ycombinator.com/item?id=1",
                "https://news.ycombinator.com/item?id=1",
            ),
            ("localhost:3000", "http://localhost:3000"),
            ("localhost", "http://localhost"),
            ("127.0.0.1:8080/x", "http://127.0.0.1:8080/x"),
            ("[::1]:5173", "http://[::1]:5173"),
            ("dev-box:8000", "http://dev-box:8000"),
            ("/tmp/page.html", "file:///tmp/page.html"),
            ("data:text/html,<p>hi</p>", "data:text/html,<p>hi</p>"),
            ("about:blank", "about:blank"),
        ] {
            assert_eq!(web_address(typed).as_deref(), Ok(url), "{typed}");
        }
        for typed in [
            "",
            "   ",
            "how to rust",
            "intranet",
            "javascript:alert(1)",
            "chrome://settings",
            "mailto:a@b.c",
            "ftp://example.com",
            "example.com/\u{7}",
            "about:config",
            ".com",
        ] {
            assert!(web_address(typed).is_err(), "{typed}");
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dreamer-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(PROFILE).join("Default")).expect("profile");
        std::fs::write(d.join(PROFILE).join("Default/Cookies"), b"x").expect("cookies");
        d
    }

    #[test]
    fn clearing_empties_the_profile_unless_a_desktop_holds_it() {
        let web = scratch("clear");
        // Nobody holds it (or the holder is gone): emptied at once.
        std::os::unix::fs::symlink("host-4242", web.join(LOCK)).expect("lock");
        assert_eq!(clear(&web, |_| false), Ok(Cleared::Now));
        assert!(!web.join(PROFILE).exists());
        let _ = std::fs::remove_dir_all(&web);

        // A live desktop holds it: left alone, and emptied before
        // Chromium next opens it.
        let web = scratch("held");
        std::os::unix::fs::symlink("my-host.local-4242", web.join(LOCK)).expect("lock");
        assert_eq!(clear(&web, |p| p == 4242), Ok(Cleared::Later(4242)));
        assert!(web.join(PROFILE).join("Default/Cookies").exists());
        assert!(web.join(CLEAR_ASKED).exists());
        assert_eq!(clear_if_asked(&web), Ok(true));
        assert!(!web.join(PROFILE).exists() && !web.join(CLEAR_ASKED).exists());
        // Nothing asked: the profile stays.
        std::fs::create_dir_all(web.join(PROFILE)).expect("profile");
        assert_eq!(clear_if_asked(&web), Ok(false));
        assert!(web.join(PROFILE).exists());
        let _ = std::fs::remove_dir_all(&web);
    }
}
