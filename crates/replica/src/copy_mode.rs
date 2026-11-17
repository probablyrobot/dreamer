//! Copy mode as tmux has it, over a pane's local history: a cursor that
//! moves and searches with tmux's vi or emacs copy-mode keys, a selection
//! (plain or rectangle), and the text to copy. tmux draws its own copy
//! mode only for ordinary clients, so Dreamer keeps this one and draws it.
//!
//! This is a pure state machine over the history's text (one row per
//! line, as the engine formats it): it has no clock, no I/O, and nothing
//! it does changes the terminal. Rows count from the top of history, as
//! the viewport's offset does; columns are cells.

use unicode_width::UnicodeWidthChar;

use crate::keys::{Code, Key};

/// Which of tmux's copy-mode key tables (`mode-keys`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Vi,
    Emacs,
}

impl Table {
    /// The table for a `mode-keys` value; tmux's default is emacs.
    pub fn for_mode_keys(value: &str) -> Table {
        if value.trim() == "vi" {
            Table::Vi
        } else {
            Table::Emacs
        }
    }
}

/// A cell: a row from the top of history and a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

/// What a key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Still in copy mode (redraw).
    Stay,
    /// Copy this text and leave copy mode.
    Copy(String),
    /// Leave copy mode without copying.
    Leave,
}

/// A selected stretch of one row: columns `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Prompt {
    down: bool,
    text: String,
}

#[derive(Debug, Clone)]
pub struct CopyMode {
    table: Table,
    lines: Vec<String>,
    height: usize,
    width: usize,
    cursor: Pos,
    /// The column vertical moves aim for, as in tmux.
    want_col: usize,
    top: usize,
    anchor: Option<Pos>,
    rect: bool,
    prompt: Option<Prompt>,
    /// The last search: its text and whether it went down.
    search: Option<(String, bool)>,
    message: Option<String>,
}

/// One cell's character(s): its first column, width, and byte range in
/// the line (with any zero-width characters that follow it).
#[derive(Debug, Clone, Copy)]
struct Cell {
    col: usize,
    width: usize,
    start: usize,
    end: usize,
}

fn cells(line: &str) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    let mut col = 0;
    for (i, ch) in line.char_indices() {
        let end = i + ch.len_utf8();
        match ch.width() {
            Some(w) if w > 0 => {
                out.push(Cell {
                    col,
                    width: w,
                    start: i,
                    end,
                });
                col += w;
            }
            _ => {
                if let Some(last) = out.last_mut() {
                    last.end = end;
                }
            }
        }
    }
    out
}

/// Character classes for word motions: blank, word, punctuation.
fn class(ch: Option<char>, big: bool) -> u8 {
    match ch {
        None => 0,
        Some(c) if c.is_whitespace() => 0,
        Some(_) if big => 1,
        Some(c) if c.is_alphanumeric() || c == '_' => 1,
        Some(_) => 2,
    }
}

impl CopyMode {
    /// Enter copy mode over `text` (history and screen, one row per
    /// line; `rows` of them), shown `height` rows at a time from `top`,
    /// `width` cells wide, with the cursor at `cursor`.
    pub fn new(
        table: Table,
        text: &str,
        rows: usize,
        height: usize,
        width: usize,
        top: usize,
        cursor: Pos,
    ) -> CopyMode {
        let mut c = CopyMode {
            table,
            lines: Vec::new(),
            height: height.max(1),
            width: width.max(1),
            cursor,
            want_col: cursor.col,
            top,
            anchor: None,
            rect: false,
            prompt: None,
            search: None,
            message: None,
        };
        c.set_text(text, rows);
        c
    }

    /// New text for the same history (more output arrived): positions
    /// stay, clamped to it.
    pub fn set_text(&mut self, text: &str, rows: usize) {
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        lines.resize(rows.max(1), String::new());
        self.lines = lines;
        let last = self.lines.len() - 1;
        self.cursor.row = self.cursor.row.min(last);
        if let Some(a) = &mut self.anchor {
            a.row = a.row.min(last);
        }
        self.cursor.col = self.snap(self.cursor.row, self.cursor.col);
        self.top = self.top.min(self.max_top());
        self.keep_visible();
    }

    pub fn table(&self) -> Table {
        self.table
    }

