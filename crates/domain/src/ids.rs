//! Typed identities.
//!
//! tmux object IDs (`$1`, `@2`, `%3`) are only unique within one tmux server
//! incarnation. Every Dreamer identity therefore carries the server identity
//! and the incarnation; a pane `%1` on another server, or on the same server
//! after a restart, is a different object. Display indexes and names are
//! never identities.

use std::fmt;

/// Stable identity of a host's application server (`dreamerd`), chosen when
/// the daemon's state directory is first created.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerKey(pub String);

/// One tmux server's lifetime, distinguishing a restarted server on the same
/// socket. Derived from evidence such as the server's PID and start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerIncarnation(pub u64);

/// One control connection to a tmux server. Results tagged with an older
/// epoch are stale and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ConnectionEpoch(pub u64);

impl ConnectionEpoch {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// tmux's own numeric IDs, without the sigil.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TmuxSessionId(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TmuxWindowId(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TmuxPaneId(pub u32);

impl fmt::Display for TmuxSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}", self.0)
    }
}
impl fmt::Display for TmuxWindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}
impl fmt::Display for TmuxPaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

/// The tmux server a tmux object belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TmuxServerRef {
    pub server: ServerKey,
    /// The tmux endpoint (socket) on that host, as configured.
    pub endpoint: String,
    pub incarnation: ServerIncarnation,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionKey {
    pub tmux: TmuxServerRef,
    pub session: TmuxSessionId,
}

/// A window has one identity even when linked into several sessions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowKey {
    pub tmux: TmuxServerRef,
    pub window: TmuxWindowId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneKey {
    pub tmux: TmuxServerRef,
    pub pane: TmuxPaneId,
}

/// A pane's process lifetime. Respawning a pane keeps its tmux ID but starts
/// a new generation: mirrors, input leases, and worker results from an older
/// generation are invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PaneGeneration(pub u64);

/// A specific execution of a pane: identity plus process generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneInstance {
    pub pane: PaneKey,
    pub generation: PaneGeneration,
}

/// A host process with enough evidence to avoid PID reuse confusion
/// (`ProcessRef`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcessRef {
    pub server: ServerKey,
    /// PID namespace where the OS has one (Linux), otherwise `None`.
    pub pid_namespace: Option<u64>,
    pub pid: u32,
    /// Process start time or another birth identity from the OS.
    pub birth: u64,
}

macro_rules! opaque_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u128);
    };
}

opaque_id!(
    /// Identifies one submitted action across its whole lifecycle.
    ActionId
);
opaque_id!(
    /// Correlates a request with its response across components.
    RequestId
);
opaque_id!(
    /// A client (desktop, CLI, extension) connected to a daemon.
    ClientId
);
