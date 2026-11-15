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
    VerifyHost,
    InstallOnHost,
    DisconnectHost,
    AddHost,
    EditHost,
    RemoveHost,
    ToggleDecorations,
    OpenPanel,
    ClosePanel,
}

pub(crate) const ALL: [Command; 23] = [
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
    Command::VerifyHost,
    Command::InstallOnHost,
    Command::DisconnectHost,
    Command::AddHost,
    Command::EditHost,
    Command::RemoveHost,
    Command::ToggleDecorations,
    Command::OpenPanel,
    Command::ClosePanel,
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
            Command::AddHost => "Add a host profile…",
            Command::EditHost => "Edit a host profile…",
            Command::RemoveHost => "Remove a host profile… (the host is not touched)",
            Command::InstallOnHost => "Install or update the server on a host…",
            Command::VerifyHost => "Verify host in a terminal (host key, passphrase)…",
            Command::DisconnectHost => "Disconnect host… (its programs keep running)",
            Command::ToggleDecorations => "Show or hide decorations in this pane (filter marks)",
            Command::OpenPanel => "Open an app package as a panel… (its manifest's path)",
            Command::ClosePanel => "Close the app panel",
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
                | Command::VerifyHost
                | Command::InstallOnHost
                | Command::DisconnectHost
                | Command::EditHost
                | Command::RemoveHost
                | Command::OpenPanel
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
    /// A command waits for several values, one field at a time.
    Form(Form),
}

/// Fields typed one after another: Enter moves to the next, and the last
/// Enter submits. Up and Down move between fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Form {
    pub command: Command,
    /// (label, value, required)
    pub fields: Vec<(&'static str, String, bool)>,
    pub at: usize,
    /// Why the last submit was refused, shown until the next edit.
    pub error: Option<String>,
    /// For an edit: the profile name being replaced.
    pub replacing: Option<String>,
}

/// The fields of a host profile form, in order.
pub(crate) const HOST_FIELDS: [(&str, bool); 5] = [
    ("Name", true),
    ("SSH destination (host alias or user@host)", true),
    ("Remote dream path (blank: installed, else PATH)", false),
    ("Remote tmux socket name (blank: default)", false),
    ("Tags, comma separated", false),
];

impl Form {
    pub(crate) fn host(command: Command, values: [String; 5], replacing: Option<String>) -> Form {
        Form {
            command,
            fields: HOST_FIELDS
                .iter()
                .zip(values)
                .map(|((label, required), v)| (*label, v, *required))
                .collect(),
            at: 0,
            error: None,
            replacing,
        }
    }
}

/// What Enter decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Run this command (with a name, for those that need one).
    Run(Command, Option<String>),
    /// Submit a form's values.
    Submit(Form),
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
            Stage::Form(f) => {
                f.fields[f.at].1.push(c);
                f.error = None;
            }
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
            Stage::Form(f) => {
                f.fields[f.at].1.pop();
                f.error = None;
            }
        }
    }

    pub(crate) fn move_selection(&mut self, down: bool) {
        if let Stage::Form(f) = &mut self.stage {
            let n = f.fields.len();
            f.at = if down {
                (f.at + 1) % n
            } else {
                (f.at + n - 1) % n
            };
            return;
        }
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
                } else if c == Command::AddHost {
                    self.stage = Stage::Form(Form::host(c, Default::default(), None));
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
            Stage::Form(mut f) => {
                let (label, value, required) = &f.fields[f.at];
                if *required && value.trim().is_empty() {
                    f.error = Some(format!("{label} is required"));
                } else if f.at + 1 < f.fields.len() {
                    f.at += 1;
                } else if let Some(i) = f
                    .fields
                    .iter()
                    .position(|(_, v, required)| *required && v.trim().is_empty())
                {
                    f.at = i;
                    f.error = Some(format!("{} is required", f.fields[i].0));
                } else {
                    return Decision::Submit(f);
                }
                self.stage = Stage::Form(f);
                Decision::Stay
            }
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
    fn a_host_form_takes_fields_in_turn_and_insists_on_required_ones() {
        let mut p = Palette::new();
        "add a host".chars().for_each(|c| p.type_char(c));
        assert_eq!(p.shown()[0], Command::AddHost);
        assert_eq!(p.enter(), Decision::Stay);
        // Name is required.
        assert_eq!(p.enter(), Decision::Stay);
        let Stage::Form(f) = &p.stage else {
            panic!("form")
        };
        assert_eq!(f.at, 0);
        assert!(f.error.is_some());
        "devbox".chars().for_each(|c| p.type_char(c));
        assert_eq!(p.enter(), Decision::Stay);
        "me@devbox".chars().for_each(|c| p.type_char(c));
        for _ in 0..3 {
            assert_eq!(p.enter(), Decision::Stay);
        }
        let Decision::Submit(f) = p.enter() else {
            panic!("submit")
        };
        assert_eq!(f.fields[0].1, "devbox");
        assert_eq!(f.fields[1].1, "me@devbox");
        assert!(f.fields[2..].iter().all(|(_, v, _)| v.is_empty()));
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
