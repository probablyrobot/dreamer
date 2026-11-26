//! Screen frames on the wire: a mirror's [`mirror::ScreenFrame`] as the
//! `ScreenFrame` record (`docs/mirrors.md`, frames). Encoding runs on the
//! connection's task, never on the pane's owner.

use std::collections::HashMap;

use terminal_core::{CellWidth, Color, CursorShape, ImageLayer, Rgb, Row, Style, Underline};

/// Who a frame is for: subscription, pane, epoch, generation.
#[derive(Debug, Clone, Copy)]
pub struct Head {
    pub subscription: u64,
    pub pane: u32,
    pub epoch: u64,
    pub generation: u64,
}

pub fn encode(f: &mirror::ScreenFrame, head: Head) -> api::ScreenFrame {
    let mut styles = Styles::default();
    let changed = f
        .rows
        .iter()
        .map(|(i, row)| encode_row(u32::from(*i), row, &mut styles))
        .collect();
    let c = &f.cursor;
    let m = &f.modes;
    api::ScreenFrame {
        subscription: head.subscription,
        pane: head.pane,
        epoch: head.epoch,
        generation: head.generation,
        sequence: f.sequence,
        number: f.number,
        full: f.full,
        cols: u32::from(f.size.cols),
        rows: u32::from(f.size.rows),
        top: f.view.offset,
        total_rows: f.view.total_rows,
        changed,
        styles: styles.list,
        cursor: Some(api::ScreenCursor {
            x: u32::from(c.x),
            y: u32::from(c.y),
            visible: c.visible,
            shape: match c.shape {
                CursorShape::Block => 0,
                CursorShape::BlockHollow => 1,
                CursorShape::Bar => 2,
                CursorShape::Underline => 3,
            },
            blinking: c.blinking,
            on_wide_tail: c.on_wide_tail,
        }),
        modes: Some(api::InputModes {
            cursor_keys: m.cursor_keys,
            keypad_keys: m.keypad_keys,
            mouse_x10: m.mouse_x10,
            mouse_normal: m.mouse_normal,
            mouse_button: m.mouse_button,
            mouse_any: m.mouse_any,
            mouse_utf8: m.mouse_utf8,
            mouse_sgr: m.mouse_sgr,
            mouse_urxvt: m.mouse_urxvt,
            mouse_sgr_pixels: m.mouse_sgr_pixels,
            mouse_alternate_scroll: m.mouse_alternate_scroll,
            focus_events: m.focus_events,
            bracketed_paste: m.bracketed_paste,
            kitty_paste_events: m.kitty_paste_events,
            ignore_keypad_with_numlock: m.ignore_keypad_with_numlock,
            alt_esc_prefix: m.alt_esc_prefix,
            alt_sends_escape: m.alt_sends_escape,
            linefeed: m.linefeed,
            keyboard_locked: m.keyboard_locked,
            alt_screen: m.alt_screen,
            kitty_keyboard: u32::from(m.kitty_keyboard),
        }),
        title: f.title.clone(),
        colors: f.colors.as_ref().map(|c| api::ScreenColors {
            foreground: rgb(c.foreground),
            background: rgb(c.background),
            cursor: c.cursor.map(rgb),
            palette: c.palette.iter().copied().map(rgb).collect(),
        }),
        images: f.images.as_ref().map(|placed| api::ScreenImages {
            placements: placed
                .iter()
                .map(|p| api::ImagePlacement {
                    image: p.image.id,
                    generation: p.image.generation,
                    layer: match p.layer {
                        ImageLayer::BelowBackground => 0,
                        ImageLayer::BelowText => 1,
                        ImageLayer::AboveText => 2,
                    },
                    z: p.z,
                    col: p.col,
                    row: p.row,
                    offset_x: p.offset_x,
                    offset_y: p.offset_y,
                    width: p.width,
                    height: p.height,
                    source_x: p.source_x,
                    source_y: p.source_y,
                    source_width: p.source_width,
                    source_height: p.source_height,
                })
                .collect(),
        }),
        pixels: f
            .pixels
            .iter()
            .map(|p| api::ImagePixels {
                image: p.id,
                generation: p.generation,
                width: p.width,
                height: p.height,
                rgba: p.rgba.to_vec(),
            })
            .collect(),
        render_hold: f.render_hold,
    }
}

/// The frame's style table: style 0 is the default and is not listed.
#[derive(Default)]
struct Styles {
    index: HashMap<Style, u32>,
    list: Vec<api::CellStyle>,
}

impl Styles {
    fn of(&mut self, s: Style) -> u32 {
        if s == Style::default() {
            return 0;
        }
        if let Some(i) = self.index.get(&s) {
            return *i;
        }
        self.list.push(api::CellStyle {
            fg: color(s.fg),
            bg: color(s.bg),
            underline_color: color(s.underline_color),
            attrs: u32::from(s.attrs.0),
            underline: match s.underline {
                Underline::None => 0,
                Underline::Single => 1,
                Underline::Double => 2,
                Underline::Curly => 3,
                Underline::Dotted => 4,
                Underline::Dashed => 5,
            },
        });
        let i = self.list.len() as u32;
        self.index.insert(s, i);
        i
    }
}

fn encode_row(index: u32, row: &Row, styles: &mut Styles) -> api::ScreenRow {
    // Trailing blank cells in the default style are left out.
    let blank = |c: &terminal_core::Cell| {
        c.text.is_empty() && c.width == CellWidth::Narrow && c.style == Style::default()
    };
    let used = row.cells.len() - row.cells.iter().rev().take_while(|c| blank(c)).count();
    let mut text = String::new();
    let mut cells = Vec::with_capacity(used);
    let mut runs: Vec<u32> = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    for cell in &row.cells[..used] {
        text.push_str(&cell.text);
        let width = match cell.width {
            CellWidth::Narrow => 0,
            CellWidth::Wide => 1,
            CellWidth::SpacerTail => 2,
            CellWidth::SpacerHead => 3,
        };
        cells.push(((cell.text.chars().count() as u32) << 2) | width);
        let style = styles.of(cell.style);
        run = match run {
            Some((n, s)) if s == style => Some((n + 1, s)),
            Some((n, s)) => {
                runs.extend([n, s]);
                Some((1, style))
            }
            None => Some((1, style)),
        };
    }
    // A last run in the default style is implied.
    if let Some((n, s)) = run
        && s != 0
    {
        runs.extend([n, s]);
    }
    api::ScreenRow {
        index,
        text,
        cells,
        styles: runs,
    }
}

fn rgb(c: Rgb) -> u32 {
    (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
}

fn color(c: Color) -> u32 {
    match c {
        Color::Default => 0,
        Color::Palette(i) => 1 + u32::from(i),
        Color::Rgb(v) => 0x100_0000 | rgb(v),
    }
}
