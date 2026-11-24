//! The command palette: every pane and window action by
//! keyboard. Destructive actions are named as such and need a second
//! Enter; "Close view" closes only this window and never kills anything.

use terminal_ghostty::core::schemes::Scheme;

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
    ToggleDrawer,
    NewDrawer,
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
    OpenWebPage,
    /// Add Dreamer Web, the Chromium component web panes need.
    AddWeb,
    Theme,
    /// Another OS window, on a new tmux session.
    NewOsWindow,
    Settings,
    About,
    Copy,
    Paste,
    Find,
    FontBigger,
    FontSmaller,
    FontReset,
    FullScreen,
    Minimize,
    ZoomOsWindow,
    BringAllToFront,
    Quit,
    /// An installed package's contributed command, by its place in the
    /// palette's `contributed` labels.
    Contributed(u16),
}

pub(crate) const ALL: [Command; 42] = [
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
    Command::ToggleDrawer,
    Command::NewDrawer,
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
    Command::OpenWebPage,
    Command::AddWeb,
    Command::Theme,
    Command::NewOsWindow,
    Command::Settings,
    Command::About,
    Command::Copy,
    Command::Paste,
    Command::Find,
    Command::FontBigger,
    Command::FontSmaller,
    Command::FontReset,
    Command::FullScreen,
    Command::Minimize,
    Command::ZoomOsWindow,
    Command::BringAllToFront,
    Command::Quit,
];

impl Command {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Command::SplitRight => "Split pane right",
            Command::SplitDown => "Split pane down",
            Command::NewWindow => "New tab (tmux window)",
            Command::RenameWindow => "Rename window…",
            Command::NextWindow => "Next tab (tmux window)",
            Command::PreviousWindow => "Previous tab (tmux window)",
            Command::NextPane => "Focus next pane",
            Command::Zoom => "Zoom or unzoom pane",
            Command::CloseView => "Close view (programs keep running)",
            Command::KillPane => "Kill pane (ends its program)…",
            Command::KillWindow => "Kill window (ends its programs)…",
            Command::ToggleSidebar => "Toggle sidebar (sessions and hosts)",
            Command::ToggleDrawer => "Harness drawer: show, focus, or hide",
            Command::NewDrawer => "New harness drawer (ends the running harness)…",
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
            Command::OpenWebPage => "Open a web page… (its URL, in a pane beside this one)",
            Command::AddWeb => "Add Dreamer Web (Chromium for web panes, about 150 MB)",
            Command::Theme => "Theme… (colors of the terminals and the window)",
            Command::NewOsWindow => "New Dreamer window (on a new session)",
            Command::Settings => "Settings… (open dreamer.yaml)",
            Command::About => "About Dreamer",
            Command::Copy => "Copy",
            Command::Paste => "Paste",
            Command::Find => "Find in pane…",
            Command::FontBigger => "Bigger text",
            Command::FontSmaller => "Smaller text",
            Command::FontReset => "Actual text size",
            Command::FullScreen => "Full screen",
            Command::Minimize => "Minimize window",
            Command::ZoomOsWindow => "Zoom window (fill the screen)",
            Command::BringAllToFront => "Bring all windows to front",
            Command::Quit => "Quit Dreamer (programs keep running)",
            Command::Contributed(_) => "",
        }
    }

    /// The app command this runs, for those that are the menus' own.
    pub(crate) fn app_action(self) -> Option<Box<dyn gpui::Action>> {
        use crate::keymap as k;
        let a: Box<dyn gpui::Action> = match self {
            Command::NewOsWindow => Box::new(k::NewWindow),
            Command::Settings => Box::new(k::OpenSettings),
            Command::About => Box::new(k::About),
            Command::Copy => Box::new(k::Copy),
            Command::Paste => Box::new(k::Paste),
            Command::Find => Box::new(k::Find),
            Command::FontBigger => Box::new(k::FontBigger),
            Command::FontSmaller => Box::new(k::FontSmaller),
            Command::FontReset => Box::new(k::FontReset),
            Command::FullScreen => Box::new(k::ToggleFullScreen),
            Command::Minimize => Box::new(k::Minimize),
            Command::ZoomOsWindow => Box::new(k::ZoomWindow),
            Command::BringAllToFront => Box::new(k::BringAllToFront),
            Command::Quit => Box::new(k::Quit),
            _ => return None,
        };
        Some(a)
    }

    /// Ends programs: asks for a second Enter.
    pub(crate) fn destructive(self) -> bool {
        matches!(
            self,
            Command::KillPane | Command::KillWindow | Command::NewDrawer
        )
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
                | Command::OpenWebPage
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
    /// Choosing a theme; the highlighted one is previewed.
    Theme(Pick),
}

/// Themes to choose from, narrowed by what is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pick {
    pub filter: String,
    pub selected: usize,
    pub choices: Vec<&'static Scheme>,
}

impl Pick {
    /// The themes the filter leaves, matched on the title or the name.
    pub(crate) fn shown(&self) -> Vec<&'static Scheme> {
        self.choices
            .iter()
            .copied()
            .filter(|s| matches(&self.filter, s.title) || matches(&self.filter, s.name))
            .collect()
    }

    pub(crate) fn highlighted(&self) -> Option<&'static Scheme> {
        self.shown().get(self.selected).copied()
    }
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
    /// Keep this theme.
    Theme(&'static Scheme),
    /// Stay open: confirm or name next, or nothing matches.
    Stay,
}

