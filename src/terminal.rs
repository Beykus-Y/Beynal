// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::Cell as LayoutCache;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

/// Vertical placement of the terminal grid inside the window.
///
/// The enum only affects presentation. PTY coordinates, the cell grid, the
/// scrollback and the alternate screen are never rewritten.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutMode {
    /// Standard terminal: the shell starts at the top-left cell.
    #[default]
    AsIs,
    /// The shell starts on the last row. After that the terminal behaves exactly
    /// like a standard terminal, including any cursor movement the application
    /// performs.
    Bottom,
    /// The lowest written row (normally the prompt/input line) is kept on the
    /// last row by shifting the whole visible grid down. Disabled on the
    /// alternate screen and while browsing scrollback.
    BottomInput,
}

impl LayoutMode {
    pub const ALL: [Self; 3] = [Self::AsIs, Self::Bottom, Self::BottomInput];

    pub fn name(self) -> &'static str {
        match self {
            Self::AsIs => "as-is",
            Self::Bottom => "bottom",
            Self::BottomInput => "bottom-input",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorOrigin {
    DefaultForeground,
    DefaultBackground,
    Indexed(u8),
    Rgb,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub character: char,
    pub combining: Vec<char>,
    pub wide: bool,
    pub continuation: bool,
    pub written: bool,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub fg_origin: ColorOrigin,
    pub bg_origin: ColorOrigin,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            combining: Vec::new(),
            wide: false,
            continuation: false,
            written: false,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            fg_origin: ColorOrigin::DefaultForeground,
            bg_origin: ColorOrigin::DefaultBackground,
        }
    }
}

pub const DEFAULT_FG: [u8; 3] = [232, 232, 232];
pub const DEFAULT_BG: [u8; 3] = [18, 24, 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: crate::theme::CursorShape,
    pub blink: bool,
}

struct SavedScreen {
    cells: Vec<Cell>,
    cursor: (usize, usize),
    saved_cursor: (usize, usize),
    wraps: Vec<bool>,
    margins: (usize, usize),
    style: Option<CursorStyle>,
    visible: bool,
    rendition: ([u8; 3], ColorOrigin, [u8; 3], ColorOrigin, bool),
}

pub fn resolve_color(color: [u8; 3], origin: ColorOrigin, theme: &crate::theme::Theme) -> [u8; 3] {
    match origin {
        ColorOrigin::DefaultForeground => theme.foreground,
        ColorOrigin::DefaultBackground => theme.background,
        ColorOrigin::Indexed(i) if i < 16 => theme.ansi[i as usize],
        ColorOrigin::Indexed(i) => palette(i),
        ColorOrigin::Rgb => color,
    }
}

pub struct Terminal {
    parser: Parser,
    screen: Screen,
}

pub struct Screen {
    pub columns: usize,
    pub rows: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub cells: Vec<Cell>,
    pub cursor_visible: bool,
    pub cursor_style: Option<CursorStyle>,
    pub revision: u64,
    pub bracketed_paste: bool,
    mouse: crate::mouse::Mode,
    mouse_sgr: bool,
    layout: LayoutMode,
    fg: [u8; 3],
    bg: [u8; 3],
    fg_origin: ColorOrigin,
    bg_origin: ColorOrigin,
    inverse: bool,
    saved: (usize, usize),
    alternate: Option<SavedScreen>,
    scroll_top: usize,
    scroll_bottom: usize,
    responses: Vec<u8>,
    theme: crate::theme::Theme,
    history: VecDeque<(Vec<Cell>, bool)>,
    history_base: usize,
    history_bytes: usize,
    pub selection_epoch: u64,
    viewport_offset: usize,
    wraps: Vec<bool>,
    layout_cache: LayoutCache<Option<(u64, Option<usize>)>>,
}

impl Screen {
    #[cfg(test)]
    pub fn new(columns: usize, rows: usize) -> Self {
        Self::with_theme(columns, rows, crate::theme::Theme::default())
    }
    pub fn with_theme(columns: usize, rows: usize, theme: crate::theme::Theme) -> Self {
        let blank = Cell {
            fg: theme.foreground,
            bg: theme.background,
            ..Cell::default()
        };
        let layout = theme.layout;
        let mut screen = Self {
            columns,
            rows,
            cursor_x: 0,
            cursor_y: 0,
            cells: vec![blank; columns * rows],
            cursor_visible: true,
            cursor_style: None,
            revision: 0,
            bracketed_paste: false,
            mouse: crate::mouse::Mode::Off,
            mouse_sgr: false,
            layout,
            fg: theme.foreground,
            bg: theme.background,
            fg_origin: ColorOrigin::DefaultForeground,
            bg_origin: ColorOrigin::DefaultBackground,
            inverse: false,
            saved: (0, 0),
            alternate: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            responses: Vec::new(),
            theme,
            history: VecDeque::new(),
            history_base: 0,
            history_bytes: 0,
            selection_epoch: 0,
            viewport_offset: 0,
            wraps: vec![false; rows],
            layout_cache: LayoutCache::new(None),
        };
        // The shell inherits the real cursor position, so the first prompt is
        // printed on the last row without touching its output coordinates.
        if layout == LayoutMode::Bottom {
            screen.cursor_y = rows.saturating_sub(1);
        }
        screen
    }

    pub fn mouse_mode(&self) -> crate::mouse::Mode {
        self.mouse
    }

    pub fn mouse_sgr(&self) -> bool {
        self.mouse_sgr
    }

    pub fn set_layout(&mut self, layout: LayoutMode) {
        if self.layout != layout {
            self.layout = layout;
            self.revision += 1;
        }
    }

    /// Number of blank display rows inserted above the grid.
    pub fn layout_offset(&self) -> usize {
        if self.layout != LayoutMode::BottomInput
            || self.alternate.is_some()
            || self.viewport_offset > 0
        {
            0
        } else {
            // Anchor the lowest written row (normally the input line) to the
            // bottom. Including the cursor keeps an empty or freshly cleared
            // screen bottom aligned without hiding content written below it.
            let bottom = self.lowest_written_row().unwrap_or(0).max(self.cursor_y);
            self.rows.saturating_sub(1).saturating_sub(bottom)
        }
    }

    fn lowest_written_row(&self) -> Option<usize> {
        if let Some((revision, row)) = self.layout_cache.get()
            && revision == self.revision
        {
            return row;
        }
        let row = (0..self.rows).rev().find(|&y| {
            self.cells[y * self.columns..(y + 1) * self.columns]
                .iter()
                .any(|cell| cell.written)
        });
        self.layout_cache.set(Some((self.revision, row)));
        row
    }

    /// Display row of the cursor; equals `cursor_y` outside `bottom-input`.
    pub fn cursor_display_row(&self) -> usize {
        (self.cursor_y + self.layout_offset()).min(self.rows.saturating_sub(1))
    }

    fn display_start(&self) -> isize {
        self.visible_start() as isize - self.layout_offset() as isize
    }

    /// Absolute row shown at display row `y`, or `None` for padding above the
    /// oldest retained line.
    pub fn display_row(&self, y: usize) -> Option<usize> {
        if y >= self.rows {
            return None;
        }
        let row = self.display_start().checked_add(y as isize)?;
        (row >= self.history_base as isize && row < self.newest_row() as isize)
            .then_some(row as usize)
    }

