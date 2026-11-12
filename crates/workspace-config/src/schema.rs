//! The YAML schema (version 1). Unknown keys are rejected so typos surface
//! as errors instead of silently doing nothing.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFile {
    /// Schema version; currently 1.
    pub version: u32,
    /// Session name. Defaults to the file name without extension.
    pub name: Option<String>,
    /// Default working directory: absolute, `~/...`, or relative to the
    /// file's directory.
    pub root: Option<String>,
    /// Environment for every pane.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub windows: Vec<WindowSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSpec {
    pub name: Option<String>,
    /// Working directory for this window's panes, relative to the workspace
    /// root unless absolute.
    pub root: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Select this window after creation.
    #[serde(default)]
    pub focus: bool,
    /// A split tree. Use either `layout` or `panes`.
    pub layout: Option<NodeSpec>,
    /// A flat list of panes, arranged by `arrange`.
    pub panes: Option<Vec<NodeSpec>>,
    /// A tmux layout preset applied after the panes exist.
    pub arrange: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Children side by side, left to right.
    Horizontal,
    /// Children stacked, top to bottom.
    Vertical,
}

/// A size relative to the parent: `"60%"` or a number of cells.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Size {
    Cells(u16),
    Text(String),
}

/// A layout node: a split (with `split` and `panes`) or a pane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    // Split fields.
    pub split: Option<Direction>,
    pub panes: Option<Vec<NodeSpec>>,
    /// Share of the parent split (any node may have one).
    pub size: Option<Size>,

    // Pane fields: at most one way to start.
    /// Run this program directly (argv, no shell).
    pub exec: Option<Vec<String>>,
    /// Run this command with `/bin/sh -c`; the pane closes when it ends
    /// unless `keep` is set.
    pub shell: Option<String>,
    /// Start an interactive shell and type this command into it; the shell
    /// stays when the command ends.
    pub run: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Pane title shown in pane headers.
    pub title: Option<String>,
    /// Select this pane after creation.
    #[serde(default)]
    pub focus: bool,
    /// Keep the pane open after its process exits.
    #[serde(default)]
    pub keep: bool,
}
