//! `web.open`: a web page in a new tmux pane; `web.navigate`: another
//! address in one; and `web.state`: what the page shows, kept in that pane
//! (`docs/desktop.md`, "Web panes").
//!
//! The pane is a placeholder: it holds its place in tmux's layout and runs
//! a small program that prints the page's title and address, for plain
//! tmux clients. Its `@dreamer_surface` user option (`web:<URL>`) tells
//! desktops to draw the page over its rectangle instead, and
//! `@dreamer_web_scroll` keeps where the page was scrolled to. The page
//! itself runs in the desktop, never here: a desktop drawing it follows a
//! change of its URL (`web.navigate`) by loading the new address, and
//! reports where the page went by itself (a link) in `web.state`.

use std::sync::Arc;

use api::{RpcError, code};
use domain::{TmuxPaneId, TmuxSessionId};
use tmux_backend::ops::{self, Launch, Split};

use crate::log;
use crate::service::State;

fn refused(e: &tmux_backend::CommandError) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, ops::failure_message(e))
}

/// The most of a page's title the placeholder keeps.
const TITLE_MAX: usize = 200;

/// What the placeholder pane runs: on a cleared screen, the page's title
/// when it has one, the address (at most 200 characters of it), and how to
/// see the page, then nothing until it is closed.
fn placeholder(url: &str, title: &str) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '\\033[H\\033[2J'; [ -n \"$2\" ] && printf '%s\\n\\n' \"$2\"; \
         printf 'web: %.200s\\n\\nOpen in Dreamer to see the page.\\n' \"$1\"; \
         exec tail -f /dev/null"
            .into(),
        "dreamer-web".into(),
        url.into(),
        title.into(),
    ]
}

/// A page's title as the placeholder keeps it: no control characters, at
/// most [`TITLE_MAX`] characters.
fn kept_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control())
        .take(TITLE_MAX)
        .collect()
}

/// The URL a requested address means (see [`ops::web_address`]): a URL
/// a web pane may show, or an address as typed into a browser.
fn address(typed: &str) -> Result<String, RpcError> {
    ops::web_address(typed).map_err(|why| {
        RpcError::new(
            code::INVALID_PARAMS,
            format!("{why}: a web pane shows http, https, file, and data URLs, and about:blank"),
        )
    })
}

pub async fn open(state: &Arc<State>, p: api::WebOpen) -> Result<api::WebOpened, RpcError> {
    let url = address(&p.url)?;
    let mut guard = state.endpoint().await?;
    let ep = guard
        .as_mut()
        .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "endpoint not connected"))?;
    // Created in the background and marked before it is shown, so no
    // desktop draws it as a terminal first.
    let launch = Launch {
        argv: placeholder(&url, ""),
        background: true,
        ..Launch::default()
    };
    let pane = match (p.pane, p.session) {
        (Some(target), _) => {
            ops::split_pane(
                &ep.conn,
                TmuxPaneId(target),
                Split::Horizontal,
                None,
                &launch,
            )
            .await
        }
        (None, Some(session)) => {
            ops::new_window(&ep.conn, TmuxSessionId(session), Some("web"), &launch).await
        }
        (None, None) => {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "name a session (a new window) or a pane (to split) for the web page",
            ));
        }
    }
    .map_err(|e| refused(&e))?;
    let conn = &ep.conn;
    ops::set_pane_option(conn, pane, ops::SURFACE, &format!("web:{url}"))
        .await
        .map_err(|e| refused(&e))?;
    ep.tracker
        .refresh()
        .await
        .map_err(|e| RpcError::new(code::UNAVAILABLE, format!("cannot read tmux: {e}")))?;
    if let Some(window) = ep.tracker.inventory().panes.get(&pane).map(|p| p.window) {
        let _ = ops::select_window(conn, window).await;
    }
    ops::select_pane(conn, pane)
        .await
        .map_err(|e| refused(&e))?;
    log::info(format!("web: opened a page in pane %{}", pane.0));
    Ok(api::WebOpened { pane: pane.0 })
}

