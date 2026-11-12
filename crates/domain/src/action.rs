//! Typed actions, their lifecycle, and backend events (spec 6, 12.4).
//!
//! UI code and agents submit these; they never assemble tmux command
//! strings. Distinct intents stay distinct: sending input to a foreground
//! application is not the same action as launching a program.

use crate::ids::{
    ActionId, PaneInstance, PaneKey, SessionKey, TmuxPaneId, TmuxWindowId, WindowKey,
};
use crate::layout::{SplitDirection, WindowLayout};
use crate::state::{ConnectionState, PaneSyncState};

/// Size for a new or resized pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneSize {
    Cells(u16),
    Percent(u8),
}

/// A program to start, as an exact argument vector. Never a shell string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Program {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Bytes for a pane, tagged with who produced them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InputSource {
    /// A person at a client.
    Human,
    /// Automation holding an interaction lease of this generation.
    Automation { lease: u64, generation: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    NewSession {
        name: Option<String>,
        program: Option<Program>,
    },
    NewWindow {
        session: SessionKey,
        name: Option<String>,
        program: Option<Program>,
    },
    SplitPane {
        target: PaneKey,
        direction: SplitDirection,
        size: Option<PaneSize>,
        program: Option<Program>,
    },
    ResizePane {
        target: PaneKey,
        cols: Option<u16>,
        rows: Option<u16>,
    },
    SelectWindow {
        session: SessionKey,
        window: WindowKey,
    },
    SelectPane {
        target: PaneKey,
    },
    ZoomPane {
        target: PaneKey,
    },
    RenameSession {
        session: SessionKey,
        name: String,
    },
    RenameWindow {
        window: WindowKey,
        name: String,
    },
    /// Literal input to whatever runs in the pane now. Bound to a pane
    /// generation so input cannot reach a respawned process by accident.
    SendInput {
        target: PaneInstance,
        bytes: Vec<u8>,
        source: InputSource,
    },
    /// Close this client's view; the session and its processes keep running.
    Detach {
        session: SessionKey,
    },
    KillPane {
        target: PaneKey,
    },
    KillWindow {
        window: WindowKey,
    },
    KillSession {
        session: SessionKey,
    },
}

impl Action {
    /// Destructive actions are never retried automatically after an
    /// ambiguous outcome (spec 7.3).
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Action::KillPane { .. } | Action::KillWindow { .. } | Action::KillSession { .. }
        )
    }

    /// Actions whose repetition would duplicate an effect (new objects,
    /// typed input) are not replayed after an uncertain disconnect either.
    pub fn replay_safe(&self) -> bool {
        matches!(
            self,
            Action::SelectWindow { .. }
                | Action::SelectPane { .. }
                | Action::RenameSession { .. }
                | Action::RenameWindow { .. }
                | Action::ResizePane { .. }
        )
    }
}

/// Lifecycle of an action (spec 12.4). `OutcomeUnknown` is terminal until
/// reconciliation decides; it never turns into a retry by itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionState {
    Accepted,
    Prepared,
    Dispatching,
    Completed,
    Failed { reason: String },
    Cancelled,
    OutcomeUnknown { reason: String },
}

impl ActionState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ActionState::Completed
                | ActionState::Failed { .. }
                | ActionState::Cancelled
                | ActionState::OutcomeUnknown { .. }
        )
    }

    /// Legal forward transitions. Anything else is a bug in the ledger.
    pub fn can_become(&self, next: &ActionState) -> bool {
        use ActionState::*;
        matches!(
            (self, next),
            (Accepted, Prepared | Cancelled | Failed { .. })
                | (Prepared, Dispatching | Cancelled | Failed { .. })
                | (
                    Dispatching,
                    Completed | Failed { .. } | OutcomeUnknown { .. }
                )
                | (OutcomeUnknown { .. }, Completed | Failed { .. })
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRecord {
    pub id: ActionId,
    pub action: Action,
    pub state: ActionState,
}

/// Structured backend observations. Terminal output travels separately on
/// the ordered byte path and never through this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    Connection(ConnectionState),
    SessionAdded {
        session: SessionKey,
        name: String,
    },
    SessionRenamed {
        session: SessionKey,
        name: String,
    },
    SessionClosed {
        session: SessionKey,
    },
    WindowAdded {
        window: WindowKey,
        sessions: Vec<SessionKey>,
    },
    WindowRenamed {
        window: WindowKey,
        name: String,
    },
    WindowClosed {
        window: WindowKey,
    },
    WindowLayout {
        window: WindowKey,
        layout: WindowLayout,
    },
    ActiveWindow {
        session: SessionKey,
        window: TmuxWindowId,
    },
    ActivePane {
        window: WindowKey,
        pane: TmuxPaneId,
    },
    PaneSync {
        pane: PaneKey,
        state: PaneSyncState,
    },
    PaneRespawned {
        pane: PaneInstance,
    },
    PaneExited {
        pane: PaneInstance,
        status: Option<i32>,
    },
    Action(ActionRecord),
}
