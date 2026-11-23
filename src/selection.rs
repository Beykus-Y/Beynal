// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
    let col = ((x - geometry.padding as f64) / geometry.cell_width as f64)
        .floor()
        .max(0.0) as usize;
    let row = ((y - geometry.padding as f64) / geometry.cell_height as f64)
        .floor()
        .max(0.0) as usize;
    Position {
        row: screen.visible_start() + row.min(screen.rows.saturating_sub(1)),
        col: col.min(screen.columns.saturating_sub(1)),
    }
}

#[derive(Default)]
pub struct Selection {
    anchor: Option<Position>,
    end: Option<Position>,
    dragging: bool,
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
        self.dragging = true;
        let (a, b) = self.bounds_at(screen, p);
        self.initial = Some((a, b));
        self.anchor = Some(a);
        self.end = Some(b);
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
    pub fn motion(&mut self, screen: &Screen, p: Position) -> bool {
        if !self.dragging || self.range(screen).is_none() {
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
        let changed = (self.anchor, self.end) != (Some(next.0), Some(next.1));
        (self.anchor, self.end) = (Some(next.0), Some(next.1));
        changed
    }
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }
    pub fn release(&mut self) {
        self.dragging = false;
    }
    pub fn clear(&mut self) {
        self.anchor = None;
        self.end = None;
        self.dragging = false;
        self.last_click = None;
        self.clicks = 0;
        self.initial = None;
    }
    fn range(&self, screen: &Screen) -> Option<(Position, Position)> {
        let (a, b) = (self.anchor?, self.end?);
        let (start, end) = (a.min(b), a.max(b));
        (self.epoch == screen.selection_epoch
            && start.row >= screen.oldest_row()
            && end.row < screen.newest_row())
        .then_some((start, end))
    }
    pub fn active(&self, screen: &Screen) -> bool {
        self.range(screen).is_some()
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
        self.contains_absolute(screen, screen.visible_start() + row, col)
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
        let g = Geometry::from_theme(&crate::theme::Theme::default());
        assert_eq!(
            position(g, 22.0, 30.0, t.screen()),
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
        s.press(t.screen(), p, 100, 1., 1.);
        s.release();
        assert!(s.contains(t.screen(), 0, 2));
        assert!(s.contains(t.screen(), 0, 3));
        s.press(t.screen(), p, 200, 1., 1.);
        assert!(s.contains(t.screen(), 0, 0));
        assert!(!s.contains(t.screen(), 0, 4));
        s.release();
        s.press(t.screen(), p, 300, 1., 1.);
        assert!(s.contains(t.screen(), 0, 9));
        assert!(!s.contains(t.screen(), 1, 0));
    }
}
