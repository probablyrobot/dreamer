//! UI-independent host logic (spec 5.1): typed actions and their ledger,
//! and the inventory as API records. `dreamerd` drives these; nothing here
//! depends on a GUI.

pub mod actions;
pub mod ledger;
pub mod topology;

pub use actions::{Actions, Endpoint, Hooks};
pub use ledger::{Ledger, LedgerError};
pub use topology::topology;
