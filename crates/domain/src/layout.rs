//! Window layout as a typed tree with exact cell geometry (spec 7.4, 15.1).

use crate::ids::TmuxPaneId;

/// A rectangle in cells, relative to the window's top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl CellRect {
    pub fn right(&self) -> u32 {
        u32::from(self.x) + u32::from(self.cols)
    }

    pub fn bottom(&self) -> u32 {
        u32::from(self.y) + u32::from(self.rows)
    }
}

/// How a split arranges its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitDirection {
    /// Children side by side, left to right (tmux `{...}`).
    Horizontal,
    /// Children stacked, top to bottom (tmux `[...]`).
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LayoutNode {
    Pane {
        rect: CellRect,
        pane: TmuxPaneId,
    },
    Split {
        rect: CellRect,
        direction: SplitDirection,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn rect(&self) -> CellRect {
        match self {
            LayoutNode::Pane { rect, .. } | LayoutNode::Split { rect, .. } => *rect,
        }
    }

    /// Every pane with its rectangle, in layout order.
    pub fn panes(&self) -> Vec<(TmuxPaneId, CellRect)> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<(TmuxPaneId, CellRect)>) {
        match self {
            LayoutNode::Pane { rect, pane } => out.push((*pane, *rect)),
            LayoutNode::Split { children, .. } => {
                for c in children {
                    c.collect(out);
                }
            }
        }
    }

    /// Structural checks: children lie inside their split, follow its
    /// direction without overlapping, and leaves name distinct panes.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        self.validate_inner(&mut seen)
    }

    fn validate_inner(
        &self,
        seen: &mut std::collections::BTreeSet<TmuxPaneId>,
    ) -> Result<(), String> {
        match self {
            LayoutNode::Pane { pane, .. } => {
                if !seen.insert(*pane) {
                    return Err(format!("pane {pane} appears twice"));
                }
                Ok(())
            }
            LayoutNode::Split {
                rect,
                direction,
                children,
            } => {
                if children.len() < 2 {
                    return Err("a split needs at least two children".into());
                }
                let mut cursor = match direction {
                    SplitDirection::Horizontal => u32::from(rect.x),
                    SplitDirection::Vertical => u32::from(rect.y),
                };
                for child in children {
                    let r = child.rect();
                    let inside = r.x >= rect.x
                        && r.y >= rect.y
                        && r.right() <= rect.right()
                        && r.bottom() <= rect.bottom();
                    if !inside {
                        return Err(format!("{r:?} lies outside its split {rect:?}"));
                    }
                    let (start, end) = match direction {
                        SplitDirection::Horizontal => (u32::from(r.x), r.right()),
                        SplitDirection::Vertical => (u32::from(r.y), r.bottom()),
                    };
                    if start < cursor {
                        return Err(format!("{r:?} overlaps its previous sibling"));
                    }
                    cursor = end;
                    child.validate_inner(seen)?;
                }
                Ok(())
            }
        }
    }
}

/// A window's layout as tmux reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WindowLayout {
    Tiled(LayoutNode),
    /// The layout uses a form this version cannot represent (for example
    /// floating panes). The window is shown through the unsupported path,
    /// never with panes silently omitted (spec 15.1).
    Unsupported {
        raw: String,
        reason: String,
    },
}
