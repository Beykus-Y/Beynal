// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{render::Geometry, terminal::Screen};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

pub fn position(geometry: Geometry, x: f64, y: f64, screen: &Screen) -> Position {
    let (col, row) = geometry.cell_at(x, y);
    Position {
        row: screen.display_row_or_visible(row.min(screen.rows.saturating_sub(1))),
        col: col.min(screen.columns.saturating_sub(1)),
    }
}

/// Logical-pixel pointer movement threshold before a single click becomes an
/// intentional drag selection (HiDPI-independent because pointer coordinates
/// are passed in surface-local logical pixels).
pub const DRAG_THRESHOLD_LOGICAL: f64 = 4.0;

#[derive(Default)]
pub struct Selection {
    anchor: Option<Position>,
    end: Option<Position>,
    dragging: bool,
    /// True only when the user has demonstrated selection intent (dragged past
    /// threshold or across cells, double-clicked a word, triple-clicked a line,
    /// or created a keyboard selection). A plain single click leaves this false.
    committed: bool,
    press_origin: Option<(f64, f64)>,
    last_click: Option<(u32, f64, f64)>,
    clicks: u8,
    epoch: u64,
    initial: Option<(Position, Position)>,
}
impl Selection {
    pub fn press(&mut self, screen: &Screen, p: Position, time: u32, x: f64, y: f64) {
        if self.epoch != screen.selection_epoch {
            self.clear();
        }
        self.epoch = screen.selection_epoch;
        self.clicks = match self.last_click {
            Some((last, lx, ly))
                if time.wrapping_sub(last) <= 400
                    && (x - lx).abs() <= 5.0
                    && (y - ly).abs() <= 5.0 =>
            {
                self.clicks % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((time, x, y));
        self.press_origin = Some((x, y));
        self.dragging = true;
        let (a, b) = self.bounds_at(screen, p);
        self.initial = Some((a, b));
        self.anchor = Some(a);
        self.end = Some(b);
        // Double-click (word) and triple-click (line) immediately express
        // selection intent; a single click does NOT commit until the pointer
        // drags past the threshold or enters another cell.
        self.committed = self.clicks >= 2;
    }

    /// Programmatically create an intentional selection (e.g. keyboard selection).
    #[allow(dead_code)]
    pub fn select_range(&mut self, screen: &Screen, a: Position, b: Position) {
        self.epoch = screen.selection_epoch;
        self.dragging = false;
        self.press_origin = None;
        self.clicks = 0;
        self.initial = Some((a, b));
        self.anchor = Some(a);
        self.end = Some(b);
        self.committed = true;
    }

    fn bounds_at(&self, screen: &Screen, p: Position) -> (Position, Position) {
        match self.clicks {
            2 => word(screen, p),
            3 => {
                let mut first = p.row;
                let mut last = p.row;
                while first > screen.oldest_row() && screen.row_wrapped(first - 1) {
                    first -= 1;
                }
                while last + 1 < screen.newest_row() && screen.row_wrapped(last) {
                    last += 1;
                }
                (
                    Position { row: first, col: 0 },
                    Position {
                        row: last,
                        col: screen
                            .absolute_row(last)
                            .map_or(0, |row| row.len().saturating_sub(1)),
                    },
                )
            }
            _ => (p, p),
        }
    }

    #[allow(dead_code)]
    pub fn motion(&mut self, screen: &Screen, p: Position) -> bool {
        self.motion_at(screen, p, None)
    }

    /// Update an active drag with optional surface-local logical pointer
    /// coordinates so dragging across a single character (even within one cell)
    /// commits an intentional single-character selection once the pointer moves
    /// at least `DRAG_THRESHOLD_LOGICAL`.
    pub fn motion_at(
        &mut self,
        screen: &Screen,
        p: Position,
        logical_pointer: Option<(f64, f64)>,
    ) -> bool {
        if !self.dragging || self.raw_range(screen).is_none() {
            return false;
        }
        let Some((first, last)) = self.initial else {
            return false;
        };
        let (start, end) = self.bounds_at(screen, p);
        let next = if start < first {
            (last, start)
        } else {
            (first, last.max(end))
        };
        let cell_changed = (self.anchor, self.end) != (Some(next.0), Some(next.1));
        let moved_past_threshold = match (self.press_origin, logical_pointer) {
            (Some((ox, oy)), Some((lx, ly))) => {
                let dx = lx - ox;
                let dy = ly - oy;
                (dx * dx + dy * dy).sqrt() >= DRAG_THRESHOLD_LOGICAL
            }
            _ => false,
        };
        let was_committed = self.committed;
        if cell_changed || moved_past_threshold {
            self.committed = true;
        }
        (self.anchor, self.end) = (Some(next.0), Some(next.1));
        cell_changed || (self.committed != was_committed)
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }
    pub fn release(&mut self) {
        self.dragging = false;
        self.press_origin = None;
        if !self.committed {
            self.anchor = None;
            self.end = None;
            self.initial = None;
        }
    }
    pub fn clear(&mut self) {
        self.anchor = None;
        self.end = None;
        self.dragging = false;
        self.committed = false;
        self.press_origin = None;
        self.last_click = None;
        self.clicks = 0;
        self.initial = None;
    }
    fn raw_range(&self, screen: &Screen) -> Option<(Position, Position)> {
        let (a, b) = (self.anchor?, self.end?);
        let (start, end) = (a.min(b), a.max(b));
        (self.epoch == screen.selection_epoch
            && start.row >= screen.oldest_row()
            && end.row < screen.newest_row())
        .then_some((start, end))
    }
    fn range(&self, screen: &Screen) -> Option<(Position, Position)> {
        if !self.committed {
            return None;
        }
        self.raw_range(screen)
    }
    pub fn active(&self, screen: &Screen) -> bool {
        self.range(screen).is_some()
    }
    /// True only when there is an intentional, valid selection containing at
    /// least one copyable character (including written spaces, wide glyphs, or
    /// combining clusters, but excluding unwritten empty cells).
    pub fn has_copyable_text(&self, screen: &Screen) -> bool {
        self.text(screen).is_some_and(|t| !t.is_empty())
    }
    /// True while a mouse button is down and the drag anchor is still valid for
    /// the current screen epoch/history.
    pub fn drag_valid(&self, screen: &Screen) -> bool {
        self.dragging && self.raw_range(screen).is_some()
    }
    pub fn text(&self, screen: &Screen) -> Option<String> {
        let (start, end) = self.range(screen)?;
        let mut result = String::new();
        for row in start.row..=end.row {
            let cells = screen.absolute_row(row)?;
            let mut first = if row == start.row { start.col } else { 0 };
            let last = if row == end.row {
                end.col.saturating_add(1).min(cells.len())
            } else {
                cells.len()
            };
            if cells.get(first).is_some_and(|cell| cell.continuation) {
                first = first.saturating_sub(1);
            }
            let mut last_written = result.len();
            for cell in cells.get(first..last).unwrap_or_default() {
                if !cell.continuation {
                    result.push(cell.character);
                    result.extend(&cell.combining);
                    if cell.written {
                        last_written = result.len();
                    }
                }
            }
            result.truncate(last_written);
            if row < end.row && !screen.row_wrapped(row) {
                result.push('\n');
            }
        }
        Some(result)
    }

    pub fn contains(&self, screen: &Screen, row: usize, col: usize) -> bool {
        match screen.display_row(row) {
            Some(row) => self.contains_absolute(screen, row, col),
            None => false,
        }
    }
    fn contains_absolute(&self, screen: &Screen, row: usize, col: usize) -> bool {
        let Some((start, end)) = self.range(screen) else {
            return false;
        };
        if row < start.row || row > end.row {
            return false;
        }
        let mut p = Position { row, col };
        if screen
            .get_absolute(col, row)
            .is_some_and(|c| c.continuation)
        {
            p.col = p.col.saturating_sub(1);
        }
        let last = if screen.get_absolute(p.col, row).is_some_and(|c| c.wide) {
            Position {
                row,
                col: p.col + 1,
            }
        } else {
            p
        };
        p <= end && last >= start
    }
}
fn word(screen: &Screen, mut p: Position) -> (Position, Position) {
    if screen
        .get_absolute(p.col, p.row)
        .is_some_and(|c| c.continuation)
    {
        p.col = p.col.saturating_sub(1);
    }
    let class = |c: char| {
        if c.is_whitespace() {
            0
        } else if c.is_alphanumeric() || c == '_' {
            1
        } else {
            2
        }
    };
    let Some(cells) = screen
        .absolute_row(p.row)
        .filter(|cells| p.col < cells.len())
    else {
        return (p, p);
    };
    let kind = class(cells[p.col].character);
    let mut start = p.col;
    let mut end = p.col;
    while start > 0 {
        let c = &cells[start - 1];
        if !c.continuation && class(c.character) != kind {
            break;
        }
        start -= 1;
    }
    while end + 1 < cells.len() {
        let c = &cells[end + 1];
        if !c.continuation && class(c.character) != kind {
            break;
        }
        end += 1;
    }
    (
        Position {
            row: p.row,
            col: start,
        },
        Position {
            row: p.row,
            col: end,
        },
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;
    #[test]
    fn bottom_input_display_rows_map_to_absolute_selection_rows() {
        let theme = crate::theme::Theme {
            layout: crate::terminal::LayoutMode::BottomInput,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(6, 3, theme.clone());
        t.process(b"abc");
        assert_eq!(t.screen().display_row(2), Some(0));
        let g = crate::render::Renderer::with_theme(theme)
            .unwrap()
            .geometry(1.0);
        assert_eq!(
            position(
                g,
                (g.padding + g.cell_width) as f64,
                (g.padding + 2 * g.cell_height) as f64,
                t.screen()
            ),
            Position { row: 0, col: 1 }
        );
        let mut s = Selection::default();
        s.press(t.screen(), Position { row: 0, col: 0 }, 0, 0., 0.);
        s.motion(t.screen(), Position { row: 0, col: 2 });
        s.release();
        assert_eq!(s.text(t.screen()).as_deref(), Some("abc"));
        assert!(s.contains(t.screen(), 2, 1));
        assert!(!s.contains(t.screen(), 0, 1));
    }

    #[test]
    fn reverse_selection_survives_output_and_expires_with_history() {
        let theme = crate::theme::Theme {
            scrollback_limit: 5,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(6, 2, theme);
        t.process("a界bc\r\ndé\r\nff\r\ngg".as_bytes());
        let mut s = Selection::default();
        s.press(t.screen(), Position { row: 3, col: 1 }, 0, 0., 0.);
        t.screen_mut().scroll_view(isize::MAX);
        s.motion(t.screen(), Position { row: 0, col: 2 });
        s.release();
        assert_eq!(s.text(t.screen()).as_deref(), Some("界bc\ndé\nff\ngg"));
        t.process(b"\r\nhh");
        assert_eq!(s.text(t.screen()).as_deref(), Some("界bc\ndé\nff\ngg"));
        t.process(&b"\r\n".repeat(5));
        assert_eq!(s.text(t.screen()), None);
        assert!(!s.contains(t.screen(), 0, 0));
    }

    #[test]
    fn selections_do_not_cross_the_alternate_screen() {
        let mut t = Terminal::new(8, 2);
        t.process(b"main");
        let mut s = Selection::default();
        s.press(t.screen(), Position { row: 0, col: 0 }, 0, 0., 0.);
        t.process(b"\x1b[?1049halt\x1b[?1049l");
        assert_eq!(s.text(t.screen()), None);
    }

    #[test]
    fn triple_click_selects_soft_wrapped_logical_line() {
        let mut t = Terminal::new(4, 2);
        t.process(b"abcdefghij");
        t.screen_mut().scroll_view(isize::MAX);
        let mut s = Selection::default();
        let p = Position { row: 1, col: 1 };
        for time in [100, 200, 300] {
            s.press(t.screen(), p, time, 1., 1.);
            s.release();
        }
        assert_eq!(s.text(t.screen()).as_deref(), Some("abcdefghij"));
    }

    #[test]
    fn copies_soft_wrapped_history_and_preserves_spaces() {
        let mut t = Terminal::new(4, 2);
        t.process("ab  cd\r\nконец".as_bytes());
        t.screen_mut().scroll_view(isize::MAX);
        let start = t.screen().visible_start();
        let mut s = Selection::default();
        s.press(t.screen(), Position { row: start, col: 0 }, 0, 0., 0.);
        s.motion(
            t.screen(),
            Position {
                row: start + 1,
                col: 3,
            },
        );
        assert_eq!(s.text(t.screen()).unwrap(), "ab  cd");
    }
    #[test]
    fn extracts_reverse_unicode_and_trimmed_lines() {
        let mut t = Terminal::new(8, 3);
        t.process("аб界 x\r\n hi  yo".as_bytes());
        let mut s = Selection::default();
        assert_eq!(s.text(t.screen()), None);
        s.press(t.screen(), Position { row: 1, col: 7 }, 1, 0.0, 0.0);
        s.motion(t.screen(), Position { row: 0, col: 0 });
        assert_eq!(s.text(t.screen()).unwrap(), "аб界 x\n hi  yo");
    }
    #[test]
    fn coordinates_and_reverse_multiline() {
        let t = Terminal::new(5, 3);
        let g = crate::render::Renderer::new().unwrap().geometry(1.0);
        assert_eq!(
            position(
                g,
                (g.padding + g.cell_width) as f64,
                (g.padding + g.cell_height) as f64,
                t.screen()
            ),
            Position { row: 1, col: 1 }
        );
        assert_eq!(
            position(g, -4.0, 1000.0, t.screen()),
            Position { row: 2, col: 0 }
        );
        let mut s = Selection::default();
        s.press(t.screen(), Position { row: 2, col: 1 }, 0, 0., 0.);
        s.motion(t.screen(), Position { row: 0, col: 3 });
        assert!(s.contains(t.screen(), 1, 0));
        assert!(s.contains(t.screen(), 0, 4));
        assert!(!s.contains(t.screen(), 0, 2));
    }
    #[test]
    fn clicks_and_wide() {
        let mut t = Terminal::new(10, 2);
        t.process("hi界 ok".as_bytes());
        let mut s = Selection::default();
        let p = Position { row: 0, col: 3 };
        // Single click without drag does NOT create a copyable selection.
        s.press(t.screen(), p, 100, 1., 1.);
        s.release();
        assert!(!s.active(t.screen()));
        assert!(!s.has_copyable_text(t.screen()));
        // Double click selects the word (including wide glyph).
        s.press(t.screen(), p, 200, 1., 1.);
        assert!(s.contains(t.screen(), 0, 0));
        assert!(s.contains(t.screen(), 0, 2));
        assert!(s.contains(t.screen(), 0, 3));
        assert!(!s.contains(t.screen(), 0, 4));
        assert_eq!(s.text(t.screen()).as_deref(), Some("hi界"));
        s.release();
        // Triple click selects the full logical line.
        s.press(t.screen(), p, 300, 1., 1.);
        assert!(s.contains(t.screen(), 0, 9));
        assert!(!s.contains(t.screen(), 1, 0));
        s.release();
    }

    #[test]
    fn single_click_empty_cell_never_activates_selection_but_intentional_single_char_drag_does() {
        let mut t = Terminal::new(10, 3);
        t.process("a界e\u{0301}  z".as_bytes());
        let mut s = Selection::default();

        // Single click on empty cell (row 1, col 4) must not activate selection.
        s.press(t.screen(), Position { row: 1, col: 4 }, 100, 50.0, 40.0);
        s.release();
        assert!(!s.active(t.screen()));
        assert!(!s.has_copyable_text(t.screen()));

        // Dragging across empty unwritten cells produces empty text -> not copyable.
        s.press(t.screen(), Position { row: 2, col: 0 }, 200, 10.0, 60.0);
        s.motion_at(t.screen(), Position { row: 2, col: 5 }, Some((60.0, 60.0)));
        s.release();
        assert!(!s.has_copyable_text(t.screen()));

        // Intentional drag past threshold within a single character ('a') commits and copies 'a'.
        s.press(t.screen(), Position { row: 0, col: 0 }, 300, 10.0, 10.0);
        s.motion_at(
            t.screen(),
            Position { row: 0, col: 0 },
            Some((10.0 + DRAG_THRESHOLD_LOGICAL + 1.0, 10.0)),
        );
        s.release();
        assert!(s.has_copyable_text(t.screen()));
        assert_eq!(s.text(t.screen()).as_deref(), Some("a"));

        // Intentional drag on combining cluster 'é' copies the full grapheme.
        s.press(t.screen(), Position { row: 0, col: 3 }, 400, 30.0, 10.0);
        s.motion_at(t.screen(), Position { row: 0, col: 3 }, Some((36.0, 10.0)));
        s.release();
        assert_eq!(s.text(t.screen()).as_deref(), Some("e\u{0301}"));

        // Intentional drag on written spaces preserves spaces.
        s.press(t.screen(), Position { row: 0, col: 4 }, 500, 40.0, 10.0);
        s.motion_at(t.screen(), Position { row: 0, col: 5 }, Some((55.0, 10.0)));
        s.release();
        assert!(s.has_copyable_text(t.screen()));
        assert_eq!(s.text(t.screen()).as_deref(), Some("  "));

        // Programmatic / keyboard range selection commits immediately.
        s.select_range(
            t.screen(),
            Position { row: 0, col: 0 },
            Position { row: 0, col: 2 },
        );
        assert!(s.has_copyable_text(t.screen()));
        assert_eq!(s.text(t.screen()).as_deref(), Some("a界"));
    }
}
