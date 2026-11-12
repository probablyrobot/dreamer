//! Shared domain model: identities, layouts, states, capabilities, actions,
//! and events. Dependency-free, so every crate (headless and GUI) can use
//! it; wire encoding lives in `wire`.

pub mod action;
pub mod ids;
pub mod layout;
pub mod state;

pub use action::{Action, ActionRecord, ActionState, BackendEvent, InputSource, PaneSize, Program};
pub use ids::{
    ActionId, ClientId, ConnectionEpoch, PaneGeneration, PaneInstance, PaneKey, ProcessRef,
    RequestId, ServerIncarnation, ServerKey, SessionKey, TmuxPaneId, TmuxServerRef, TmuxSessionId,
    TmuxWindowId, WindowKey,
};
pub use layout::{CellRect, LayoutNode, SplitDirection, WindowLayout};
pub use state::{Capability, CapabilityProfile, ConnectionState, PaneSyncState};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn server(name: &str, incarnation: u64) -> TmuxServerRef {
        TmuxServerRef {
            server: ServerKey(name.into()),
            endpoint: "default".into(),
            incarnation: ServerIncarnation(incarnation),
        }
    }

    fn pane(server_ref: &TmuxServerRef, id: u32) -> PaneKey {
        PaneKey {
            tmux: server_ref.clone(),
            pane: TmuxPaneId(id),
        }
    }

    #[test]
    fn the_same_tmux_id_on_another_server_is_another_pane() {
        let a = server("laptop", 1);
        let b = server("buildbox", 1);
        assert_ne!(pane(&a, 1), pane(&b, 1));
    }

    #[test]
    fn the_same_tmux_id_after_a_server_restart_is_another_pane() {
        let before = server("laptop", 1);
        let after = server("laptop", 2);
        let mut views: HashMap<PaneKey, &str> = HashMap::new();
        views.insert(pane(&before, 1), "old shell");
        // A restarted server reuses %1; looking it up must not find the old view.
        assert!(!views.contains_key(&pane(&after, 1)));
    }

    #[test]
    fn a_respawned_pane_is_a_new_instance_of_the_same_pane() {
        let s = server("laptop", 1);
        let first = PaneInstance {
            pane: pane(&s, 3),
            generation: PaneGeneration(1),
        };
        let second = PaneInstance {
            pane: pane(&s, 3),
            generation: PaneGeneration(2),
        };
        assert_eq!(first.pane, second.pane);
        assert_ne!(first, second);
    }

    #[test]
    fn a_linked_window_has_one_identity_across_sessions() {
        let s = server("laptop", 1);
        let window = WindowKey {
            tmux: s.clone(),
            window: TmuxWindowId(7),
        };
        let in_work = SessionKey {
            tmux: s.clone(),
            session: TmuxSessionId(1),
        };
        let in_play = SessionKey {
            tmux: s,
            session: TmuxSessionId(2),
        };
        let event = BackendEvent::WindowAdded {
            window: window.clone(),
            sessions: vec![in_work, in_play],
        };
        let BackendEvent::WindowAdded {
            window: w,
            sessions,
        } = event
        else {
            unreachable!()
        };
        assert_eq!(w, window);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn stale_epochs_compare_older() {
        let e1 = ConnectionEpoch::default().next();
        let e2 = e1.next();
        assert!(e1 < e2);
    }

    #[test]
    fn layout_validation_catches_overlap_and_duplicates() {
        let rect = |x, y, cols, rows| CellRect { x, y, cols, rows };
        let good = LayoutNode::Split {
            rect: rect(0, 0, 81, 24),
            direction: SplitDirection::Horizontal,
            children: vec![
                LayoutNode::Pane {
                    rect: rect(0, 0, 40, 24),
                    pane: TmuxPaneId(0),
                },
                LayoutNode::Pane {
                    rect: rect(41, 0, 40, 24),
                    pane: TmuxPaneId(1),
                },
            ],
        };
        assert_eq!(good.validate(), Ok(()));
        assert_eq!(good.panes().len(), 2);

        let overlap = LayoutNode::Split {
            rect: rect(0, 0, 80, 24),
            direction: SplitDirection::Horizontal,
            children: vec![
                LayoutNode::Pane {
                    rect: rect(0, 0, 41, 24),
                    pane: TmuxPaneId(0),
                },
                LayoutNode::Pane {
                    rect: rect(40, 0, 40, 24),
                    pane: TmuxPaneId(1),
                },
            ],
        };
        assert!(overlap.validate().is_err());

        let duplicate = LayoutNode::Split {
            rect: rect(0, 0, 80, 24),
            direction: SplitDirection::Vertical,
            children: vec![
                LayoutNode::Pane {
                    rect: rect(0, 0, 80, 12),
                    pane: TmuxPaneId(0),
                },
                LayoutNode::Pane {
                    rect: rect(0, 13, 80, 11),
                    pane: TmuxPaneId(0),
                },
            ],
        };
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn destructive_and_replay_rules() {
        let s = server("laptop", 1);
        let kill = Action::KillPane {
            target: pane(&s, 1),
        };
        assert!(kill.is_destructive());
        assert!(!kill.replay_safe());
        let input = Action::SendInput {
            target: PaneInstance {
                pane: pane(&s, 1),
                generation: PaneGeneration(1),
            },
            bytes: b"ls\r".to_vec(),
            source: InputSource::Human,
        };
        assert!(!input.is_destructive());
        assert!(!input.replay_safe(), "typed input is never replayed");
        assert!(
            Action::SelectPane {
                target: pane(&s, 1)
            }
            .replay_safe()
        );
    }

    #[test]
    fn action_lifecycle_transitions() {
        use ActionState::*;
        assert!(Accepted.can_become(&Prepared));
        assert!(Dispatching.can_become(&OutcomeUnknown {
            reason: "disconnect".into()
        }));
        assert!(
            OutcomeUnknown {
                reason: String::new()
            }
            .can_become(&Completed)
        );
        assert!(!Completed.can_become(&Dispatching));
        assert!(
            !OutcomeUnknown {
                reason: String::new()
            }
            .can_become(&Dispatching)
        );
        assert!(Completed.is_terminal());
        assert!(!Dispatching.is_terminal());
    }

    #[test]
    fn capability_needs_every_listing_layer() {
        let cap = |name: &str, supported| Capability {
            name: name.into(),
            supported,
            evidence: "test".into(),
        };
        let profile = CapabilityProfile {
            engine: vec![cap("kitty-graphics", true)],
            tmux: vec![cap("kitty-graphics", false)],
            ..CapabilityProfile::default()
        };
        assert!(!profile.supports("kitty-graphics"));
        assert!(!profile.supports("never-listed"));
        let ok = CapabilityProfile {
            engine: vec![cap("sgr-mouse", true)],
            ..CapabilityProfile::default()
        };
        assert!(ok.supports("sgr-mouse"));
    }
}
