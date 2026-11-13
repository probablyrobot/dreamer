//! The command palette (spec 4.2, 14.1): every pane and window action by
//! keyboard. Destructive actions are named as such and need a second
//! Enter; "Close view" closes only this window and never kills anything.

/// A workspace command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    SplitRight,
    SplitDown,
    NewWindow,
    RenameWindow,
    NextWindow,
    PreviousWindow,
    NextPane,
    Zoom,
    CloseView,
    KillPane,
    KillWindow,
    ToggleSidebar,
    OpenHost,
    ReconnectHost,
    DisconnectHost,
}

pub(crate) const ALL: [Command; 15] = [
    Command::SplitRight,
    Command::SplitDown,
    Command::NewWindow,
    Command::RenameWindow,
    Command::NextWindow,
    Command::PreviousWindow,
    Command::NextPane,
    Command::Zoom,
    Command::CloseView,
    Command::KillPane,
    Command::KillWindow,
    Command::ToggleSidebar,
    Command::OpenHost,
    Command::ReconnectHost,
    Command::DisconnectHost,
];

impl Command {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Command::SplitRight => "Split pane right",
            Command::SplitDown => "Split pane down",
            Command::NewWindow => "New window",
            Command::RenameWindow => "Rename window…",
            Command::NextWindow => "Next window",
            Command::PreviousWindow => "Previous window",
            Command::NextPane => "Focus next pane",
            Command::Zoom => "Zoom or unzoom pane",
            Command::CloseView => "Close view (programs keep running)",
            Command::KillPane => "Kill pane (ends its program)…",
            Command::KillWindow => "Kill window (ends its programs)…",
            Command::ToggleSidebar => "Toggle sidebar (sessions and hosts)",
            Command::OpenHost => "Open host…",
            Command::ReconnectHost => "Reconnect host…",
            Command::DisconnectHost => "Disconnect host… (its programs keep running)",
        }
    }

    /// Ends programs: asks for a second Enter.
    pub(crate) fn destructive(self) -> bool {
        matches!(self, Command::KillPane | Command::KillWindow)
    }

    /// Needs a name typed before it runs.
    pub(crate) fn needs_name(self) -> bool {
        matches!(
            self,
            Command::RenameWindow
                | Command::OpenHost
                | Command::ReconnectHost
                | Command::DisconnectHost
        )
    }
}

/// Every character of `filter`, in order, somewhere in `label`
/// (case-insensitive): "spr" finds "Split pane right".
pub(crate) fn matches(filter: &str, label: &str) -> bool {
    let mut label = label.chars().flat_map(char::to_lowercase);
    filter
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .all(|c| label.any(|l| l == c))
}

/// Where the palette is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Picking a command by typing a filter.
    Choose,
    /// A destructive command waits for a second Enter.
    Confirm(Command),
    /// A command waits for a name.
    Name(Command, String),
}

/// What Enter decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Run this command (with a name, for those that need one).
    Run(Command, Option<String>),
    /// Stay open: confirm or name next, or nothing matches.
    Stay,
}

#[derive(Debug, Clone)]
pub(crate) struct Palette {
    pub filter: String,
    pub selected: usize,
    pub stage: Stage,
}

impl Palette {
    pub(crate) fn new() -> Palette {
        Palette {
            filter: String::new(),
            selected: 0,
            stage: Stage::Choose,
        }
    }

    /// The commands the filter leaves, in order.
    pub(crate) fn shown(&self) -> Vec<Command> {
        ALL.into_iter()
            .filter(|c| matches(&self.filter, c.label()))
            .collect()
    }

    pub(crate) fn type_char(&mut self, c: char) {
        match &mut self.stage {
            Stage::Choose => {
                self.filter.push(c);
                self.selected = 0;
            }
            Stage::Name(_, name) => name.push(c),
            Stage::Confirm(_) => {}
        }
    }

    pub(crate) fn backspace(&mut self) {
        match &mut self.stage {
            Stage::Choose => {
                self.filter.pop();
                self.selected = 0;
            }
            Stage::Name(_, name) => {
                name.pop();
            }
            Stage::Confirm(_) => self.stage = Stage::Choose,
        }
    }

    pub(crate) fn move_selection(&mut self, down: bool) {
        let n = self.shown().len();
        if n == 0 {
            return;
        }
        self.selected = if down {
            (self.selected + 1) % n
        } else {
            (self.selected + n - 1) % n
        };
    }

    pub(crate) fn enter(&mut self) -> Decision {
        match self.stage.clone() {
            Stage::Choose => {
                let Some(c) = self.shown().get(self.selected).copied() else {
                    return Decision::Stay;
                };
                if c.destructive() {
                    self.stage = Stage::Confirm(c);
                    Decision::Stay
                } else if c.needs_name() {
                    self.stage = Stage::Name(c, String::new());
                    Decision::Stay
                } else {
                    Decision::Run(c, None)
                }
            }
            Stage::Confirm(c) => Decision::Run(c, None),
            Stage::Name(c, name) if !name.trim().is_empty() => {
                Decision::Run(c, Some(name.trim().to_owned()))
            }
            Stage::Name(..) => Decision::Stay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_finds_commands_by_letters_in_order() {
        assert!(matches("spr", "Split pane right"));
        assert!(matches("SPLIT DOWN", "Split pane down"));
        assert!(!matches("xyz", "Split pane right"));
        let mut p = Palette::new();
        for c in "kill".chars() {
            p.type_char(c);
        }
        assert_eq!(p.shown(), vec![Command::KillPane, Command::KillWindow]);
    }

    #[test]
    fn kills_need_a_second_enter_and_close_view_kills_nothing() {
        let mut p = Palette::new();
        for c in "kill pane".chars() {
            p.type_char(c);
        }
        assert_eq!(p.enter(), Decision::Stay, "the first Enter only asks");
        assert_eq!(p.stage, Stage::Confirm(Command::KillPane));
        p.backspace();
        assert_eq!(p.stage, Stage::Choose, "backing out cancels the kill");
        assert_eq!(p.enter(), Decision::Stay);
        assert_eq!(p.enter(), Decision::Run(Command::KillPane, None));

        let mut p = Palette::new();
        for c in "close".chars() {
            p.type_char(c);
        }
        assert_eq!(p.enter(), Decision::Run(Command::CloseView, None));
        assert!(!Command::CloseView.destructive());
        assert!(
            ALL.iter()
                .filter(|c| c.destructive())
                .all(|c| c.label().contains("Kill"))
        );
    }

    #[test]
    fn rename_asks_for_a_name() {
        let mut p = Palette::new();
        for c in "rename".chars() {
            p.type_char(c);
        }
        assert_eq!(p.enter(), Decision::Stay);
        assert_eq!(p.enter(), Decision::Stay, "an empty name runs nothing");
        for c in "logs".chars() {
            p.type_char(c);
        }
        assert_eq!(
            p.enter(),
            Decision::Run(Command::RenameWindow, Some("logs".into()))
        );
    }
}
