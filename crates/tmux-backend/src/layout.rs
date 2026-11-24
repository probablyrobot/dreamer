//! Parse tmux window layouts into the domain layout tree.
//!
//! tmux 3.8 has two layout encodings:
//!
//! - **v2 (JSON)**: `{"V":2,"L":cell}`. Sent to control clients that set the
//!   `new-layouts` flag. Includes floating panes (cells with a `z` index).
//! - **v1 (legacy)**: `csum,WxH,X,Y{...}` / `[...]` / `,ID`. Sent to control
//!   clients without that flag. It omits floating panes entirely, so a
//!   connection must use v2 to avoid silently losing panes.
//!
//! A layout that cannot be represented (floating panes, unknown cell types,
//! invalid geometry) becomes [`WindowLayout::Unsupported`] with the raw text
//! and the reason; panes are never dropped silently.

use domain::{CellRect, LayoutNode, SplitDirection, TmuxPaneId, WindowLayout};
use serde_json::Value;

/// Parse either encoding.
pub fn parse_layout(raw: &str) -> WindowLayout {
    let result = if raw.trim_start().starts_with('{') {
        parse_v2(raw)
    } else {
        parse_v1(raw)
    };
    match result.and_then(|node| node.validate().map(|()| node)) {
        Ok(node) => WindowLayout::Tiled(node),
        Err(reason) => WindowLayout::Unsupported {
            raw: raw.to_owned(),
            reason,
        },
    }
}

fn parse_v2(raw: &str) -> Result<LayoutNode, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("layout JSON: {e}"))?;
    match v.get("V").and_then(Value::as_i64) {
        Some(2) => {}
        Some(other) => return Err(format!("unsupported layout version {other}")),
        None => return Err("layout JSON has no version".into()),
    }
    let root = v.get("L").ok_or("layout JSON has no root cell")?;
    let mut floating = Vec::new();
    let node = v2_cell(root, &mut floating, 0)?;
    if !floating.is_empty() {
        let ids: Vec<String> = floating.iter().map(ToString::to_string).collect();
        return Err(format!(
            "floating panes are not supported yet: {}",
            ids.join(", ")
        ));
    }
    Ok(node)
}

fn v2_u16(cell: &Value, key: &str) -> Result<u16, String> {
    cell.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("layout cell field `{key}` missing or out of range"))
}

fn v2_cell(
    cell: &Value,
    floating: &mut Vec<TmuxPaneId>,
    depth: usize,
) -> Result<LayoutNode, String> {
    if depth > 64 {
        return Err("layout nested too deeply".into());
    }
    let rect = CellRect {
        x: v2_u16(cell, "x")?,
        y: v2_u16(cell, "y")?,
        cols: v2_u16(cell, "w")?,
        rows: v2_u16(cell, "h")?,
    };
    match cell.get("t").and_then(Value::as_str) {
        Some("p") => {
            let id = cell
                .get("I")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix('%'))
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or("pane cell without a valid `I` pane id")?;
            if cell.get("z").is_some() {
                floating.push(TmuxPaneId(id));
            }
            Ok(LayoutNode::Pane {
                rect,
                pane: TmuxPaneId(id),
            })
        }
        Some(t @ ("h" | "v")) => {
            let direction = if t == "h" {
                SplitDirection::Horizontal
            } else {
                SplitDirection::Vertical
            };
            let children = cell
                .get("c")
                .and_then(Value::as_array)
                .ok_or("split cell without children")?
                .iter()
                .map(|c| v2_cell(c, floating, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LayoutNode::Split {
                rect,
                direction,
                children,
            })
        }
        other => Err(format!("unknown layout cell type {other:?}")),
    }
}

/// tmux's v1 checksum over the layout body.
pub fn v1_checksum(body: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in body.bytes() {
        csum = (csum >> 1) | ((csum & 1) << 15);
        csum = csum.wrapping_add(u16::from(b));
    }
    csum
}

fn parse_v1(raw: &str) -> Result<LayoutNode, String> {
    let (sum, body) = raw.split_once(',').ok_or("v1 layout without checksum")?;
    let expected = u16::from_str_radix(sum, 16).map_err(|_| "bad v1 checksum".to_owned())?;
    if v1_checksum(body) != expected {
        return Err("v1 layout checksum mismatch".into());
    }
    let mut p = V1 {
        s: body.as_bytes(),
        i: 0,
    };
    let node = p.cell(0)?;
    if p.i != p.s.len() {
        return Err(format!("trailing data in v1 layout at {}", p.i));
    }
    Ok(node)
}