    /// The first row shown.
    pub fn top(&self) -> usize {
        self.top
    }

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    pub fn rectangle(&self) -> bool {
        self.rect
    }

    pub fn selecting(&self) -> bool {
        self.anchor.is_some()
    }

    /// tmux's position indicator: `[rows back/history rows]`.
    pub fn indicator(&self) -> String {
        let history = self.lines.len().saturating_sub(self.height);
        format!("[{}/{}]", history.saturating_sub(self.top), history)
    }

    /// The search prompt being typed, as tmux shows it.
    pub fn prompt(&self) -> Option<String> {
        self.prompt
            .as_ref()
            .map(|p| format!("(search {}) {}", if p.down { "down" } else { "up" }, p.text))
    }

    /// A message for the user (`Search failed`), until the next key.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    fn last_row(&self) -> usize {
        self.lines.len() - 1
    }

    /// Columns of text on a row.
    fn line_len(&self, row: usize) -> usize {
        cells(&self.lines[row])
            .last()
            .map_or(0, |c| c.col + c.width)
    }

    /// The start of the cell holding `col` (a wide character's second
    /// column moves to its first), within the screen.
    fn snap(&self, row: usize, col: usize) -> usize {
        let col = col.min(self.width - 1);
        cells(&self.lines[row])
            .iter()
            .find(|c| c.col <= col && col < c.col + c.width)
            .map_or(col, |c| c.col)
    }

    fn cell_width(&self, p: Pos) -> usize {
        cells(&self.lines[p.row])
            .iter()
            .find(|c| c.col == p.col)
            .map_or(1, |c| c.width)
    }

    fn keep_visible(&mut self) {
        if self.cursor.row < self.top {
            self.top = self.cursor.row;
        } else if self.cursor.row >= self.top + self.height {
            self.top = self.cursor.row + 1 - self.height;
        }
        self.top = self.top.min(self.max_top());
    }

    /// The last column the cursor rests on at the end of a line: on the
    /// last character in vi, just past it in emacs.
    fn end_col(&self, row: usize) -> usize {
        let len = self.line_len(row);
        match self.table {
            Table::Vi => cells(&self.lines[row]).last().map_or(0, |c| c.col),
            Table::Emacs => len.min(self.width - 1),
        }
    }

    fn move_to(&mut self, p: Pos) {
        let row = p.row.min(self.last_row());
        self.cursor = Pos {
            row,
            col: self.snap(row, p.col),
        };
        self.want_col = self.cursor.col;
        self.keep_visible();
    }

    fn vertical(&mut self, rows: isize) {
        let row = self
            .cursor
            .row
            .saturating_add_signed(rows)
            .min(self.last_row());
        let col = self.want_col.min(self.end_col(row));
        self.cursor = Pos {
            row,
            col: self.snap(row, col),
        };
        self.keep_visible();
    }

    fn left(&mut self) {
        let Pos { row, col } = self.cursor;
        if col == 0 {
            if row > 0 {
                let end = self.end_col(row - 1);
                self.move_to(Pos {
                    row: row - 1,
                    col: end,
                });
            }
            return;
        }
        self.move_to(Pos { row, col: col - 1 });
    }

    fn right(&mut self) {
        let Pos { row, col } = self.cursor;
        if col >= self.end_col(row) {
            if row < self.last_row() {
                self.move_to(Pos {
                    row: row + 1,
                    col: 0,
                });
            }
            return;
        }
        let w = self.cell_width(self.cursor);
        self.move_to(Pos { row, col: col + w });
    }

    /// Scroll the view by `rows` with the cursor keeping its place on
    /// screen, as tmux's page and half-page moves do.
    fn scroll(&mut self, rows: isize) {
        let top = self.top.saturating_add_signed(rows).min(self.max_top());
        let moved = top as isize - self.top as isize;
        self.top = top;
        let row = if moved == 0 {
            // Already at an end: the cursor goes there instead.
            if rows < 0 { 0 } else { self.last_row() }
        } else {
            self.cursor.row.saturating_add_signed(moved)
        };
        let row = row.min(self.last_row());
        let col = self.want_col.min(self.end_col(row));
        self.cursor = Pos {
            row,
            col: self.snap(row, col),
        };
        self.keep_visible();
    }

    /// Scroll one row, the cursor staying on its row while it is in view.
    fn scroll_line(&mut self, rows: isize) {
        self.top = self.top.saturating_add_signed(rows).min(self.max_top());
        let row = self
            .cursor
            .row
            .clamp(self.top, self.top + self.height - 1)
            .min(self.last_row());
        if row != self.cursor.row {
            let col = self.want_col.min(self.end_col(row));
            self.cursor = Pos {
                row,
                col: self.snap(row, col),
            };
        }
    }

    // ---- word motions over a stream of cells; each row ends in a blank.

    /// The cell index of the cursor in its row (the row's length when it
    /// is past the text).
    fn index(&self, p: Pos) -> (usize, usize) {
        let cs = cells(&self.lines[p.row]);
        let i = cs.iter().position(|c| c.col >= p.col).unwrap_or(cs.len());
        (p.row, i)
    }

    fn char_at(&self, (row, i): (usize, usize)) -> Option<char> {
        let cs = cells(&self.lines[row]);
        cs.get(i)
            .and_then(|c| self.lines[row][c.start..c.end].chars().next())
    }

    fn next(&self, (row, i): (usize, usize)) -> Option<(usize, usize)> {
        if i < cells(&self.lines[row]).len() {
            Some((row, i + 1))
        } else if row < self.last_row() {
            Some((row + 1, 0))
        } else {
            None
        }
    }

    fn prev(&self, (row, i): (usize, usize)) -> Option<(usize, usize)> {
        if i > 0 {
            Some((row, i - 1))
        } else if row > 0 {
            Some((row - 1, cells(&self.lines[row - 1]).len()))
        } else {
            None
        }
    }

    fn pos_of(&self, (row, i): (usize, usize)) -> Pos {
        let cs = cells(&self.lines[row]);
        let col = cs
            .get(i)
            .map_or_else(|| cs.last().map_or(0, |c| c.col + c.width), |c| c.col);
        Pos { row, col }
    }

    /// The start of the next word (vi `w`).
    fn next_word(&mut self, big: bool) {
        let mut at = self.index(self.cursor);
        let start = class(self.char_at(at), big);
        if start != 0 {
            while class(self.char_at(at), big) == start {
                match self.next(at) {
                    Some(n) => at = n,
                    None => return,
                }
            }
        }
        while class(self.char_at(at), big) == 0 {
            match self.next(at) {
                Some(n) => at = n,
                None => return,
            }
        }
        self.move_to(self.pos_of(at));
    }

    /// The last character of this or the next word (vi `e`); with
    /// `past`, the cell after it (emacs `M-f`).
    fn word_end(&mut self, big: bool, past: bool) {
        let Some(mut at) = self.next(self.index(self.cursor)) else {
            return;
        };
        // Emacs moves past the word it is in, from wherever in it.
        if past {
            at = self.index(self.cursor);
        }
        while class(self.char_at(at), big) == 0 {
            match self.next(at) {
                Some(n) => at = n,
                None => return,
            }
        }
        let c = class(self.char_at(at), big);
        while let Some(n) = self.next(at)
            && class(self.char_at(n), big) == c
        {
            at = n;
        }
        if past && let Some(n) = self.next(at) {
            at = n;
        }
        self.move_to(self.pos_of(at));
    }

    /// The start of this or the previous word (vi `b`, emacs `M-b`).
    fn previous_word(&mut self, big: bool) {
        let Some(mut at) = self.prev(self.index(self.cursor)) else {
            return;
        };
        while class(self.char_at(at), big) == 0 {
            match self.prev(at) {
                Some(p) => at = p,
                None => {
                    self.move_to(self.pos_of(at));
                    return;
                }
            }
        }
        let c = class(self.char_at(at), big);
        while let Some(p) = self.prev(at)
            && class(self.char_at(p), big) == c
        {
            at = p;
        }
        self.move_to(self.pos_of(at));
    }

    fn first_nonblank(&mut self) {
        let row = self.cursor.row;
        let col = cells(&self.lines[row])
            .iter()
            .find(|c| !self.lines[row][c.start..c.end].trim().is_empty())
            .map_or(0, |c| c.col);
        self.move_to(Pos { row, col });
    }

    // ---- search

    /// Find `text` from the cursor, down (towards newer rows) or up,
    /// wrapping around as tmux does. Case is ignored when the text has no
    /// capital letter.
    fn find(&mut self, text: &str, down: bool) {
        let ignore_case = !text.chars().any(char::is_uppercase);
        let fold = |c: char| {
            if ignore_case {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c
            }
        };
        let needle: Vec<char> = text.chars().map(fold).collect();
        if needle.is_empty() {
            return;
        }
        // Columns where `needle` starts in a row.
        let hits = |row: usize| -> Vec<usize> {
            let line = &self.lines[row];
            let chars: Vec<(usize, char)> =
                line.char_indices().map(|(i, c)| (i, fold(c))).collect();
            let mut out = Vec::new();
            if chars.len() < needle.len() {
                return out;
            }
            for s in 0..=chars.len() - needle.len() {
                if chars[s..s + needle.len()]
                    .iter()
                    .zip(&needle)
                    .all(|((_, a), b)| a == b)
                {
                    let byte = chars[s].0;
                    out.push(line[..byte].chars().filter_map(|c| c.width()).sum());
                }
            }
            out
        };
        let n = self.lines.len();
        let Pos { row, col } = self.cursor;
        for step in 0..=n {
            let r = if down {
                (row + step) % n
            } else {
                (row + n - step % n) % n
            };
            let found = hits(r);
            let pick = if step == 0 {
                if down {
                    found.into_iter().find(|&c| c > col)
                } else {
                    found.into_iter().rev().find(|&c| c < col)
                }
            } else if step == n {
                // Back on the cursor's row from the other side.
                if down {
                    found.into_iter().find(|&c| c <= col)
                } else {
                    found.into_iter().rev().find(|&c| c >= col)
                }
            } else if down {
                found.into_iter().next()
            } else {
                found.into_iter().next_back()
            };
            if let Some(c) = pick {
                self.move_to(Pos { row: r, col: c });
                return;
            }
        }
        self.message = Some("Search failed".into());
    }

    fn search_again(&mut self, reverse: bool) {
        match self.search.clone() {
            Some((text, down)) => self.find(&text, down != reverse),
            None => self.message = Some("No previous search".into()),
        }
    }

    // ---- selection

    fn begin_selection(&mut self) {
        self.anchor = Some(self.cursor);
    }

    fn clear_selection(&mut self) {
        self.anchor = None;
        self.rect = false;
    }

    /// The selection's ends in reading order, the second one past its
    /// last cell (vi includes the cursor's cell; emacs ends before it).
    fn bounds(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        let (s, e) = if a <= self.cursor {
            (a, self.cursor)
        } else {
            (self.cursor, a)
        };
        let end = |p: Pos| match self.table {
            Table::Vi => Pos {
                row: p.row,
                col: p.col + self.cell_width(p),
            },
            Table::Emacs => p,
        };
        if self.rect {
            let (c0, c1) = (a.col.min(self.cursor.col), a.col.max(self.cursor.col));
            let right = if self.table == Table::Vi {
                c1 + self.cell_width(Pos {
                    row: if c1 == a.col { a.row } else { self.cursor.row },
                    col: c1,
                })
            } else {
                c1
            };
            return Some((
                Pos {
                    row: s.row,
                    col: c0,
                },
                Pos {
                    row: e.row,
                    col: right,
                },
            ));
        }
        // The anchor ends the selection when it is the later end.
        Some((s, end(e)))
    }

    /// The selected cells of every row, in order.
    pub fn spans(&self) -> Vec<Span> {
        let Some((s, e)) = self.bounds() else {
            return Vec::new();
        };
        (s.row..=e.row)
            .filter_map(|row| {
                let (start, end) = if self.rect {
                    (s.col, e.col)
                } else {
                    (
                        if row == s.row { s.col } else { 0 },
                        if row == e.row { e.col } else { self.width },
                    )
                };
                (end > start).then_some(Span { row, start, end })
            })
            .collect()
    }

    /// The selected spans on rows in view.
    pub fn visible_spans(&self) -> Vec<Span> {
        self.spans()
            .into_iter()
            .filter(|s| s.row >= self.top && s.row < self.top + self.height)
            .collect()
    }

    /// The selection's text: each row's selected cells with trailing
    /// blanks trimmed, rows joined with newlines.
    pub fn selection_text(&self) -> Option<String> {
        self.anchor?;
        let rows: Vec<String> = self
            .spans()
            .iter()
            .map(|sp| {
                let line = &self.lines[sp.row];
                let mut out = String::new();
                for c in cells(line) {
                    if c.col >= sp.start && c.col < sp.end {
                        out.push_str(&line[c.start..c.end]);
                    }
                }
                out.trim_end().to_owned()
            })
            .collect();
        let text = rows.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn copy(&mut self) -> Outcome {
        match self.selection_text() {
            Some(t) => Outcome::Copy(t),
            None => Outcome::Leave,
        }
    }

    // ---- keys

    /// Handle one key; see `docs/input.md` for the bindings.
    pub fn key(&mut self, k: Key) -> Outcome {
        self.message = None;
        if self.prompt.is_some() {
            self.prompt_key(k);
            return Outcome::Stay;
        }
        match self.table {
            Table::Vi => self.vi(k),
            Table::Emacs => self.emacs(k),
        }
    }

    fn prompt_key(&mut self, k: Key) {
        let Some(p) = self.prompt.as_mut() else {
            return;
        };
        match (k.code, k.ctrl, k.alt) {
            (Code::Enter, false, false) => {
                let Some(p) = self.prompt.take() else { return };
                if p.text.is_empty() {
                    self.search_again(false);
                } else {
                    self.search = Some((p.text.clone(), p.down));
                    self.find(&p.text, p.down);
                }
            }
            (Code::Escape, ..) | (Code::Char('c' | 'g'), true, false) => self.prompt = None,
            (Code::Backspace, false, false) => {
                p.text.pop();
            }
            (Code::Char('u'), true, false) => p.text.clear(),
            (Code::Char(c), false, false) => p.text.push(c),
            _ => {}
        }
    }

    fn common(&mut self, k: Key) -> Option<Outcome> {
        let page = self.height as isize;
        match (k.code, k.ctrl, k.alt) {
            (Code::Up, false, false) => self.vertical(-1),
            (Code::Down, false, false) => self.vertical(1),
            (Code::Left, false, false) => self.left(),
            (Code::Right, false, false) => self.right(),
            (Code::Home, false, false) => self.move_to(Pos {
                row: self.cursor.row,
                col: 0,
            }),
            (Code::End, false, false) => self.move_to(Pos {
                row: self.cursor.row,
                col: self.end_col(self.cursor.row),
            }),
            (Code::PageUp, false, false) => self.scroll(-page),
            (Code::PageDown, false, false) => self.scroll(page),
            (Code::Up, true, false) => self.scroll_line(-1),
            (Code::Down, true, false) => self.scroll_line(1),
            (Code::Char('n'), false, false) => self.search_again(false),
            (Code::Char('N'), false, false) => self.search_again(true),
            (Code::Char('q'), false, false) => return Some(Outcome::Leave),
            _ => return None,
        }
        Some(Outcome::Stay)
    }

    fn vi(&mut self, k: Key) -> Outcome {
        if let Some(o) = self.common(k) {
            return o;
        }
        let page = self.height as isize;
        let half = (self.height / 2).max(1) as isize;
        match (k.code, k.ctrl, k.alt) {
            (Code::Char('h'), false, false) | (Code::Backspace, false, false) => self.left(),
            (Code::Char('l'), false, false) => self.right(),
            (Code::Char('j'), false, false) => self.vertical(1),
            (Code::Char('k'), false, false) => self.vertical(-1),
            (Code::Char('w'), false, false) => self.next_word(false),
            (Code::Char('W'), false, false) => self.next_word(true),
            (Code::Char('b'), false, false) => self.previous_word(false),
            (Code::Char('B'), false, false) => self.previous_word(true),
            (Code::Char('e'), false, false) => self.word_end(false, false),
            (Code::Char('E'), false, false) => self.word_end(true, false),
            (Code::Char('0'), false, false) => self.move_to(Pos {
                row: self.cursor.row,
                col: 0,
            }),
            (Code::Char('^'), false, false) => self.first_nonblank(),
            (Code::Char('$'), false, false) => self.move_to(Pos {
                row: self.cursor.row,
                col: self.end_col(self.cursor.row),
            }),
            (Code::Char('g'), false, false) => self.move_to(Pos::default()),
            (Code::Char('G'), false, false) => {
                let row = self.last_row();
                self.move_to(Pos {
                    row,
                    col: self.end_col(row),
                });
            }
            (Code::Char('H'), false, false) => self.move_to(Pos {
                row: self.top,
                col: 0,
            }),
            (Code::Char('M'), false, false) => self.move_to(Pos {
                row: (self.top + self.height / 2).min(self.last_row()),
                col: 0,
            }),
            (Code::Char('L'), false, false) => self.move_to(Pos {
                row: (self.top + self.height - 1).min(self.last_row()),
                col: 0,
            }),
            (Code::Char('u'), true, false) => self.scroll(-half),
            (Code::Char('d'), true, false) => self.scroll(half),
            (Code::Char('b'), true, false) => self.scroll(-page),
            (Code::Char('f'), true, false) => self.scroll(page),
            (Code::Char('y'), true, false) => self.scroll_line(-1),
            (Code::Char('e'), true, false) => self.scroll_line(1),
            (Code::Char('/'), false, false) => {
                self.prompt = Some(Prompt {
                    down: true,
                    text: String::new(),
                });
            }
            (Code::Char('?'), false, false) => {
                self.prompt = Some(Prompt {
                    down: false,
                    text: String::new(),
                });
            }
            (Code::Char('v' | ' '), false, false) => self.begin_selection(),
            (Code::Char('v'), true, false) => self.rect = !self.rect,
            (Code::Char('o'), false, false) => {
                if let Some(a) = self.anchor.replace(self.cursor) {
                    self.move_to(a);
                }
            }
            (Code::Char('y'), false, false) | (Code::Enter, false, false) => return self.copy(),
            (Code::Escape, false, false) => {
                if self.anchor.is_none() {
                    return Outcome::Leave;
                }
                self.clear_selection();
            }
            (Code::Char('c'), true, false) => return Outcome::Leave,
            _ => {}
        }
        Outcome::Stay
    }

    fn emacs(&mut self, k: Key) -> Outcome {
        if let Some(o) = self.common(k) {
            return o;
        }
        let page = self.height as isize;
        match (k.code, k.ctrl, k.alt) {
            (Code::Char('b'), true, false) => self.left(),
            (Code::Char('f'), true, false) => self.right(),
            (Code::Char('p'), true, false) => self.vertical(-1),
            (Code::Char('n'), true, false) => self.vertical(1),
            (Code::Char('a'), true, false) => self.move_to(Pos {
                row: self.cursor.row,
                col: 0,
            }),
            (Code::Char('e'), true, false) => self.move_to(Pos {
                row: self.cursor.row,
                col: self.end_col(self.cursor.row),
            }),
            (Code::Char('m'), false, true) => self.first_nonblank(),
            (Code::Char('f'), false, true) => self.word_end(false, true),
            (Code::Char('b'), false, true) => self.previous_word(false),
            (Code::Char('<'), false, true) => self.move_to(Pos::default()),
            (Code::Char('>'), false, true) => {
                let row = self.last_row();
                self.move_to(Pos {
                    row,
                    col: self.end_col(row),
                });
            }
            (Code::Char('v'), true, false) => self.scroll(page),
            (Code::Char('v'), false, true) => self.scroll(-page),
            (Code::Char('s'), true, false) => {
                self.prompt = Some(Prompt {
                    down: true,
                    text: String::new(),
                });
            }
            (Code::Char('r'), true, false) => {
                self.prompt = Some(Prompt {
                    down: false,
                    text: String::new(),
                });
            }
            (Code::Char(' '), true, false) => self.begin_selection(),
            (Code::Char('R'), false, false) => self.rect = !self.rect,
            (Code::Char('w'), false, true) | (Code::Char('w'), true, false) => {
                return self.copy();
            }
            (Code::Char('g'), true, false) => self.clear_selection(),
            (Code::Escape, false, false) | (Code::Char('c'), true, false) => {
                return Outcome::Leave;
            }
            _ => {}
        }
        Outcome::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::parse;

    /// Twenty rows of history and a five-row screen.
    fn text() -> String {
        let mut rows: Vec<String> = (0..20)
            .map(|i| format!("history {i:02} alpha-beta gamma"))
            .collect();
        rows.push("$ echo hello world".into());
        rows.push("hello world".into());
        rows.push("wide 世界 end".into());
        rows.push("  indented line".into());
        rows.push("$ ".into());
        rows.join("\n")
    }

    fn mode(table: Table) -> CopyMode {
        // The screen is rows 20..25, the cursor after the last prompt.
        CopyMode::new(table, &text(), 25, 5, 40, 20, Pos { row: 24, col: 2 })
    }

    fn keys(c: &mut CopyMode, names: &[&str]) -> Outcome {
        let mut last = Outcome::Stay;
        for n in names {
            last = c.key(parse(n).unwrap_or_else(|| panic!("key {n}")));
        }
        last
    }

    fn at(c: &CopyMode) -> (usize, usize) {
        (c.cursor().row, c.cursor().col)
    }

    #[test]
    fn vi_moves_by_character_line_and_word() {
        let mut c = mode(Table::Vi);
        keys(&mut c, &["k", "k", "0"]);
        assert_eq!(at(&c), (22, 0));
        keys(&mut c, &["w"]);
        assert_eq!(at(&c), (22, 5), "the wide word");
        keys(&mut c, &["e"]);
        assert_eq!(at(&c), (22, 7), "on the second wide character");
        keys(&mut c, &["l"]);
        assert_eq!(at(&c), (22, 9), "a wide character is one step");
        keys(&mut c, &["$"]);
        assert_eq!(at(&c), (22, 12));
        keys(&mut c, &["b", "b"]);
        assert_eq!(at(&c), (22, 5));
        keys(&mut c, &["j", "^"]);
        assert_eq!(at(&c), (23, 2));
        keys(&mut c, &["k", "k", "0", "w"]);
        assert_eq!(at(&c), (21, 6));
        keys(&mut c, &["w"]);
        assert_eq!(at(&c), (22, 0), "words go on across rows");
        keys(&mut c, &["W"]);
        assert_eq!(at(&c), (22, 5));
        keys(&mut c, &["g"]);
        assert_eq!((at(&c), c.top()), ((0, 0), 0));
        assert_eq!(c.indicator(), "[20/20]");
        keys(&mut c, &["G"]);
        assert_eq!((at(&c), c.top()), ((24, 1), 20));
        assert_eq!(c.indicator(), "[0/20]");
    }

    #[test]
    fn vi_pages_scroll_with_the_cursor() {
        let mut c = mode(Table::Vi);
        keys(&mut c, &["C-u"]);
        assert_eq!((c.top(), c.cursor().row), (18, 22));
        keys(&mut c, &["C-b"]);
        assert_eq!((c.top(), c.cursor().row), (13, 17));
        keys(&mut c, &["C-f", "C-d"]);
        assert_eq!(c.top(), 20);
        keys(&mut c, &["H"]);
        assert_eq!(at(&c), (20, 0));
        keys(&mut c, &["L"]);
        assert_eq!(at(&c), (24, 0));
        keys(&mut c, &["C-y"]);
        assert_eq!((c.top(), c.cursor().row), (19, 23));
    }

    #[test]
    fn a_vi_selection_across_scrollback_copies_its_text() {
        let mut c = mode(Table::Vi);
        // From "world" on the echo line, back into history.
        keys(&mut c, &["k", "k", "k", "k", "0", "w", "w", "w"]);
        assert_eq!(at(&c), (20, 13));
        keys(&mut c, &["v", "k", "k", "0"]);
        assert!(c.selecting());
        let out = keys(&mut c, &["y"]);
        assert_eq!(
            out,
            Outcome::Copy(
                "history 18 alpha-beta gamma\nhistory 19 alpha-beta gamma\n$ echo hello w".into()
            ),
            "vi includes the cell under the cursor"
        );
    }

    #[test]
    fn a_rectangle_copies_the_same_columns_of_each_row() {
        let mut c = mode(Table::Vi);
        keys(&mut c, &["g", "w", "v", "C-v", "j", "j", "e"]);
        assert!(c.rectangle());
        assert_eq!(at(&c), (2, 9));
        assert_eq!(c.selection_text().as_deref(), Some("00\n01\n02"));
        let spans = c.spans();
        assert_eq!(spans.len(), 3);
        assert!(spans.iter().all(|s| (s.start, s.end) == (8, 10)));
        // Toggled off, the same ends make a plain selection.
        keys(&mut c, &["C-v"]);
        assert_eq!(c.spans()[0].end, 40, "middle rows run to the edge");
        assert_eq!(
            keys(&mut c, &["Enter"]),
            Outcome::Copy("00 alpha-beta gamma\nhistory 01 alpha-beta gamma\nhistory 02".into())
        );
    }

    #[test]
    fn vi_search_goes_both_ways_wraps_and_repeats() {
        let mut c = mode(Table::Vi);
        keys(&mut c, &["?", "h", "e", "l", "l", "o"]);
        assert_eq!(c.prompt().as_deref(), Some("(search up) hello"));
        keys(&mut c, &["Enter"]);
        assert_eq!((at(&c), c.prompt()), ((21, 0), None));
        keys(&mut c, &["n"]);
        assert_eq!(at(&c), (20, 7));
        keys(&mut c, &["N"]);
        assert_eq!(at(&c), (21, 0));
        // Down from the bottom wraps to the top of history.
        keys(&mut c, &["G", "/", "a", "l", "p", "h", "a", "Enter"]);
        assert_eq!((at(&c), c.top()), ((0, 11), 0));
        // Capitals make case matter.
        keys(&mut c, &["/", "H", "E", "L", "L", "O", "Enter"]);
        assert_eq!(c.message(), Some("Search failed"));
        assert_eq!(at(&c), (0, 11));
        // Escape abandons a prompt; a second one leaves.
        keys(&mut c, &["/", "x", "Escape"]);
        assert_eq!(c.prompt(), None);
        assert_eq!(keys(&mut c, &["Escape"]), Outcome::Leave);
    }

    #[test]
    fn emacs_keys_move_select_and_copy_without_the_cursor_cell() {
        let mut c = mode(Table::Emacs);
        keys(&mut c, &["C-p", "C-p", "C-p", "C-a"]);
        assert_eq!(at(&c), (21, 0));
        keys(&mut c, &["M-f"]);
        assert_eq!(at(&c), (21, 5), "past the word");
        keys(&mut c, &["M-f", "M-b"]);
        assert_eq!(at(&c), (21, 6));
        keys(&mut c, &["C-e"]);
        assert_eq!(at(&c), (21, 11), "just past the text");
        keys(
            &mut c,
            &["C-a", "C-Space", "C-f", "C-f", "C-f", "C-f", "C-f"],
        );
        assert_eq!(keys(&mut c, &["M-w"]), Outcome::Copy("hello".into()));

        let mut c = mode(Table::Emacs);
        keys(&mut c, &["M-<"]);
        assert_eq!(at(&c), (0, 0));
        keys(&mut c, &["M->"]);
        assert_eq!(at(&c), (24, 2));
        keys(&mut c, &["C-r", "w", "o", "r", "l", "d", "Enter"]);
        assert_eq!(at(&c), (21, 6));
        keys(&mut c, &["C-r", "Enter"]);
        assert_eq!(at(&c), (20, 13), "an empty search repeats the last one");
        keys(&mut c, &["C-s", "g", "a", "m", "Enter"]);
        assert_eq!(at(&c), (0, 22), "down wraps to the top");
        // A rectangle: R toggles it.
        keys(&mut c, &["C-Space", "C-n", "C-f", "C-f", "C-f", "R"]);
        assert_eq!(c.selection_text().as_deref(), Some("gam\ngam"));
        // C-g drops the selection; copying nothing leaves.
        keys(&mut c, &["C-g"]);
        assert!(!c.selecting());
        assert_eq!(keys(&mut c, &["M-w"]), Outcome::Leave);
    }

    #[test]
    fn new_output_keeps_the_cursor_and_selection() {
        let mut c = mode(Table::Vi);
        keys(&mut c, &["k", "0", "v", "e"]);
        let more = format!("{}\nnew line\n$ ", text());
        c.set_text(&more, 27);
        assert_eq!(c.selection_text().as_deref(), Some("  indented"));
        assert_eq!(c.indicator(), "[2/22]");
        keys(&mut c, &["G"]);
        assert_eq!(c.cursor().row, 26);
        assert_eq!(c.table(), Table::Vi);
        assert_eq!(Table::for_mode_keys("vi\n"), Table::Vi);
        assert_eq!(Table::for_mode_keys("emacs"), Table::Emacs);
    }
}
