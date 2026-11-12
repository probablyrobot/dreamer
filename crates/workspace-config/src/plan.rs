//! Validation and planning: a workspace file becomes typed steps.
//!
//! Splits are planned so sizes are exact in tmux: for a split with children
//! `c1..cn`, the current pane holds the region of all remaining children;
//! each child except the last is created *before* that region
//! (`split-window -b`) with its own size, and the last child keeps the
//! original pane. Percentages are converted to the share of the remaining
//! region at the moment of each split.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::ConfigError;
use crate::schema::{Direction, NodeSpec, Size, WindowSpec, WorkspaceFile};

/// A pane within a plan, numbered in creation order per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneRef(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// The user's default shell.
    Shell,
    /// Exact argv, no shell.
    Exec(Vec<String>),
    /// `/bin/sh -c <command>`.
    ShellCommand(String),
    /// The default shell, then this text typed and submitted.
    Run(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanePlan {
    pub cwd: PathBuf,
    /// Merged workspace, window, and pane environment.
    pub env: Vec<(String, String)>,
    pub start: Start,
    pub title: Option<String>,
    pub keep: bool,
}

/// The size of a newly created pane, as `split-window -l` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeArg {
    Cells(u16),
    Percent(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitStep {
    /// The pane to split.
    pub target: PaneRef,
    /// The pane this step creates, placed before `target`.
    pub new: PaneRef,
    pub direction: Direction,
    pub size: Option<SizeArg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPlan {
    pub name: Option<String>,
    /// Panes in creation order; `panes[0]` is created with the window.
    pub panes: Vec<PanePlan>,
    /// The same panes in the order the file declares them.
    pub declared: Vec<PaneRef>,
    pub splits: Vec<SplitStep>,
    /// tmux layout preset to apply after creation.
    pub arrange: Option<Arrange>,
    pub focus_pane: Option<PaneRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrange {
    Tiled,
    EvenHorizontal,
    EvenVertical,
    MainHorizontal,
    MainVertical,
}

impl Arrange {
    pub fn tmux_name(self) -> &'static str {
        match self {
            Arrange::Tiled => "tiled",
            Arrange::EvenHorizontal => "even-horizontal",
            Arrange::EvenVertical => "even-vertical",
            Arrange::MainHorizontal => "main-horizontal",
            Arrange::MainVertical => "main-vertical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub session: String,
    pub root: PathBuf,
    pub env: Vec<(String, String)>,
    pub windows: Vec<WindowPlan>,
    pub focus_window: Option<usize>,
}

struct Ctx {
    errors: Vec<ConfigError>,
}

impl Ctx {
    fn err(&mut self, at: impl Into<String>, message: impl Into<String>) {
        self.errors.push(ConfigError {
            file: None,
            line: None,
            column: None,
            at: at.into(),
            message: message.into(),
        });
    }
}

fn expand(path: &str, base: &Path) -> PathBuf {
    if path == "~" {
        return home();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home().join(rest);
    }
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn merge_env(
    ctx: &mut Ctx,
    at: &str,
    base: &BTreeMap<String, String>,
    more: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = base.clone();
    for (k, v) in more {
        if !valid_env_name(k) {
            ctx.err(at, format!("invalid environment variable name `{k}`"));
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

pub(crate) fn build(
    file: &WorkspaceFile,
    default_name: &str,
    base: &Path,
) -> Result<Plan, Vec<ConfigError>> {
    let mut ctx = Ctx { errors: Vec::new() };
    if file.version != 1 {
        ctx.err(
            "version",
            format!("unsupported version {}; expected 1", file.version),
        );
    }
    let session = file.name.clone().unwrap_or_else(|| default_name.to_owned());
    if session.is_empty() || session.contains(':') || session.contains('.') {
        ctx.err(
            "name",
            "session names must be non-empty and contain no `:` or `.`",
        );
    }
    let root = file
        .root
        .as_deref()
        .map_or_else(|| base.to_path_buf(), |r| expand(r, base));
    let env = merge_env(&mut ctx, "env", &BTreeMap::new(), &file.env);
    if file.windows.is_empty() {
        ctx.err("windows", "a workspace needs at least one window");
    }

    let mut windows = Vec::new();
    let mut focus_window = None;
    for (i, w) in file.windows.iter().enumerate() {
        let at = format!("windows[{i}]");
        if w.focus {
            if focus_window.is_some() {
                ctx.err(&at, "only one window may have `focus: true`");
            }
            focus_window = Some(i);
        }
        windows.push(window(&mut ctx, &at, w, &root, &env));
    }

    if ctx.errors.is_empty() {
        Ok(Plan {
            session,
            env: env.into_iter().collect(),
            root,
            windows,
            focus_window,
        })
    } else {
        Err(ctx.errors)
    }
}

fn window(
    ctx: &mut Ctx,
    at: &str,
    w: &WindowSpec,
    workspace_root: &Path,
    env: &BTreeMap<String, String>,
) -> WindowPlan {
    let root = w.root.as_deref().map_or_else(
        || workspace_root.to_path_buf(),
        |r| expand(r, workspace_root),
    );
    let env = merge_env(ctx, &format!("{at}.env"), env, &w.env);
    let arrange = w.arrange.as_deref().and_then(|a| {
        let preset = match a {
            "tiled" => Some(Arrange::Tiled),
            "even-horizontal" => Some(Arrange::EvenHorizontal),
            "even-vertical" => Some(Arrange::EvenVertical),
            "main-horizontal" => Some(Arrange::MainHorizontal),
            "main-vertical" => Some(Arrange::MainVertical),
            _ => None,
        };
        if preset.is_none() {
            ctx.err(
                format!("{at}.arrange"),
                format!("unknown preset `{a}`; use tiled, even-horizontal, even-vertical, main-horizontal, or main-vertical"),
            );
        }
        preset
    });

    let mut plan = WindowPlan {
        name: w.name.clone(),
        panes: Vec::new(),
        declared: Vec::new(),
        splits: Vec::new(),
        arrange,
        focus_pane: None,
    };
    let root_node = match (&w.layout, &w.panes) {
        (Some(layout), None) => {
            if arrange.is_some() {
                ctx.err(
                    format!("{at}.arrange"),
                    "`arrange` applies to `panes`, not `layout`",
                );
            }
            layout.clone()
        }
        (None, Some(panes)) => {
            if panes.iter().any(|p| p.split.is_some() || p.panes.is_some()) {
                ctx.err(format!("{at}.panes"), "use `layout` for nested splits");
            }
            match panes.as_slice() {
                [] => {
                    ctx.err(
                        format!("{at}.panes"),
                        "`panes` is empty; omit it for a single shell",
                    );
                    empty_node()
                }
                [only] => only.clone(),
                // A flat list: an even vertical stack, rearranged by the preset.
                _ => NodeSpec {
                    split: Some(Direction::Vertical),
                    panes: Some(panes.clone()),
                    ..empty_node()
                },
            }
        }
        (None, None) => empty_node(),
        (Some(_), Some(_)) => {
            ctx.err(at, "use either `layout` or `panes`, not both");
            empty_node()
        }
    };
    let first = PaneRef(0);
    plan.panes.push(placeholder(&root));
    node(
        ctx,
        &format!("{at}.layout"),
        &root_node,
        first,
        &root,
        &env,
        &mut plan,
    );
    plan
}

fn empty_node() -> NodeSpec {
    NodeSpec {
        split: None,
        panes: None,
        size: None,
        exec: None,
        shell: None,
        run: None,
        cwd: None,
        env: BTreeMap::new(),
        title: None,
        focus: false,
        keep: false,
    }
}

fn placeholder(cwd: &Path) -> PanePlan {
    PanePlan {
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
        start: Start::Shell,
        title: None,
        keep: false,
    }
}

/// Fill `slot` (already allocated) from `n`.
fn node(
    ctx: &mut Ctx,
    at: &str,
    n: &NodeSpec,
    slot: PaneRef,
    root: &Path,
    env: &BTreeMap<String, String>,
    plan: &mut WindowPlan,
) {
    let is_split = n.split.is_some() || n.panes.is_some();
    if is_split {
        let has_pane_fields = n.exec.is_some()
            || n.shell.is_some()
            || n.run.is_some()
            || n.cwd.is_some()
            || !n.env.is_empty()
            || n.title.is_some()
            || n.focus
            || n.keep;
        if has_pane_fields {
            ctx.err(at, "a split cannot also have pane fields (exec, shell, run, cwd, env, title, focus, keep)");
        }
        let Some(direction) = n.split else {
            ctx.err(
                at,
                "`panes` inside a layout needs `split: horizontal|vertical`",
            );
            return;
        };
        let children = n.panes.as_deref().unwrap_or_default();
        if children.len() < 2 {
            ctx.err(at, "a split needs at least two panes");
            return;
        }
        split(ctx, at, direction, children, slot, root, env, plan);
        return;
    }

    let starts = [n.exec.is_some(), n.shell.is_some(), n.run.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if starts > 1 {
        ctx.err(at, "use at most one of `exec`, `shell`, `run`");
    }
    let start = if let Some(argv) = &n.exec {
        if argv.is_empty() || argv[0].is_empty() {
            ctx.err(format!("{at}.exec"), "`exec` needs a program");
        }
        Start::Exec(argv.clone())
    } else if let Some(cmd) = &n.shell {
        Start::ShellCommand(cmd.clone())
    } else if let Some(cmd) = &n.run {
        if cmd.contains('\n') {
            ctx.err(
                format!("{at}.run"),
                "`run` is one command line; use `shell` for scripts",
            );
        }
        Start::Run(cmd.clone())
    } else {
        Start::Shell
    };
    let env = merge_env(ctx, &format!("{at}.env"), env, &n.env);
    if n.focus {
        if plan.focus_pane.is_some() {
            ctx.err(at, "only one pane per window may have `focus: true`");
        }
        plan.focus_pane = Some(slot);
    }
    plan.declared.push(slot);
    plan.panes[slot.0] = PanePlan {
        cwd: n
            .cwd
            .as_deref()
            .map_or_else(|| root.to_path_buf(), |c| expand(c, root)),
        env: env.into_iter().collect(),
        start,
        title: n.title.clone(),
        keep: n.keep,
    };
}

/// Requested shares of a split, as percentages or cells per child.
enum Share {
    Percent(f64),
    Cells(u16),
}

fn parse_size(ctx: &mut Ctx, at: &str, s: &Size) -> Option<Share> {
    match s {
        Size::Cells(0) => {
            ctx.err(at, "size must be at least 1 cell");
            None
        }
        Size::Cells(c) => Some(Share::Cells(*c)),
        Size::Text(t) => {
            match t
                .strip_suffix('%')
                .map(str::trim)
                .and_then(|n| n.parse::<f64>().ok())
            {
                Some(p) if p > 0.0 && p < 100.0 => Some(Share::Percent(p)),
                _ => {
                    ctx.err(at, format!("size `{t}` must be a percentage like \"30%\" (1-99) or a number of cells"));
                    None
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn split(
    ctx: &mut Ctx,
    at: &str,
    direction: Direction,
    children: &[NodeSpec],
    slot: PaneRef,
    root: &Path,
    env: &BTreeMap<String, String>,
    plan: &mut WindowPlan,
) {
    // Resolve shares: explicit percentages, explicit cells, and an equal
    // split of what is left among children without a size.
    let shares: Vec<Option<Share>> = children
        .iter()
        .enumerate()
        .map(|(i, c)| {
            c.size
                .as_ref()
                .and_then(|s| parse_size(ctx, &format!("{at}.panes[{i}].size"), s))
        })
        .collect();
    let explicit_pct: f64 = shares
        .iter()
        .filter_map(|s| match s {
            Some(Share::Percent(p)) => Some(*p),
            _ => None,
        })
        .sum();
    if explicit_pct >= 100.0 {
        ctx.err(
            at,
            format!("sizes add up to {explicit_pct}%, leaving no room"),
        );
        return;
    }
    let flexible = shares.iter().filter(|s| s.is_none()).count();
    let flexible_pct = if flexible > 0 {
        (100.0 - explicit_pct) / flexible as f64
    } else {
        0.0
    };
    if flexible == 0
        && (explicit_pct - 100.0).abs() > 0.5
        && shares.iter().all(|s| matches!(s, Some(Share::Percent(_))))
    {
        ctx.err(at, format!("sizes add up to {explicit_pct}%; they must total 100% or leave a pane without a size"));
    }
    let pct = |s: &Option<Share>| match s {
        Some(Share::Percent(p)) => Some(*p),
        Some(Share::Cells(_)) => None,
        None => Some(flexible_pct),
    };

    // Allocate a slot per child: children 0..n-1 are new panes created
    // before the remaining region; the last child keeps `slot`.
    let mut slots = Vec::with_capacity(children.len());
    let mut remaining_pct: f64 = shares.iter().filter_map(pct).sum();
    for (i, share) in shares.iter().enumerate() {
        if i + 1 == children.len() {
            slots.push(slot);
            break;
        }
        let new = PaneRef(plan.panes.len());
        plan.panes.push(placeholder(root));
        let size = match share {
            Some(Share::Cells(c)) => Some(SizeArg::Cells(*c)),
            _ => {
                let p = pct(share).unwrap_or(0.0);
                let of_remaining = if remaining_pct > 0.0 {
                    p / remaining_pct * 100.0
                } else {
                    50.0
                };
                remaining_pct -= p;
                Some(SizeArg::Percent(of_remaining.round().clamp(1.0, 99.0) as u8))
            }
        };
        plan.splits.push(SplitStep {
            target: slot,
            new,
            direction,
            size,
        });
        slots.push(new);
    }
    for (i, (child, child_slot)) in children.iter().zip(slots).enumerate() {
        node(
            ctx,
            &format!("{at}.panes[{i}]"),
            child,
            child_slot,
            root,
            env,
            plan,
        );
    }
}
