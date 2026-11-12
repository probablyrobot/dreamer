//! Connection and pane synchronization states (spec 6), and the capability
//! profile that records what each layer supports, with evidence.

/// Lifecycle of one control connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Negotiating,
    Synchronizing,
    Live,
    Recovering,
    Closing,
    Failed,
}

/// Whether a pane's mirror can be trusted and shown live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneSyncState {
    Discovered,
    Capturing,
    Restoring,
    Live,
    Paused,
    /// Continuity was lost; the view is old and marked as such.
    Stale,
    /// This pane cannot be mirrored independently; use the attach fallback.
    Unsupported,
    Exited,
}

impl PaneSyncState {
    /// Only a live mirror may be presented as current.
    pub fn is_live(self) -> bool {
        self == PaneSyncState::Live
    }
}

/// One capability decision and the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub supported: bool,
    /// Version or probe result that justifies the decision.
    pub evidence: String,
}

/// What each layer supports for one connection (spec 6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilityProfile {
    pub engine: Vec<Capability>,
    pub renderer: Vec<Capability>,
    pub tmux: Vec<Capability>,
    pub transport: Vec<Capability>,
    pub bootstrap: Vec<Capability>,
    pub platform: Vec<Capability>,
}

impl CapabilityProfile {
    /// A capability is usable only if every layer that lists it supports it.
    pub fn supports(&self, name: &str) -> bool {
        let layers = [
            &self.engine,
            &self.renderer,
            &self.tmux,
            &self.transport,
            &self.bootstrap,
            &self.platform,
        ];
        let mut listed = false;
        for layer in layers {
            for c in layer.iter().filter(|c| c.name == name) {
                listed = true;
                if !c.supported {
                    return false;
                }
            }
        }
        listed
    }
}
