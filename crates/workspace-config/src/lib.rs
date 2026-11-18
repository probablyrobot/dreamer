//! Declarative workspace layouts.
//!
//! A workspace file (YAML) describes a tmux session: its windows, each
//! window's split tree with sizes, and what runs in every pane. This crate
//! finds the configuration directory, parses and validates files with
//! precise errors, and turns a workspace into a [`Plan`] of typed steps.
//! It never talks to tmux; `tmux-backend` applies plans. Keeping the model
//! here lets a future settings UI read, edit, and validate the same files.
//!
//! See `docs/workspaces.md` for the file format.

pub mod agents;
pub mod browser;
pub mod check;
pub mod desktop_state;
mod dirs;
pub mod edit;
pub mod extensions;
pub mod hosts;
mod plan;
mod schema;
pub mod settings;
pub mod themes;

pub use dirs::{config_dir, workspace_dir, workspace_path};
pub use plan::{Arrange, PanePlan, PaneRef, Plan, SizeArg, SplitStep, Start, WindowPlan};
pub use schema::{Direction, NodeSpec, Size, WindowSpec, WorkspaceFile};
pub use settings::{
    Appearance, ClipboardSettings, ClipboardTmux, ClipboardWrite, Decorations, DrawerSettings,
    DrawerSide, FallbackSettings, FontSettings, LinkSettings, MemoryProvider, MemorySettings,
    MiddleClick, STARTER_SETTINGS, Settings, TierSettings, WindowSettings, ensure_settings_file,
    load_settings, settings_path,
};

use std::path::{Path, PathBuf};

/// A problem in a workspace file, with its location when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// Path to the offending element, such as `windows[1].layout.panes[0]`.
    pub at: String,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}", file.display())?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
                if let Some(col) = self.column {
                    write!(f, ":{col}")?;
                }
            }
            write!(f, ": ")?;
        }
        if !self.at.is_empty() {
            write!(f, "{}: ", self.at)?;
        }
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Parse a workspace from YAML text. `origin` names the file for errors and
/// is the base for relative roots.
pub fn parse(text: &str, origin: Option<&Path>) -> Result<WorkspaceFile, ConfigError> {
    serde_norway::from_str::<WorkspaceFile>(text).map_err(|e| {
        let loc = e.location();
        ConfigError {
            file: origin.map(Path::to_path_buf),
            line: loc.as_ref().map(serde_norway::Location::line),
            column: loc.as_ref().map(serde_norway::Location::column),
            at: String::new(),
            message: e.to_string(),
        }
    })
}

/// Read, parse, validate, and plan a workspace file.
pub fn load(path: &Path) -> Result<Plan, Vec<ConfigError>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        vec![ConfigError {
            file: Some(path.to_path_buf()),
            line: None,
            column: None,
            at: String::new(),
            message: e.to_string(),
        }]
    })?;
    let file = parse(&text, Some(path)).map_err(|e| vec![e])?;
    let default_name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    plan::build(&file, &default_name, &base).map_err(|errs| {
        errs.into_iter()
            .map(|mut e| {
                e.file = Some(path.to_path_buf());
                e
            })
            .collect()
    })
}
