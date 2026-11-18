//! The browser: what a plain web page's view does beyond showing it.
//!
//! - **Its own profile** (`browser` in Chromium's profile directory),
//!   session cookies kept, so logins survive restarts.
//! - **No Dreamer API.** An arbitrary site never gets the bridge: only an
//!   app's page has one ([`crate::site`]), and the browser's own script
//!   runs in an isolated world the page cannot see.
//! - **Back and forward by mouse and swipe:** mouse buttons 4 and 5, and a
//!   two-finger swipe sideways past the page's edge, go back and forward
//!   in the pane's history ([`NAVIGATION_SCRIPT`]). The keys are the
//!   desktop's.
//! - **One page per pane:** a link that wants a new window opens in the
//!   same pane.
//! - **Downloads are refused,** with a line in the pane's status
//!   ([`download_refused`]). Saving them is planned behind the
//!   [`DOWNLOAD_CAPABILITY`]; until then the handler only refuses.

/// The browser's profile, a directory of Chromium's.
pub const PROFILE: &str = "browser";

/// The capability that will let a page save downloads (to a configured
/// folder, after a prompt). Not granted to anything yet: every download
/// is refused.
pub const DOWNLOAD_CAPABILITY: &str = "web.download";

/// What the browser does with a download a page starts: refuse it, for
/// now. Planned: ask, then save it, behind [`DOWNLOAD_CAPABILITY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downloads {
    Refused,
}

/// The download policy in force.
pub const DOWNLOADS: Downloads = Downloads::Refused;

/// The pane's status line for a download of `url` (or a suggested file
/// name) that was refused.
pub fn download_refused(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url);
    let name: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
    format!("Download refused: {name}. Downloads are off for now.")
}

/// The isolated world the browser's own script runs in.
pub const WORLD: &str = "dreamer-browser";

/// Back and forward from the mouse and the trackpad, in every document of
/// a plain page, in an isolated world: the page sees no global of it, and
/// it calls nothing of Dreamer's. Mouse button 4 goes back and 5 forward.
/// A two-finger swipe sideways that the page does not scroll (the page,
/// and every element under the pointer, at its edge that way) goes back
/// (to the right) or forward (to the left), once per gesture.
pub const NAVIGATION_SCRIPT: &str = r#"(() => {
  if (globalThis.__dreamerNavigation) return;
  globalThis.__dreamerNavigation = true;
  addEventListener("mouseup", (e) => {
    if (e.button === 3 || e.button === 4) {
      e.preventDefault();
      e.button === 3 ? history.back() : history.forward();
    }
  }, true);
  const canScroll = (el, dx) => {
    for (let n = el; n && n.nodeType === 1; n = n.parentElement) {
      const s = getComputedStyle(n);
      const scrolls = n === document.scrollingElement || /(auto|scroll)/.test(s.overflowX);
      if (!scrolls || n.scrollWidth <= n.clientWidth) continue;
      if (dx < 0 ? n.scrollLeft > 0 : n.scrollLeft + n.clientWidth < n.scrollWidth - 1) return true;
    }
    const d = document.scrollingElement;
    return !!d && (dx < 0 ? d.scrollLeft > 0 : d.scrollLeft + d.clientWidth < d.scrollWidth - 1);
  };
  let sum = 0, last = 0, done = false;
  addEventListener("wheel", (e) => {
    const now = e.timeStamp;
    if (now - last > 300) { sum = 0; done = false; }
    last = now;
    if (done || e.ctrlKey || e.defaultPrevented) return;
    if (Math.abs(e.deltaX) <= Math.abs(e.deltaY) * 2 || canScroll(e.target, e.deltaX)) { sum = 0; return; }
    sum += e.deltaX;
    if (Math.abs(sum) > 160) {
      done = true;
      sum < 0 ? history.back() : history.forward();
    }
  }, { passive: true });
})()"#;

#[cfg(test)]
mod tests {
    use super::{NAVIGATION_SCRIPT, download_refused};

    #[test]
    fn a_refused_download_is_named_in_the_status() {
        assert_eq!(
            download_refused("https://a.test/files/report.pdf?x=1#y"),
            "Download refused: report.pdf. Downloads are off for now."
        );
        assert_eq!(
            download_refused("https://a.test/"),
            "Download refused: a.test. Downloads are off for now."
        );
        assert!(!download_refused("http://a.test/x\u{1b}[2J").contains('\u{1b}'));
    }

    #[test]
    fn the_browsers_script_gives_a_page_nothing_of_dreamers() {
        // No bridge, no global the page could find, nothing called but the
        // page's own history.
        for s in [
            "dreamer.call",
            "/.dreamer",
            "fetch(",
            "window.dreamer",
            "postMessage",
        ] {
            assert!(!NAVIGATION_SCRIPT.contains(s), "{s}");
        }
    }
}
