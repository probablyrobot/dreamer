//! The inventory as API records (`tmux.topology`).

use domain::WindowLayout;
use tmux_backend::InventoryTracker;

/// Sessions, windows, and panes of one endpoint, in id order.
pub fn topology(endpoint: &str, tracker: &InventoryTracker) -> api::Topology {
    let inv = tracker.inventory();
    let sessions = inv
        .sessions
        .iter()
        .map(|(id, s)| api::Session {
            id: id.0,
            name: s.name.clone(),
            windows: s.windows.values().map(|w| w.0).collect(),
            active_window: s.active_window.map(|w| w.0),
        })
        .collect();
    let windows = inv
        .windows
        .iter()
        .map(|(id, w)| {
            let index = inv
                .sessions
                .values()
                .find_map(|s| {
                    s.windows
                        .iter()
                        .find(|(_, wid)| *wid == id)
                        .map(|(i, _)| *i)
                })
                .unwrap_or_default();
            let (supported, width, height, panes) = match &w.layout {
                WindowLayout::Tiled(node) => {
                    let r = node.rect();
                    let panes = node.panes().iter().map(|(p, _)| p.0).collect();
                    (true, u32::from(r.cols), u32::from(r.rows), panes)
                }
                WindowLayout::Unsupported { .. } => {
                    let panes = inv
                        .panes
                        .iter()
                        .filter(|(_, p)| p.window == *id)
                        .map(|(pid, _)| pid.0)
                        .collect();
                    (false, 0, 0, panes)
                }
            };
            api::Window {
                id: id.0,
                name: w.name.clone(),
                sessions: w.sessions.iter().map(|s| s.0).collect(),
                index,
                panes,
                active_pane: w.active_pane.map(|p| p.0),
                zoomed: w.zoomed,
                layout_supported: supported,
                width,
                height,
            }
        })
        .collect();
    let panes = inv
        .panes
        .iter()
        .map(|(id, p)| {
            let rect = inv.windows.get(&p.window).and_then(|w| match &w.layout {
                WindowLayout::Tiled(node) => node
                    .panes()
                    .into_iter()
                    .find(|(pid, _)| pid == id)
                    .map(|(_, r)| r),
                WindowLayout::Unsupported { .. } => None,
            });
            api::Pane {
                id: id.0,
                window: p.window.0,
                width: u32::from(p.cols),
                height: u32::from(p.rows),
                left: rect.map_or(0, |r| u32::from(r.x)),
                top: rect.map_or(0, |r| u32::from(r.y)),
                active: inv.windows.get(&p.window).and_then(|w| w.active_pane) == Some(*id),
                pid: (p.pid > 0).then_some(p.pid),
                current_command: p.command.clone(),
                title: p.title.clone(),
                dead: p.dead,
                mode: p.mode.clone(),
                generation: tracker.generation(*id).map_or(0, |g| g.0),
            }
        })
        .collect();
    api::Topology {
        endpoint: endpoint.to_owned(),
        incarnation: inv.server.incarnation.0,
        sessions,
        windows,
        panes,
    }
}