    /// Absolute row for a click, clamped to the live content when the pointer is
    /// over the blank top padding.
    pub fn display_row_or_visible(&self, y: usize) -> usize {
        self.display_row(y).unwrap_or_else(|| self.visible_start())
    }

    pub fn get(&self, x: usize, y: usize) -> Option<&Cell> {
        if x >= self.columns || y >= self.rows {
            return None;
        }

        self.get_absolute(x, self.display_row(y)?)
            .filter(|cell| !cell.wide || x + 1 < self.columns)
    }
    pub fn visible_start(&self) -> usize {
        self.history_base
            + self
                .history
                .len()
                .saturating_sub(if self.alternate.is_some() {
                    0
                } else {
                    self.viewport_offset
                })
    }
    pub fn oldest_row(&self) -> usize {
        self.history_base
            + if self.alternate.is_some() {
                self.history.len()
            } else {
                0
            }
    }
    pub fn newest_row(&self) -> usize {
        self.history_base + self.history.len() + self.rows
    }
    pub fn absolute_row(&self, row: usize) -> Option<&[Cell]> {
        let index = row.checked_sub(self.history_base)?;
        if self.alternate.is_none() && index < self.history.len() {
            return Some(&self.history.get(index)?.0);
        }
        let y = index.checked_sub(self.history.len())?;
        if y >= self.rows {
            return None;
        }
        self.cells.get(y * self.columns..(y + 1) * self.columns)
    }
    pub fn get_absolute(&self, x: usize, row: usize) -> Option<&Cell> {
        self.absolute_row(row)?.get(x)
    }
    pub fn row_wrapped(&self, row: usize) -> bool {
        let Some(index) = row.checked_sub(self.history_base) else {
            return false;
        };
        if self.alternate.is_none() && index < self.history.len() {
            self.history.get(index).is_some_and(|r| r.1)
        } else {
            index
                .checked_sub(self.history.len())
                .and_then(|y| self.wraps.get(y))
                .copied()
                .unwrap_or(false)
        }
    }
    pub fn scroll_view(&mut self, delta: isize) -> bool {
        if self.alternate.is_some() {
            return false;
        }
        let previous = self.viewport_offset;
        self.viewport_offset = self
            .viewport_offset
            .saturating_add_signed(delta)
            .min(self.history.len());
        self.viewport_offset != previous
    }
    pub fn view_offset(&self) -> usize {
        if self.alternate.is_some() {
            0
        } else {
            self.viewport_offset
        }
    }
    pub fn scroll_speed(&self) -> usize {
        self.theme.scroll_speed
    }
    pub fn reset_view(&mut self) {
        if self.alternate.is_none() {
            self.viewport_offset = 0;
        }
    }

    fn apply_theme(&mut self, theme: crate::theme::Theme) {
        let change = |color: &mut [u8; 3], origin| {
            *color = resolve_color(*color, origin, &theme);
        };
        for cell in &mut self.cells {
            change(&mut cell.fg, cell.fg_origin);
            change(&mut cell.bg, cell.bg_origin);
        }
        if let Some(saved) = &mut self.alternate {
            for cell in &mut saved.cells {
                change(&mut cell.fg, cell.fg_origin);
                change(&mut cell.bg, cell.bg_origin);
            }
            let (fg, fg_origin, bg, bg_origin, _) = &mut saved.rendition;
            change(fg, *fg_origin);
            change(bg, *bg_origin);
        }
        change(&mut self.fg, self.fg_origin);
        change(&mut self.bg, self.bg_origin);
        self.set_layout(theme.layout);
        self.theme = theme;
        self.trim_history();
    }

    fn row_bytes(line: &[Cell]) -> usize {
        std::mem::size_of_val(line)
            + line
                .iter()
                .map(|cell| cell.combining.capacity() * size_of::<char>())
                .sum::<usize>()
    }

    fn trim_history(&mut self) {
        const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
        while self.history.len() > self.theme.scrollback_limit
            || self.history_bytes > MAX_HISTORY_BYTES
        {
            let (line, _) = self.history.pop_front().expect("history is nonempty");
            self.history_bytes -= Self::row_bytes(&line);
            self.history_base += 1;
        }
        self.viewport_offset = self.viewport_offset.min(self.history.len());
    }

    fn save_top_rows(&mut self, count: usize) {
        if self.theme.scrollback_limit == 0 {
            self.history_base += count;
            return;
        }
        let (cells, wraps) = self
            .alternate
            .as_ref()
            .map_or((&self.cells, &self.wraps), |saved| {
                (&saved.cells, &saved.wraps)
            });
        for (cells, wrapped) in cells.chunks_exact(self.columns).zip(wraps).take(count) {
            let line = cells.to_vec();
            self.history_bytes += Self::row_bytes(&line);
            self.history.push_back((line, *wrapped));
        }
        if self.viewport_offset > 0 {
            self.viewport_offset += count;
        }
        self.trim_history();
    }
    fn resize(&mut self, columns: usize, rows: usize) {
        fn resize_cells(
            old: &[Cell],
            old_columns: usize,
            old_rows: usize,
            columns: usize,
            rows: usize,
            blank: &Cell,
        ) -> Vec<Cell> {
            let mut result = vec![blank.clone(); columns * rows];
            for y in 0..old_rows.min(rows) {
                for x in 0..old_columns.min(columns) {
                    let cell = &old[y * old_columns + x];
                    if cell.wide && x + 1 >= columns {
                        continue;
                    }
                    result[y * columns + x] = cell.clone();
                }
            }
            result
        }
        let (columns, rows) = (columns.max(1), rows.max(1));
        let (old_columns, old_rows) = (self.columns, self.rows);
        if (old_columns, old_rows) == (columns, rows) {
            return;
        }
        let blank = self.blank();
        let shift = self.cursor_y.saturating_sub(rows - 1);
        let main_shift = self
            .alternate
            .as_ref()
            .map_or(shift, |saved| saved.cursor.1.saturating_sub(rows - 1));
        self.save_top_rows(main_shift);
        self.cells = resize_cells(
            &self.cells[shift * old_columns..],
            old_columns,
            old_rows - shift,
            columns,
            rows,
            &blank,
        );
        self.wraps.drain(..shift);
        self.cursor_y -= shift;
        self.saved.1 = self.saved.1.saturating_sub(shift);
        self.selection_epoch += 1;
        self.revision += 1;
        if let Some(saved) = &mut self.alternate {
            let blank = Cell {
                fg: self.theme.foreground,
                bg: self.theme.background,
                ..Cell::default()
            };
            saved.cells = resize_cells(
                &saved.cells[main_shift * old_columns..],
                old_columns,
                old_rows - main_shift,
                columns,
                rows,
                &blank,
            );
            saved.wraps.drain(..main_shift);
            if old_columns != columns {
                saved.cursor.0 = saved.cursor.0.min(columns - 1);
            }
            saved.cursor.1 -= main_shift;
            saved.saved_cursor.1 = saved.saved_cursor.1.saturating_sub(main_shift);
            saved.saved_cursor.0 = saved.saved_cursor.0.min(columns - 1);
            saved.saved_cursor.1 = saved.saved_cursor.1.min(rows - 1);
            saved.wraps.resize(rows, false);
            saved.margins = (0, rows - 1);
        }
        self.wraps.resize(rows, false);
        self.columns = columns;
        self.rows = rows;
        if old_columns != columns {
            self.cursor_x = self.cursor_x.min(columns - 1);
        }
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.saved = (self.saved.0.min(columns - 1), self.saved.1.min(rows - 1));
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
    }