struct V1<'a> {
    s: &'a [u8],
    i: usize,
}

impl V1<'_> {
    fn number(&mut self) -> Result<u16, String> {
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| format!("expected a number at {start}"))
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected `{}` at {}", c as char, self.i))
        }
    }

    fn cell(&mut self, depth: usize) -> Result<LayoutNode, String> {
        if depth > 64 {
            return Err("layout nested too deeply".into());
        }
        let cols = self.number()?;
        self.expect(b'x')?;
        let rows = self.number()?;
        self.expect(b',')?;
        let x = self.number()?;
        self.expect(b',')?;
        let y = self.number()?;
        let rect = CellRect { x, y, cols, rows };
        match self.s.get(self.i) {
            Some(b',') => {
                self.i += 1;
                let id = self.number()?;
                Ok(LayoutNode::Pane {
                    rect,
                    pane: TmuxPaneId(u32::from(id)),
                })
            }
            Some(&open @ (b'{' | b'[')) => {
                self.i += 1;
                let (close, direction) = if open == b'{' {
                    (b'}', SplitDirection::Horizontal)
                } else {
                    (b']', SplitDirection::Vertical)
                };
                let mut children = vec![self.cell(depth + 1)?];
                while self.s.get(self.i) == Some(&b',') {
                    self.i += 1;
                    children.push(self.cell(depth + 1)?);
                }
                self.expect(close)?;
                Ok(LayoutNode::Split {
                    rect,
                    direction,
                    children,
                })
            }
            _ => Err(format!("unexpected layout text at {}", self.i)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from tmux 3.8 (an 80x24 window split horizontally, then the
    // right side vertically; then pane %2 made floating with break-pane -W).
    const TILED_V2: &str = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":1,"i":0,"I":"%0"},{"t":"v","w":39,"h":24,"x":41,"y":0,"c":[{"t":"p","w":39,"h":12,"x":41,"y":0,"l":0,"i":1,"I":"%1"},{"t":"p","w":39,"h":11,"x":41,"y":13,"a":true,"i":2,"I":"%2"}]}]}}"#;
    const FLOATING_V2: &str = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"l":1,"i":0,"I":"%0"},{"t":"v","w":39,"h":24,"x":41,"y":0,"c":[{"t":"p","w":39,"h":24,"x":41,"y":0,"l":0,"i":1,"I":"%1"},{"t":"p","w":28,"h":6,"x":4,"y":2,"a":true,"i":2,"z":0,"I":"%2"}]}]}}"#;
    // From the recorded control session in tmux-protocol's fixtures.
    const SPLIT_V1: &str = "5285,40x6,0,0{20x6,0,0,0,19x6,21,0,1}";

    #[test]
    fn parses_tiled_v2() {
        let WindowLayout::Tiled(node) = parse_layout(TILED_V2) else {
            panic!("expected tiled")
        };
        let panes = node.panes();
        assert_eq!(panes.len(), 3);
        assert_eq!(
            panes[2],
            (
                TmuxPaneId(2),
                CellRect {
                    x: 41,
                    y: 13,
                    cols: 39,
                    rows: 11
                }
            )
        );
    }

    #[test]
    fn floating_panes_are_reported_not_dropped() {
        match parse_layout(FLOATING_V2) {
            WindowLayout::Unsupported { reason, raw } => {
                assert!(reason.contains("%2"), "{reason}");
                assert_eq!(raw, FLOATING_V2);
            }
            WindowLayout::Tiled(_) => panic!("floating layout must not parse as tiled"),
        }
    }

    #[test]
    fn parses_v1_and_checks_its_checksum() {
        let WindowLayout::Tiled(node) = parse_layout(SPLIT_V1) else {
            panic!("expected tiled")
        };
        assert_eq!(node.panes().len(), 2);
        assert!(matches!(
            parse_layout("5286,40x6,0,0{20x6,0,0,0,19x6,21,0,1}"),
            WindowLayout::Unsupported { .. }
        ));
    }

    #[test]
    fn garbage_and_unknown_cells_are_unsupported() {
        for raw in [
            "",
            "zzzz",
            r#"{"V":3,"L":{}}"#,
            r#"{"V":2,"L":{"t":"q","w":1,"h":1,"x":0,"y":0}}"#,
            r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0}}"#,
        ] {
            assert!(
                matches!(parse_layout(raw), WindowLayout::Unsupported { .. }),
                "{raw}"
            );
        }
    }
}
