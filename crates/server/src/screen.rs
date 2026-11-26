//! Screen frames on the wire. Encoding runs on the connection's task,
//! never on the pane's owner.

pub use client::frames::{Head, encode};