    fn put_char(&mut self, character: char) {
        if self.columns == 0 || self.rows == 0 || character.is_control() {
            return;
        }

        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            if self.cursor_x > 0 {
                let mut x = self.cursor_x.min(self.columns) - 1;
                if self.cells[self.cursor_y * self.columns + x].continuation && x > 0 {
                    x -= 1;
                }
                let marks = &mut self.cells[self.cursor_y * self.columns + x].combining;
                if marks.len() < 32 {
                    marks.push(character);
                    self.revision += 1;
                }
            }
            return;
        }
        if width > self.columns {
            return;
        }
        if self.cursor_x + width > self.columns {
            self.wraps[self.cursor_y] = true;
            self.cursor_x = 0;
            self.line_feed();
        }
        let index = self.cursor_y * self.columns + self.cursor_x;
        self.clear_cell(index);
        if width == 2 {
            self.clear_cell(index + 1);
        }
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        let (fg_origin, bg_origin) = if self.inverse {
            (self.bg_origin, self.fg_origin)
        } else {
            (self.fg_origin, self.bg_origin)
        };
        self.cells[index] = Cell {
            character,
            written: true,
            wide: width == 2,
            fg,
            bg,
            fg_origin,
            bg_origin,
            ..Cell::default()
        };
        if width == 2 {
            self.cells[index + 1] = Cell {
                continuation: true,
                written: true,
                fg,
                bg,
                fg_origin,
                bg_origin,
                ..Cell::default()
            };
        }
        self.cursor_x += width;
        self.revision += 1;
    }

    fn clear_cell(&mut self, index: usize) {
        if self.cells[index].continuation && !index.is_multiple_of(self.columns) {
            self.cells[index - 1] = self.blank();
        }
        if self.cells[index].wide && index % self.columns + 1 < self.columns {
            self.cells[index + 1] = self.blank();
        }
        self.cells[index] = self.blank();
    }

    fn line_feed(&mut self) {
        if self.rows == 0 {
            return;
        }

        if self.cursor_y == self.scroll_bottom {
            self.scroll_region(true, 1);
        } else {
            self.cursor_y = (self.cursor_y + 1).min(self.rows - 1);
        }
    }

    fn blank(&self) -> Cell {
        if self.inverse {
            Cell {
                fg: self.bg,
                bg: self.fg,
                fg_origin: self.bg_origin,
                bg_origin: self.fg_origin,
                ..Cell::default()
            }
        } else {
            Cell {
                fg: self.fg,
                bg: self.bg,
                fg_origin: self.fg_origin,
                bg_origin: self.bg_origin,
                ..Cell::default()
            }
        }
    }
    /// Shift grid rows by `n` cells inside `start..end`, then blank the freed
    /// rows. `Cell` owns a `Vec`, so this cannot use `copy_within`; the duplicated
    /// source row is forgotten with `ptr::write` (no drop) after its ownership has
    /// moved, while the row leaving the grid is dropped with `fill`.
    fn shift_cells(&mut self, start: usize, end: usize, n: usize, up: bool, blank: &Cell) {
        let total = end.saturating_sub(start);
        if n == 0 || n > total {
            return;
        }
        if n == total {
            self.cells[start..end].fill(blank.clone());
            return;
        }
        let moved = total - n;
        unsafe {
            let ptr = self.cells.as_mut_ptr();
            if up {
                // Drop the row that scrolls out before overwriting it.
                self.cells[start..start + n].fill(blank.clone());
                std::ptr::copy(ptr.add(start + n), ptr.add(start), moved);
                for index in 0..n {
                    std::ptr::write(ptr.add(end - n + index), blank.clone());
                }
            } else {
                self.cells[end - n..end].fill(blank.clone());
                std::ptr::copy(ptr.add(start), ptr.add(start + n), moved);
                for index in 0..n {
                    std::ptr::write(ptr.add(start + index), blank.clone());
                }
            }
        }
    }

    fn scroll_region(&mut self, up: bool, count: usize) {
        if self.rows == 0 || self.columns == 0 {
            return;
        }
        self.revision += 1;
        let start = self.scroll_top * self.columns;
        let end = (self.scroll_bottom + 1) * self.columns;
        let n = count.min(self.scroll_bottom - self.scroll_top + 1) * self.columns;
        let blank = self.blank();
        if up {
            if self.alternate.is_none()
                && self.scroll_top == 0
                && self.scroll_bottom + 1 == self.rows
            {
                self.save_top_rows(n / self.columns);
            } else {
                self.selection_epoch += 1;
            }
            self.wraps[self.scroll_top..=self.scroll_bottom].rotate_left(n / self.columns);
            for row in (self.scroll_bottom + 1 - n / self.columns)..=self.scroll_bottom {
                self.wraps[row] = false;
            }
            // `rotate_left` walks the grid several times. Scrolling is the hot
            // path for line-oriented output, so shift rows with one memmove and
            // explicitly forget the duplicated row at the tail.
            self.shift_cells(start, end, n, true, &blank);
        } else {
            self.selection_epoch += 1;
            self.wraps[self.scroll_top..=self.scroll_bottom].rotate_right(n / self.columns);
            for row in self.scroll_top..self.scroll_top + n / self.columns {
                self.wraps[row] = false;
            }
            self.shift_cells(start, end, n, false, &blank);
        }
    }
    fn erase(&mut self, start: usize, end: usize) {
        let end = end.min(self.cells.len());
        for index in start.min(end)..end {
            self.clear_cell(index);
        }
        if start < end {
            self.revision += 1;
            self.wraps[start / self.columns..end / self.columns].fill(false);
        }
    }
    fn color(&self, index: u8) -> [u8; 3] {
        if index < 16 {
            self.theme.ansi[index as usize]
        } else {
            palette(index)
        }
    }
    fn sgr(&mut self, params: &Params) {
        let mut values = params.iter();
        while let Some(param) = values.next() {
            let p = param[0];
            match p {
                0 => {
                    self.fg = self.theme.foreground;
                    self.bg = self.theme.background;
                    self.fg_origin = ColorOrigin::DefaultForeground;
                    self.bg_origin = ColorOrigin::DefaultBackground;
                    self.inverse = false;
                }
                7 => self.inverse = true,
                27 => self.inverse = false,
                30..=37 => {
                    let index = (p - 30) as u8;
                    self.fg = self.color(index);
                    self.fg_origin = ColorOrigin::Indexed(index);
                }
                90..=97 => {
                    let index = (p - 90 + 8) as u8;
                    self.fg = self.color(index);
                    self.fg_origin = ColorOrigin::Indexed(index);
                }
                40..=47 => {
                    let index = (p - 40) as u8;
                    self.bg = self.color(index);
                    self.bg_origin = ColorOrigin::Indexed(index);
                }
                100..=107 => {
                    let index = (p - 100 + 8) as u8;
                    self.bg = self.color(index);
                    self.bg_origin = ColorOrigin::Indexed(index);
                }
                39 => {
                    self.fg = self.theme.foreground;
                    self.fg_origin = ColorOrigin::DefaultForeground;
                }
                49 => {
                    self.bg = self.theme.background;
                    self.bg_origin = ColorOrigin::DefaultBackground;
                }
                38 | 48 => {
                    let mut separate = [0; 4];
                    let args = match &param[1..] {
                        [] => {
                            let mode = values.next().map_or(0, |param| param[0]);
                            separate[0] = mode;
                            let count = match mode {
                                2 => 3,
                                5 => 1,
                                _ => 0,
                            };
                            let mut len = 1;
                            for item in values.by_ref().take(count) {
                                separate[len] = item[0];
                                len += 1;
                            }
                            &separate[..len]
                        }
                        args => args,
                    };
                    let (color, origin) = match args {
                        [5, n] if *n <= 255 => {
                            (Some(self.color(*n as u8)), ColorOrigin::Indexed(*n as u8))
                        }
                        [2, r, g, b] | [2, 0, r, g, b] if *r <= 255 && *g <= 255 && *b <= 255 => {
                            (Some([*r as u8, *g as u8, *b as u8]), ColorOrigin::Rgb)
                        }
                        _ => (None, ColorOrigin::Rgb),
                    };
                    if let Some(color) = color {
                        if p == 38 {
                            self.fg = color;
                            self.fg_origin = origin;
                        } else {
                            self.bg = color;
                            self.bg_origin = origin;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    #[cfg(test)]
    pub fn line(&self, y: usize) -> String {
        if y >= self.rows {
            return String::new();
        }

        (0..self.columns)
            .filter_map(|x| self.get(x, y))
            .map(|cell| cell.character)
            .collect()
    }
}

fn palette(index: u8) -> [u8; 3] {
    if index < 16 {
        return crate::theme::ANSI[index as usize];
    }
    if index < 232 {
        let n = index - 16;
        let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        return [level(n / 36), level((n / 6) % 6), level(n % 6)];
    }
    let v = 8 + (index - 232) * 10;
    [v, v, v]
}

impl Perform for Screen {
    fn print(&mut self, character: char) {
        self.put_char(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => {
                self.cursor_x = self.cursor_x.min(self.columns - 1);
                self.line_feed();
            }
            b'\r' => self.cursor_x = 0,
            0x08 => {
                self.cursor_x = self.cursor_x.saturating_sub(1);
            }
            b'\t' => {
                let next = (self.cursor_x / 8 + 1) * 8;
                self.cursor_x = next.min(self.columns.saturating_sub(1));
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if intermediates == b"?$" && action == 'p' {
            for mode in params.iter().filter_map(|p| p.first()) {
                let status = match mode {
                    25 => {
                        if self.cursor_visible {
                            1
                        } else {
                            2
                        }
                    }
                    1049 => {
                        if self.alternate.is_some() {
                            1
                        } else {
                            2
                        }
                    }
                    1000 => {
                        if self.mouse == crate::mouse::Mode::Click {
                            1
                        } else {
                            2
                        }
                    }
                    1002 => {
                        if self.mouse == crate::mouse::Mode::Drag {
                            1
                        } else {
                            2
                        }
                    }
                    1003 => {
                        if self.mouse == crate::mouse::Mode::Motion {
                            1
                        } else {
                            2
                        }
                    }
                    1006 => {
                        if self.mouse_sgr {
                            1
                        } else {
                            2
                        }
                    }
                    2004 => {
                        if self.bracketed_paste {
                            1
                        } else {
                            2
                        }
                    }
                    _ => 0,
                };
                self.responses
                    .extend_from_slice(format!("\x1b[?{mode};{status}$y").as_bytes());
            }
            return;
        }
        if intermediates == b"?" {
            for param in params.iter().filter_map(|p| p.first()) {
                match (*param, action) {
                    (2004, 'h') => self.bracketed_paste = true,
                    (2004, 'l') => self.bracketed_paste = false,
                    (25, 'h') => self.cursor_visible = true,
                    (25, 'l') => self.cursor_visible = false,
                    (1000 | 1002 | 1003, 'h') => {
                        if let Some(mode) = crate::mouse::Mode::from_decimal((*param).into()) {
                            self.mouse = mode;
                        }
                    }
                    (1000 | 1002 | 1003, 'l') => self.mouse = crate::mouse::Mode::Off,
                    (1006, 'h') => self.mouse_sgr = true,
                    (1006, 'l') => self.mouse_sgr = false,
                    (47 | 1047 | 1049, 'h') if self.alternate.is_none() => {
                        self.selection_epoch += 1;
                        self.revision += 1;
                        let blank = self.blank();
                        self.alternate = Some(SavedScreen {
                            cells: std::mem::replace(
                                &mut self.cells,
                                vec![blank; self.columns * self.rows],
                            ),
                            cursor: (self.cursor_x, self.cursor_y),
                            saved_cursor: std::mem::take(&mut self.saved),
                            wraps: std::mem::replace(&mut self.wraps, vec![false; self.rows]),
                            margins: (self.scroll_top, self.scroll_bottom),
                            style: self.cursor_style,
                            visible: self.cursor_visible,
                            rendition: (
                                self.fg,
                                self.fg_origin,
                                self.bg,
                                self.bg_origin,
                                self.inverse,
                            ),
                        });
                        self.cursor_x = 0;
                        self.cursor_y = 0;
                        self.scroll_top = 0;
                        self.scroll_bottom = self.rows.saturating_sub(1);
                    }
                    (47 | 1047 | 1049, 'l') => {
                        if let Some(saved) = self.alternate.take() {
                            self.selection_epoch += 1;
                            self.revision += 1;
                            self.wraps = saved.wraps;
                            self.cells = saved.cells;
                            (self.cursor_x, self.cursor_y) = saved.cursor;
                            self.saved = saved.saved_cursor;
                            (self.scroll_top, self.scroll_bottom) = saved.margins;
                            self.cursor_style = saved.style;
                            self.cursor_visible = saved.visible;
                            (
                                self.fg,
                                self.fg_origin,
                                self.bg,
                                self.bg_origin,
                                self.inverse,
                            ) = saved.rendition;
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        let first = params
            .iter()
            .next()
            .and_then(|p| p.first())
            .copied()
            .unwrap_or(0) as usize;

        if intermediates == b" " && action == 'q' {
            use crate::theme::CursorShape::*;
            self.cursor_style = match first {
                0 => None,
                1..=6 => Some(CursorStyle {
                    shape: match first {
                        1 | 2 => Block,
                        3 | 4 => Underline,
                        _ => Beam,
                    },
                    blink: first % 2 == 1,
                }),
                _ => self.cursor_style,
            };
            return;
        }
        if !intermediates.is_empty() {
            return;
        }

        match action {
            'A' => {
                self.cursor_y = self.cursor_y.saturating_sub(first.max(1));
            }
            'B' => {
                self.cursor_y = (self.cursor_y + first.max(1)).min(self.rows.saturating_sub(1));
            }
            'C' => {
                self.cursor_x = (self.cursor_x + first.max(1)).min(self.columns.saturating_sub(1));
            }
            'D' => {
                self.cursor_x = self
                    .cursor_x
                    .min(self.columns.saturating_sub(1))
                    .saturating_sub(first.max(1));
            }
            'H' | 'f' => {
                let mut values = params.iter();

                let row = values
                    .next()
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(1)
                    .max(1) as usize;

                let col = values
                    .next()
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(1)
                    .max(1) as usize;

                self.cursor_y = (row - 1).min(self.rows.saturating_sub(1));
                self.cursor_x = (col - 1).min(self.columns.saturating_sub(1));
            }
            'n' if first == 5 => self.responses.extend_from_slice(b"\x1b[0n"),
            'n' if first == 6 => self.responses.extend_from_slice(
                format!(
                    "\x1b[{};{}R",
                    self.cursor_y + 1,
                    self.cursor_x.min(self.columns.saturating_sub(1)) + 1
                )
                .as_bytes(),
            ),
            'c' if first == 0 => self.responses.extend_from_slice(b"\x1b[?1;2c"),
            'm' => self.sgr(params),
            's' => self.saved = (self.cursor_x, self.cursor_y),
            'u' => {
                (self.cursor_x, self.cursor_y) = self.saved;
            }
            'G' => {
                self.cursor_x = first
                    .max(1)
                    .saturating_sub(1)
                    .min(self.columns.saturating_sub(1))
            }
            'd' => {
                self.cursor_y = first
                    .max(1)
                    .saturating_sub(1)
                    .min(self.rows.saturating_sub(1))
            }
            'E' => {
                self.cursor_y = (self.cursor_y + first.max(1)).min(self.rows.saturating_sub(1));
                self.cursor_x = 0;
            }
            'F' => {
                self.cursor_y = self.cursor_y.saturating_sub(first.max(1));
                self.cursor_x = 0;
            }
            'J' => {
                let pos = self.cursor_y * self.columns
                    + self.cursor_x.min(self.columns.saturating_sub(1));
                match first {
                    0 => self.erase(pos, self.cells.len()),
                    1 => self.erase(0, pos + 1),
                    2 => self.erase(0, self.cells.len()),
                    3 if self.alternate.is_none() => {
                        self.history_base += self.history.len();
                        self.history.clear();
                        self.history_bytes = 0;
                        self.viewport_offset = 0;
                    }
                    _ => {}
                }
            }
            'K' => {
                let start = self.cursor_y * self.columns;
                let pos = start + self.cursor_x.min(self.columns.saturating_sub(1));
                match first {
                    0 => self.erase(pos, start + self.columns),
                    1 => self.erase(start, pos + 1),
                    2 => self.erase(start, start + self.columns),
                    _ => {}
                }
            }
            '@' | 'P' | 'X' if self.rows > 0 && self.columns > 0 => {
                let x = self.cursor_x.min(self.columns - 1);
                let start = self.cursor_y * self.columns + x;
                let end = (self.cursor_y + 1) * self.columns;
                let n = first.max(1).min(end - start);
                // Clear complete wide glyphs at boundaries before moving cells.
                if self.cells[start].continuation {
                    self.clear_cell(start);
                }
                if action == '@' {
                    self.cells[start..end].rotate_right(n);
                    self.erase(start, start + n);
                } else if action == 'P' {
                    self.cells[start..end].rotate_left(n);
                    self.erase(end - n, end);
                } else {
                    self.erase(start, start + n);
                }
                for col in 0..self.columns {
                    let i = self.cursor_y * self.columns + col;
                    if (self.cells[i].continuation && (col == 0 || !self.cells[i - 1].wide))
                        || (self.cells[i].wide
                            && (col + 1 == self.columns || !self.cells[i + 1].continuation))
                    {
                        self.cells[i] = self.blank();
                    }
                }
            }
            'S' => self.scroll_region(true, first.max(1)),
            'T' => self.scroll_region(false, first.max(1)),
            'L' | 'M'
                if self.cursor_y >= self.scroll_top && self.cursor_y <= self.scroll_bottom =>
            {
                let top = self.scroll_top;
                self.scroll_top = self.cursor_y;
                self.scroll_region(action == 'M', first.max(1));
                self.scroll_top = top;
            }
            'r' => {
                let bottom = params
                    .iter()
                    .nth(1)
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(self.rows as u16) as usize;
                let top = first.max(1);
                if top < bottom && bottom <= self.rows {
                    self.scroll_top = top - 1;
                    self.scroll_bottom = bottom - 1;
                    self.cursor_x = 0;
                    self.cursor_y = 0;
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}

    fn put(&mut self, _: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let (mut slot, values) = match params {
            [b"10", values @ ..] => (10, values),
            [b"11", values @ ..] => (11, values),
            _ => return,
        };
        for value in values {
            if slot > 11 {
                break;
            }
            if *value == b"?" {
                let [r, g, b] = if slot == 10 {
                    self.theme.foreground
                } else {
                    self.theme.background
                };
                let (r, g, b) = (u16::from(r) * 257, u16::from(g) * 257, u16::from(b) * 257);
                let end = if bell_terminated { "\x07" } else { "\x1b\\" };
                self.responses.extend_from_slice(
                    format!("\x1b]{slot};rgb:{r:04x}/{g:04x}/{b:04x}{end}").as_bytes(),
                );
            }
            slot += 1;
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.saved = (self.cursor_x, self.cursor_y),
            b'8' => (self.cursor_x, self.cursor_y) = self.saved,
            b'D' => self.line_feed(),
            b'M' => {
                if self.cursor_y == self.scroll_top {
                    self.scroll_region(false, 1);
                } else {
                    self.cursor_y = self.cursor_y.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

impl Terminal {
    #[cfg(test)]
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::new(columns, rows),
        }
    }

    pub fn with_theme(columns: usize, rows: usize, theme: crate::theme::Theme) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::with_theme(columns, rows, theme),
        }
    }

    pub fn apply_theme(&mut self, theme: crate::theme::Theme) {
        self.screen.apply_theme(theme);
    }

    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.screen.resize(columns, rows);
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.screen.responses)
    }

    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responds_to_tui_queries_with_current_theme_colors() {
        let mut t = Terminal::new(5, 1);
        t.apply_theme(crate::theme::Theme {
            foreground: [1, 2, 3],
            background: [4, 5, 6],
            ..Default::default()
        });
        t.process(b"\x1b]10;?;?\x1b\\\x1b]11;?\x07");
        assert_eq!(t.take_responses(), b"\x1b]10;rgb:0101/0202/0303\x1b\\\x1b]11;rgb:0404/0505/0606\x1b\\\x1b]11;rgb:0404/0505/0606\x07");
        assert_eq!(t.screen().line(0), "     ");
    }

    #[test]
    fn parses_colon_rgb_with_optional_color_space_and_indexed_colors() {
        let mut t = Terminal::new(5, 1);
        t.process(b"\x1b[38:2::10:20:30;48:5:12mA\x1b[38:2:40:50:60mB");
        assert_eq!(t.screen().get(0, 0).unwrap().fg, [10, 20, 30]);
        assert_eq!(
            t.screen().get(0, 0).unwrap().bg_origin,
            ColorOrigin::Indexed(12)
        );
        assert_eq!(t.screen().get(1, 0).unwrap().fg, [40, 50, 60]);
    }

    #[test]
    fn resizing_alternate_preserves_main_cursor_line_and_viewport() {
        let mut t = Terminal::new(8, 3);
        t.process(b"oldest\r\nolder\r\ncurrent\r\ncursor");
        t.screen_mut().scroll_view(1);
        let anchor = t.screen().visible_start();
        t.process(b"\x1b[?1049hALT");
        t.resize(8, 2);
        t.process(b"\x1b[?1049l");
        assert_eq!(t.screen().visible_start(), anchor);
        t.screen_mut().reset_view();
        assert!(t.screen().line(1).starts_with("cursor"));
    }

    #[test]
    fn alternate_restores_cursor_mode_and_does_not_save_its_output() {
        use crate::theme::CursorShape;
        let mut t = Terminal::new(8, 2);
        t.process(b"main\r\nhere\x1b[6 q");
        t.process(b"\x1b[?1049h\x1b[1 q\x1b[?25l");
        t.process(&b"ALT\r\n".repeat(100));
        assert!(t.screen().history.is_empty());
        t.process(b"\x1b[?1049l");
        assert!(t.screen().cursor_visible);
        assert_eq!(
            t.screen().cursor_style,
            Some(CursorStyle {
                shape: CursorShape::Beam,
                blink: false
            })
        );
        assert!(t.screen().line(0).starts_with("main"));
    }

    #[test]
    fn erase_scrollback_keeps_live_cells_and_reverse_video_uses_inverted_colors() {
        let mut t = Terminal::new(8, 2);
        t.process(b"gone\r\nmain\r\nhere\x1b[7m\x1b[K");
        assert_eq!(
            t.screen().get(7, 1).unwrap().bg_origin,
            ColorOrigin::DefaultForeground
        );
        assert!(t.screen_mut().scroll_view(1));
        t.process(b"\x1b[3J");
        assert!(t.screen().history.is_empty());
        assert_eq!(t.screen().view_offset(), 0);
        assert!(t.screen().line(0).starts_with("main"));
    }

    #[test]
    fn long_output_keeps_view_anchored_until_eviction() {
        let mut t = Terminal::new(40, 24);
        let now = std::time::Instant::now();
        for n in 0..50_000 {
            t.process(format!("row {n:05} — история\r\n").as_bytes());
        }
        let processing = now.elapsed();
        assert_eq!(t.screen().history.len(), 10_000);
        let now = std::time::Instant::now();
        for _ in 0..100_000 {
            std::hint::black_box(t.screen_mut()).scroll_view(1);
            std::hint::black_box(t.screen_mut()).scroll_view(-1);
        }
        eprintln!(
            "50k rows: {processing:?}; 200k viewport steps: {:?}; history payload: {} bytes",
            now.elapsed(),
            t.screen().history_bytes
        );
        assert!(t.screen_mut().scroll_view(9990));
        let anchor = t.screen().visible_start();
        let line = t.screen().line(0);
        t.process(b"more\r\n");
        assert_eq!(t.screen().visible_start(), anchor);
        assert_eq!(t.screen().line(0), line);
        t.process(&b"more\r\n".repeat(15));
        assert_eq!(t.screen().visible_start(), t.screen().oldest_row());
        assert!(t.screen_mut().scroll_view(isize::MIN));
        assert_eq!(t.screen().view_offset(), 0);
    }

    #[test]
    fn history_memory_limit_accounts_for_wide_rows() {
        let mut t = Terminal::new(2048, 2);
        t.process(&b"\r\n".repeat(1024));
        assert!(t.screen().history_bytes <= 64 * 1024 * 1024);
        assert!(t.screen().oldest_row() > 0);
    }

    #[test]
    fn resizing_preserves_history_and_the_cursor_line() {
        let mut t = Terminal::new(8, 3);
        t.process("abc界xyz\r\nsecond\r\nthird\r\ncursor".as_bytes());
        t.screen_mut().scroll_view(1);
        let first = t.screen().visible_start();
        t.resize(4, 2);
        assert_eq!(t.screen().visible_start(), first);
        assert!(t.screen().get(3, 0).is_none());
        assert_eq!(t.screen().absolute_row(first).unwrap()[7].character, 'z');
        t.screen_mut().reset_view();
        assert_eq!(t.screen().line(1), "curs");
        assert_eq!(t.screen().cursor_y, 1);
    }

    #[test]
    fn history_limit_scroll_and_alternate() {
        let theme = crate::theme::Theme {
            scrollback_limit: 3,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(8, 2, theme);
        t.process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert_eq!(t.screen().history.len(), 3);
        t.screen_mut().scroll_view(2);
        assert_eq!(t.screen().get(0, 0).unwrap().character, 't');
        t.process(b"\r\nsix");
        assert_eq!(t.screen().get(0, 0).unwrap().character, 't');
        t.process(b"\x1b[?1049hALT");
        t.screen_mut().reset_view();
        assert_eq!(t.screen().get(0, 0).unwrap().character, 'A');
        t.process(b"\x1b[?1049l");
        assert_eq!(t.screen().get(0, 0).unwrap().character, 't');
        t.screen_mut().reset_view();
        assert_eq!(t.screen().view_offset(), 0);
    }
    #[test]
    fn truecolor_equal_to_palette_is_not_recolored() {
        let mut t = Terminal::new(8, 1);
        let [r, g, b] = crate::theme::ANSI[1];
        t.process(format!("\x1b[38;2;{r};{g};{b}mA\x1b[31mB").as_bytes());
        let mut theme = crate::theme::Theme::default();
        theme.ansi[1] = [1, 2, 3];
        t.apply_theme(theme);
        assert_eq!(t.screen().get(0, 0).unwrap().fg, crate::theme::ANSI[1]);
        assert_eq!(t.screen().get(1, 0).unwrap().fg, [1, 2, 3]);
    }
    #[test]
    fn theme_update_preserves_grid_and_recolors_defaults() {
        let mut t = Terminal::new(10, 2);
        t.process(b"hello");
        let theme = crate::theme::Theme::builtin("signature").unwrap();
        t.apply_theme(theme.clone());
        t.resize(8, 2);
        assert!(t.screen().line(0).starts_with("hello"));
        assert_eq!(t.screen().get(0, 0).unwrap().fg, theme.foreground);
        assert_eq!(t.screen().get(0, 0).unwrap().bg, theme.background);
    }
    #[test]
    fn nano_reverse_video_and_character_edits() {
        let mut t = Terminal::new(8, 2);
        t.process(b"\x1b[7mA\x1b[27mBCDE");
        assert_eq!(t.screen().get(0, 0).unwrap().fg, DEFAULT_BG);
        assert_eq!(t.screen().get(0, 0).unwrap().bg, DEFAULT_FG);
        t.process(b"\x1b[1;2H\x1b[2@xy");
        assert!(t.screen().line(0).starts_with("AxyBCDE"));
        t.process(b"\x1b[1;2H\x1b[2P");
        assert!(t.screen().line(0).starts_with("ABCDE"));
    }
    #[test]
    fn line_end_cursor_and_bracketed_mode() {
        let mut t = Terminal::new(4, 3);
        t.process(b"abcd\x1b[D!");
        assert_eq!(t.screen().line(0), "ab!d");
        t.process(b"\r\nX\nY\rZ");
        assert_eq!(t.screen().get(0, 1).unwrap().character, 'X');
        assert_eq!(t.screen().get(0, 2).unwrap().character, 'Z');
        t.process(b"\x1b[?2004h");
        assert!(t.screen().bracketed_paste);
        t.process(b"\x1b[?2004l");
        assert!(!t.screen().bracketed_paste);
    }
    #[test]
    fn tracks_mouse_modes_and_reports_them_through_decrqm() {
        use crate::mouse::Mode;
        let mut t = Terminal::new(10, 2);
        assert_eq!(t.screen().mouse_mode(), Mode::Off);
        assert!(!t.screen().mouse_sgr());

        t.process(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(t.screen().mouse_mode(), Mode::Drag);
        assert!(t.screen().mouse_sgr());
        t.process(b"\x1b[?1000$p\x1b[?1002$p\x1b[?1006$p\x1b[?2004$p");
        assert_eq!(
            t.take_responses(),
            b"\x1b[?1000;2$y\x1b[?1002;1$y\x1b[?1006;1$y\x1b[?2004;2$y"
        );

        // Focus reporting is not implemented, so its query must stay
        // unrecognized instead of being advertised as supported.
        t.process(b"\x1b[?2004h");
        assert_eq!(t.screen().mouse_mode(), Mode::Drag);
        t.process(b"\x1b[?1004$p\x1b[?2004$p");
        assert_eq!(t.take_responses(), b"\x1b[?1004;0$y\x1b[?2004;1$y");

        t.process(b"\x1b[?1002l\x1b[?1006l");
        assert_eq!(t.screen().mouse_mode(), Mode::Off);
        assert!(!t.screen().mouse_sgr());
    }

    #[test]
    fn alternate_screen_keeps_mouse_reporting_mode() {
        use crate::mouse::Mode;
        let mut t = Terminal::new(10, 3);
        t.process(b"\x1b[?1003h\x1b[?1006h\x1b[?1049h");
        assert_eq!(t.screen().mouse_mode(), Mode::Motion);
        assert!(t.screen().mouse_sgr());
        t.process(b"\x1b[?1049l");
        assert_eq!(t.screen().mouse_mode(), Mode::Motion);
    }

    #[test]
    fn resize_preserves_both_screens_and_clamps_cursor() {
        let mut t = Terminal::new(6, 3);
        t.process("ab界".as_bytes());
        t.process(b"\x1b[?1049hxyz");
        t.resize(3, 2);
        assert!(t.screen().line(0).starts_with("xyz"));
        assert!(t.screen().cursor_x < 3);
        t.process(b"\x1b[?1049l");
        assert_eq!(t.screen().line(0), "ab ");
        assert_eq!((t.screen().cursor_x, t.screen().cursor_y), (2, 0));
        t.resize(8, 4);
        assert!(t.screen().line(0).starts_with("ab"));
        assert_eq!(t.screen().cells.len(), 32);
    }
    #[test]
    fn reports_terminal_state_without_displaying_responses() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[3;9H\x1b[6n\x1b[5n\x1b[c\x1b[?25$p\x1b[?9999$p");
        assert_eq!(
            t.take_responses(),
            b"\x1b[3;9R\x1b[0n\x1b[?1;2c\x1b[?25;1$y\x1b[?9999;0$y"
        );
        assert!(t.take_responses().is_empty());
        assert!(t.screen().cells.iter().all(|c| c.character == ' '));
    }
    #[test]
    fn alternate_screen_and_cursor_restore() {
        let mut t = Terminal::new(10, 3);
        t.process(b"main\x1b[?1049hother\x1b[?25l");
        assert!(t.screen().line(0).starts_with("other"));
        assert!(!t.screen().cursor_visible);
        t.process(b"\x1b[?25h\x1b[?1049l!");
        assert!(t.screen().line(0).starts_with("main!"));
        assert!(t.screen().cursor_visible);
    }
    #[test]
    fn configured_palette_and_defaults_survive_sgr_reset() {
        let mut theme = crate::theme::Theme {
            foreground: [4, 5, 6],
            background: [7, 8, 9],
            ..Default::default()
        };
        theme.ansi[1] = [11, 22, 33];
        let mut t = Terminal::with_theme(4, 1, theme);
        t.process(b"\x1b[31mA\x1b[0mB");
        assert_eq!(t.screen().get(0, 0).unwrap().fg, [11, 22, 33]);
        assert_eq!(t.screen().get(1, 0).unwrap().fg, [4, 5, 6]);
        assert_eq!(t.screen().get(1, 0).unwrap().bg, [7, 8, 9]);
    }
    #[test]
    fn sgr_colors_and_reset() {
        let mut t = Terminal::new(10, 1);
        t.process(b"\x1b[31;104mA\x1b[38;5;196;48;2;1;2;3mB\x1b[0mC");
        assert_eq!(t.screen().get(0, 0).unwrap().fg, palette(1));
        assert_eq!(t.screen().get(0, 0).unwrap().bg, palette(12));
        assert_eq!(t.screen().get(1, 0).unwrap().fg, [255, 0, 0]);
        assert_eq!(t.screen().get(1, 0).unwrap().bg, [1, 2, 3]);
        assert_eq!(t.screen().get(2, 0).unwrap().fg, DEFAULT_FG);
        assert_eq!(palette(232), [8, 8, 8]);
    }
    #[test]
    fn erase_cursor_and_lines() {
        let mut t = Terminal::new(5, 3);
        t.process(b"abcde\r\n12345\r\nxyz\x1b[2;3H\x1b[s\x1b[2J\x1b[u!");
        assert_eq!(t.screen().cursor_y, 1);
        assert_eq!(t.screen().get(2, 1).unwrap().character, '!');
        assert_eq!(t.screen().get(0, 0).unwrap().character, ' ');
        t.process(b"\x1b[1;1Hone\x1b[2;1Htwo\x1b[3;1Htri\x1b[2;1H\x1b[L");
        assert!(t.screen().line(2).starts_with("two"));
        t.process(b"\x1b[M");
        assert!(t.screen().line(1).starts_with("two"));
        t.process(b"\x1b[2;2H\x1b[1K");
        assert!(t.screen().line(1).starts_with("  o"));
    }
    #[test]
    fn scrolling_combining_rows_keeps_ownership_correct() {
        let mut t = Terminal::new(6, 3);
        for _ in 0..2000 {
            t.process("e\u{301}\r\n".as_bytes());
        }
        // Reaching here without a double free / corrupted grid is the point;
        // also verify the last line is intact and history is populated.
        assert!(t.screen().visible_start() > 0);
        for _ in 0..2000 {
            t.process("x\r\n".as_bytes());
        }
        assert!(
            (0..t.screen().rows).any(|row| t.screen().line(row).trim_end() == "x"),
            "the last written row must survive scrolling"
        );
    }

    #[test]
    fn bottom_layout_starts_on_last_row_then_uses_standard_scrolling() {
        let theme = crate::theme::Theme {
            layout: LayoutMode::Bottom,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(12, 3, theme);
        assert_eq!(t.screen().cursor_y, 2);
        assert_eq!(t.screen().layout_offset(), 0);
        t.process(b"$ first");
        assert_eq!(t.screen().line(2).trim_end(), "$ first");
        t.process(b"\r\nsecond\r\nthird");
        assert_eq!(t.screen().line(0).trim_end(), "$ first");
        assert_eq!(t.screen().line(1).trim_end(), "second");
        assert_eq!(t.screen().line(2).trim_end(), "third");
        assert_eq!(t.screen().cursor_y, 2);
        t.process(b"\x1b[6n");
        assert_eq!(t.take_responses(), b"\x1b[3;6R");
    }

    #[test]
    fn bottom_input_pins_the_input_line_without_faking_coordinates() {
        let theme = crate::theme::Theme {
            layout: LayoutMode::BottomInput,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(10, 4, theme);
        assert_eq!(t.screen().cursor_y, 0, "real cursor must stay at the top");
        assert_eq!(t.screen().layout_offset(), 3);
        assert_eq!(t.screen().cursor_display_row(), 3);
        t.process(b"$ one");
        assert_eq!(t.screen().line(3).trim_end(), "$ one");
        t.process(b"\r\ntwo\r\nthree");
        assert_eq!(t.screen().layout_offset(), 1);
        assert_eq!(t.screen().line(1).trim_end(), "$ one");
        assert_eq!(t.screen().line(2).trim_end(), "two");
        assert_eq!(t.screen().line(3).trim_end(), "three");
        assert_eq!(t.screen().cursor_display_row(), 3);
        // Moving the real cursor up must not hide text below it.
        t.process(b"\x1b[2;1H");
        assert_eq!(t.screen().cursor_display_row(), 2);
        assert_eq!(t.screen().line(3).trim_end(), "three");
        t.process(b"\x1b[6n");
        assert_eq!(t.take_responses(), b"\x1b[2;1R");
    }

    #[test]
    fn bottom_input_maps_history_alternate_screen_and_resize() {
        let theme = crate::theme::Theme {
            layout: LayoutMode::BottomInput,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(10, 3, theme);
        t.process(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(t.screen().line(0).trim_end(), "two");
        assert_eq!(t.screen().line(2).trim_end(), "four");
        assert!(t.screen_mut().scroll_view(1));
        assert_eq!(t.screen().layout_offset(), 0);
        assert_eq!(t.screen().line(0).trim_end(), "one");
        t.screen_mut().reset_view();
        t.process(b"\x1b[?1049hALT");
        assert_eq!(t.screen().layout_offset(), 0);
        assert_eq!(t.screen().cursor_display_row(), 0);
        assert_eq!(t.screen().line(0).trim_end(), "ALT");
        t.process(b"\x1b[?1049l");
        t.process(b"\x1b[?25l");
        t.resize(8, 2);
        assert_eq!(t.screen().line(1).trim_end(), "four");
        assert_eq!(t.screen().cursor_display_row(), 1);
    }

    #[test]
    fn bottom_input_does_not_clip_content_below_the_cursor() {
        let theme = crate::theme::Theme {
            layout: LayoutMode::BottomInput,
            ..Default::default()
        };
        let mut t = Terminal::with_theme(10, 3, theme);
        t.process(b"top\r\nbottom");
        assert_eq!(t.screen().layout_offset(), 1);
        t.process(b"\x1b[1;1H!");
        assert_eq!(t.screen().layout_offset(), 1);
        assert_eq!(t.screen().line(1).trim_end(), "!op");
        assert_eq!(t.screen().line(2).trim_end(), "bottom");
        assert_eq!(t.screen().cursor_display_row(), 1);
    }

    #[test]
    fn layout_mode_names_round_trip() {
        for mode in LayoutMode::ALL {
            assert_eq!(LayoutMode::parse(mode.name()), Some(mode));
        }
        assert_eq!(LayoutMode::parse("normal"), None);
    }

    #[test]
    fn switching_to_bottom_input_only_changes_presentation() {
        let mut t = Terminal::new(10, 3);
        t.process(b"abc");
        assert_eq!(t.screen().layout_offset(), 0);
        t.screen_mut().set_layout(LayoutMode::BottomInput);
        assert_eq!(t.screen().cursor_y, 0);
        assert_eq!(t.screen().layout_offset(), 2);
        assert_eq!(t.screen().cursor_display_row(), 2);
        assert_eq!(t.screen().line(2).trim_end(), "abc");
        t.screen_mut().set_layout(LayoutMode::AsIs);
        assert_eq!(t.screen().line(0).trim_end(), "abc");
    }

    #[test]
    fn writes_text() {
        let mut terminal = Terminal::new(20, 5);

        terminal.process(b"Hello Beyrix");

        assert!(terminal.screen().line(0).starts_with("Hello Beyrix"));
    }

    #[test]
    fn handles_newlines() {
        let mut terminal = Terminal::new(20, 5);

        terminal.process(b"Hello\r\nWorld");

        assert!(terminal.screen().line(0).starts_with("Hello"));
        assert!(terminal.screen().line(1).starts_with("World"));
    }

    #[test]
    fn handles_cursor_movement() {
        let mut terminal = Terminal::new(20, 5);

        terminal.process(b"Hello\x1b[1D!");

        assert!(terminal.screen().line(0).starts_with("Hell!"));
    }

    #[test]
    fn handles_scrolling() {
        let mut terminal = Terminal::new(10, 2);

        terminal.process(b"Line1\r\nLine2\r\nLine3");

        assert!(terminal.screen().line(0).starts_with("Line2"));
        assert!(terminal.screen().line(1).starts_with("Line3"));
    }

    #[test]
    fn overwrite_wide_continuation_clears_both_cells() {
        let mut terminal = Terminal::new(5, 1);
        terminal.process("界".as_bytes());
        terminal.process(b"\x1b[1D!");
        assert_eq!(terminal.screen().get(0, 0).unwrap().character, ' ');
        assert_eq!(terminal.screen().get(1, 0).unwrap().character, '!');
        assert!(!terminal.screen().get(1, 0).unwrap().continuation);
    }

    #[test]
    fn wide_and_combining_characters() {
        let mut terminal = Terminal::new(4, 2);
        terminal.process("a界e\u{301}".as_bytes());
        let screen = terminal.screen();
        assert!(screen.get(1, 0).unwrap().wide);
        assert!(screen.get(2, 0).unwrap().continuation);
        assert_eq!(screen.get(3, 0).unwrap().combining, vec!['\u{301}']);
        assert_eq!(screen.cursor_x, 4);
        terminal.process("界".as_bytes());
        assert_eq!(terminal.screen().get(0, 1).unwrap().character, '界');
    }

    #[test]
    fn handles_utf8() {
        let mut terminal = Terminal::new(20, 5);

        terminal.process("Привет".as_bytes());

        assert!(terminal.screen().line(0).starts_with("Привет"));
    }
}
