//! `web.open`: a web page in a new tmux pane (`docs/desktop.md`, "Web
//! panes").
//!
//! The pane is a placeholder: it holds its place in tmux's layout and runs
//! a small program that prints the page's address, for plain tmux clients.
//! Its `@dreamer_surface` user option (`web:<URL>`) tells desktops to draw
//! the page over its rectangle instead. The page itself runs in the
//! desktop, never here.

use std::sync::Arc;

use api::{RpcError, code};
use domain::{TmuxPaneId, TmuxSessionId};
use tmux_backend::ops::{self, Launch, Split};

use crate::log;
use crate::service::State;

fn refused(e: &tmux_backend::CommandError) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, ops::failure_message(e))
}

/// What the placeholder pane runs: the address (at most 200 characters of
/// it), a line saying where the page shows, and then nothing until it is
/// closed.
fn placeholder(url: &str) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf 'web: %.200s\\n\\nThis pane shows the page in Dreamer.\\n' \"$1\"; \
         exec tail -f /dev/null"
            .into(),
        "dreamer-web".into(),
        url.into(),
    ]
}

pub async fn open(state: &Arc<State>, p: api::WebOpen) -> Result<api::WebOpened, RpcError> {
    if !ops::web_url_allowed(&p.url) {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            "a web pane shows http, https, file, and data URLs, and about:blank",
        ));
    }
    let mut guard = state.endpoint().await?;
    let ep = guard
        .as_mut()
        .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "endpoint not connected"))?;
    // Created in the background and marked before it is shown, so no
    // desktop draws it as a terminal first.
    let launch = Launch {
        argv: placeholder(&p.url),
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
    ops::set_pane_option(conn, pane, ops::SURFACE, &format!("web:{}", p.url))
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

#[cfg(test)]
mod tests {
    #[test]
    fn the_placeholder_passes_the_url_as_an_argument() {
        let argv = super::placeholder("https://a.test/?q='x'; rm -rf /");
        assert_eq!(
            argv.last().map(String::as_str),
            Some("https://a.test/?q='x'; rm -rf /")
        );
        assert!(!argv[2].contains("a.test"));
    }
}