/// Keep `url` as web pane `pane`'s page, with `title`: its surface option,
/// and the placeholder's text.
async fn set_url(
    conn: &tmux_backend::connection::Connection,
    pane: TmuxPaneId,
    url: &str,
    title: &str,
) -> Result<(), RpcError> {
    ops::set_pane_option(conn, pane, ops::SURFACE, &format!("web:{url}"))
        .await
        .map_err(|e| refused(&e))?;
    ops::respawn_pane(conn, pane, &placeholder(url, title))
        .await
        .map_err(|e| refused(&e))
}

pub async fn navigate(state: &Arc<State>, p: api::WebNavigate) -> Result<api::Empty, RpcError> {
    let url = address(&p.url)?;
    let mut guard = state.endpoint().await?;
    let ep = guard
        .as_mut()
        .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "endpoint not connected"))?;
    let pane = TmuxPaneId(p.pane);
    let Some(info) = ep.tracker.inventory().panes.get(&pane).cloned() else {
        return Err(RpcError::new(
            code::NOT_FOUND,
            format!("no pane %{}", p.pane),
        ));
    };
    if info.surface.as_deref().and_then(ops::web_surface).is_none() {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("pane %{} is not a web pane", p.pane),
        ));
    }
    // The old page's scroll position is not the new one's.
    set_url(&ep.conn, pane, &url, "").await?;
    ops::set_pane_option(&ep.conn, pane, ops::WEB_SCROLL, "0 0")
        .await
        .map_err(|e| refused(&e))?;
    ep.tracker
        .refresh()
        .await
        .map_err(|e| RpcError::new(code::UNAVAILABLE, format!("cannot read tmux: {e}")))?;
    log::info(format!("web: pane %{} loads another address", pane.0));
    Ok(api::Empty {})
}

pub async fn state(state: &Arc<State>, p: api::WebState) -> Result<api::Empty, RpcError> {
    let mut guard = state.endpoint().await?;
    let ep = guard
        .as_mut()
        .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "endpoint not connected"))?;
    let pane = TmuxPaneId(p.pane);
    let Some(info) = ep.tracker.inventory().panes.get(&pane).cloned() else {
        return Err(RpcError::new(
            code::NOT_FOUND,
            format!("no pane %{}", p.pane),
        ));
    };
    let Some(url) = info.surface.as_deref().and_then(ops::web_surface) else {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("pane %{} is not a web pane", p.pane),
        ));
    };
    let conn = &ep.conn;
    let title = kept_title(&p.title);
    // Where the page went by itself (a link, back), if a web pane may
    // show it there.
    let moved = p
        .url
        .as_deref()
        .filter(|u| *u != url && ops::web_url_allowed(u));
    if let Some(to) = moved {
        set_url(conn, pane, to, &title).await?;
        ops::set_pane_title(conn, pane, &title)
            .await
            .map_err(|e| refused(&e))?;
    } else if title != info.title {
        // The placeholder prints the title for plain tmux clients.
        ops::respawn_pane(conn, pane, &placeholder(url, &title))
            .await
            .map_err(|e| refused(&e))?;
        ops::set_pane_title(conn, pane, &title)
            .await
            .map_err(|e| refused(&e))?;
    }
    let scroll = (p.scroll_x, p.scroll_y);
    if info.web_scroll != Some(scroll) {
        let value = format!("{} {}", scroll.0, scroll.1);
        ops::set_pane_option(conn, pane, ops::WEB_SCROLL, &value)
            .await
            .map_err(|e| refused(&e))?;
    }
    ep.tracker
        .refresh()
        .await
        .map_err(|e| RpcError::new(code::UNAVAILABLE, format!("cannot read tmux: {e}")))?;
    Ok(api::Empty {})
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_placeholder_passes_the_url_and_title_as_arguments() {
        let argv = super::placeholder("https://a.test/?q='x'; rm -rf /", "$(reboot)");
        assert_eq!(argv[4], "https://a.test/?q='x'; rm -rf /");
        assert_eq!(argv[5], "$(reboot)");
        assert!(!argv[2].contains("a.test") && !argv[2].contains("reboot"));
    }

    #[test]
    fn a_kept_title_has_no_control_characters_and_a_bound() {
        assert_eq!(super::kept_title("a\x1b]0;b\x07c\n"), "a]0;bc");
        assert_eq!(super::kept_title(&"é".repeat(500)).chars().count(), 200);
    }
}
