//! tmux control-mode backend.
//!
//! At this stage (M0) the crate holds the feasibility prototype for cold
//! attach: an isolated test server, a blocking control client, and the pane
//! bootstrap procedure documented in `docs/tmux-bootstrap.md`. The production
//! connection, scheduler, and inventory arrive in M1.

pub mod bootstrap;
pub mod command;
pub mod connection;
pub mod control;
pub mod inventory;
pub mod layout;
pub mod server;
pub mod workspace;

pub use bootstrap::{BootstrapError, Bootstrapped, PaneState, bootstrap_pane};
pub use command::{Batch, TmuxCommand};
pub use connection::{CommandError, ConnEvent, Connection, ConnectionConfig, LinkState, Socket};
pub use control::{ControlClient, ControlError};
pub use inventory::{Inventory, InventoryError, InventoryTracker, affects_structure};
pub use server::TmuxServer;

/// Quote a value for tmux's command parser (single quotes, with embedded
/// single quotes closed, escaped, and reopened).
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn quote_wraps_and_escapes() {
        assert_eq!(super::quote("%1:pause"), "'%1:pause'");
        assert_eq!(super::quote("it's"), "'it'\\''s'");
    }
}