#[derive(Debug, Clone)]
pub(crate) struct Palette {
    pub filter: String,
    pub selected: usize,
    pub stage: Stage,
    /// Labels of installed packages' commands, after the built-in ones.
    pub contributed: Vec<String>,
    /// Every theme, for "Theme…", and the one in force.
    pub themes: Vec<&'static Scheme>,
    pub current: Option<&'static Scheme>,
}

impl Palette {
    pub(crate) fn new() -> Palette {
        Palette {
            filter: String::new(),
            selected: 0,
            stage: Stage::Choose,
            contributed: Vec::new(),
            themes: Vec::new(),
            current: None,
        }
    }

    /// With the themes "Theme…" offers, starting at `current`.
    pub(crate) fn with_themes(
        mut self,
        themes: Vec<&'static Scheme>,
        current: Option<&'static Scheme>,
    ) -> Palette {
        self.themes = themes;
        self.current = current;
        self
    }

    /// The theme to preview: the highlighted one while choosing a theme.
    pub(crate) fn previewed(&self) -> Option<&'static Scheme> {
        match &self.stage {
            Stage::Theme(p) => p.highlighted(),
            _ => None,
        }
    }

    /// With installed packages' commands, labelled as shown.
    pub(crate) fn with_contributed(mut self, labels: Vec<String>) -> Palette {
        self.contributed = labels;
        self
    }

    /// A command's label as shown.
    pub(crate) fn label_of(&self, c: Command) -> String {
        match c {
            Command::Contributed(i) => self
                .contributed
                .get(usize::from(i))
                .cloned()
                .unwrap_or_default(),
            c => c.label().to_owned(),
        }
    }

    /// The commands the filter leaves, in order: built-in ones first.
    pub(crate) fn shown(&self) -> Vec<Command> {
        let contributed = (0..self.contributed.len())
            .filter_map(|i| u16::try_from(i).ok())
            .map(Command::Contributed);
        ALL.into_iter()
            .chain(contributed)
            // Only a build with a web engine can add its component.
            .filter(|c| *c != Command::AddWeb || crate::webcomp::can_add())
            .filter(|c| matches(&self.filter, &self.label_of(*c)))
            .collect()
    }

    pub(crate) fn type_char(&mut self, c: char) {
        match &mut self.stage {
            Stage::Choose => {
                self.filter.push(c);
                self.selected = 0;
            }
            Stage::Name(_, name) => name.push(c),
            Stage::Theme(p) => {
                p.filter.push(c);
                p.selected = 0;
            }
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
            Stage::Theme(p) => {
                p.filter.pop();
                p.selected = 0;
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
        let (n, selected) = match &mut self.stage {
            Stage::Theme(p) => (p.shown().len(), &mut p.selected),
            _ => (self.shown().len(), &mut self.selected),
        };
        if n == 0 {
            return;
        }
        *selected = if down {
            (*selected + 1) % n
        } else {
            (*selected + n - 1) % n
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
                } else if c == Command::Theme {
                    let selected = self
                        .current
                        .and_then(|now| self.themes.iter().position(|s| s.name == now.name))
                        .unwrap_or(0);
                    self.stage = Stage::Theme(Pick {
                        filter: String::new(),
                        selected,
                        choices: self.themes.clone(),
                    });
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
            Stage::Theme(p) => p.highlighted().map_or(Decision::Stay, Decision::Theme),
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
    fn contributed_commands_follow_the_built_in_ones() {
        let mut p = Palette::new().with_contributed(vec![
            "Windows: rename the first".into(),
            "Kill nothing (a package's)".into(),
        ]);
        "kill".chars().for_each(|c| p.type_char(c));
        assert_eq!(
            p.shown(),
            vec![
                Command::KillPane,
                Command::KillWindow,
                Command::Contributed(1)
            ]
        );
        assert_eq!(
            p.label_of(Command::Contributed(1)),
            "Kill nothing (a package's)"
        );
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
                .all(|c| c.label().contains("ends")),
            "a destructive command says what it ends"
        );
    }

    #[test]
    fn theme_starts_at_the_one_in_force_and_narrows_by_typing() {
        use terminal_ghostty::core::schemes;
        let all: Vec<_> = schemes::all().collect();
        let nord = schemes::find("nord").expect("ok");
        let mut p = Palette::new().with_themes(all, Some(nord));
        "theme".chars().for_each(|c| p.type_char(c));
        assert_eq!(p.shown()[0], Command::Theme);
        assert_eq!(p.previewed(), None);
        assert_eq!(p.enter(), Decision::Stay);
        assert_eq!(p.previewed(), Some(nord), "the one in force first");
        "rose pine dawn".chars().for_each(|c| p.type_char(c));
        let dawn = p.previewed().expect("ok");
        assert_eq!(dawn.name, "rose-pine-dawn");
        p.move_selection(true);
        p.move_selection(false);
        assert_eq!(p.enter(), Decision::Theme(dawn));
        "zzzz".chars().for_each(|c| p.type_char(c));
        assert_eq!(p.enter(), Decision::Stay, "nothing left to choose");
    }

    #[test]
    fn tabs_are_tmux_windows_and_a_new_os_window_is_its_own_command() {
        let first = |filter: &str| {
            let mut p = Palette::new();
            filter.chars().for_each(|c| p.type_char(c));
            p.shown().first().copied()
        };
        assert_eq!(first("new tab"), Some(Command::NewWindow));
        assert_eq!(first("new window"), Some(Command::NewWindow));
        assert_eq!(first("next window"), Some(Command::NextWindow));
        assert_eq!(first("new dreamer window"), Some(Command::NewOsWindow));
        assert!(Command::NewOsWindow.app_action().is_some());
        assert!(Command::NewWindow.app_action().is_none());
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
