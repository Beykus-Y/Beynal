// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::Cell as LayoutCache;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::terminal_graphics::{
    GraphicsState, MAX_APC_PAYLOAD_BYTES, MAX_SIXEL_PAYLOAD_BYTES, TerminalImage, base64_decode,
    base64_encode, compute_cell_span, decode_sixel,
};

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

/// Compact bitflags for SGR text attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CellAttrs(pub u16);

impl CellAttrs {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const BLINK: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const CONCEAL: u16 = 1 << 5;
    pub const STRIKETHROUGH: u16 = 1 << 6;
    pub const OVERLINE: u16 = 1 << 7;

    #[inline]
    pub fn contains(self, bit: u16) -> bool {
        (self.0 & bit) != 0
    }
    #[inline]
    pub fn set(&mut self, bit: u16) {
        self.0 |= bit;
    }
    #[inline]
    pub fn clear(&mut self, bit: u16) {
        self.0 &= !bit;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charset {
    #[default]
    Ascii,
    DecSpecialGraphics,
}

impl Charset {
    fn map(self, ch: char) -> char {
        if self != Self::DecSpecialGraphics {
            return ch;
        }
        match ch {
            '`' => '◆',
            'a' => '▒',
            'b' => '␉',
            'c' => '␌',
            'd' => '␍',
            'e' => '␊',
            'f' => '°',
            'g' => '±',
            'h' => '␤',
            'i' => '␋',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => ch,
        }
    }
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
    pub attrs: CellAttrs,
    pub underline: UnderlineStyle,
    pub underline_color: Option<[u8; 3]>,
    pub hyperlink_id: u32,
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
            attrs: CellAttrs::default(),
            underline: UnderlineStyle::None,
            underline_color: None,
            hyperlink_id: 0,
        }
    }
}

pub const DEFAULT_FG: [u8; 3] = [232, 232, 232];
pub const DEFAULT_BG: [u8; 3] = [18, 24, 32];

/// Safety timeout for DEC mode 2026 Synchronized Output so a buggy app can never freeze the display.
pub const SYNC_OUTPUT_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_HYPERLINKS: usize = 4096;
const MAX_OSC52_BYTES: usize = 1024 * 1024;
const MAX_KITTY_STACK: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: crate::theme::CursorShape,
    pub blink: bool,
}

/// Snapshot of active terminal input-encoding modes passed to the keyboard encoder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalInputModes {
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    pub backarrow_key: bool,
    pub modify_other_keys: u8,
    pub kitty_flags: u8,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub alternate_scroll: bool,
    pub alternate_screen: bool,
}

/// Semantic shell integration lifecycle event (OSC 133).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    PromptStart {
        abs_row: usize,
    },
    CommandInputStart {
        abs_row: usize,
    },
    CommandExecuted {
        abs_row: usize,
    },
    CommandFinished {
        abs_row: usize,
        exit_code: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    x: usize,
    y: usize,
    fg: [u8; 3],
    fg_origin: ColorOrigin,
    bg: [u8; 3],
    bg_origin: ColorOrigin,
    attrs: CellAttrs,
    underline: UnderlineStyle,
    underline_color: Option<[u8; 3]>,
    origin_mode: bool,
    auto_wrap: bool,
    charset_g0: Charset,
    charset_g1: Charset,
    active_gl: u8,
}

impl Default for SavedCursor {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            fg: DEFAULT_FG,
            fg_origin: ColorOrigin::DefaultForeground,
            bg: DEFAULT_BG,
            bg_origin: ColorOrigin::DefaultBackground,
            attrs: CellAttrs::default(),
            underline: UnderlineStyle::None,
            underline_color: None,
            origin_mode: false,
            auto_wrap: true,
            charset_g0: Charset::Ascii,
            charset_g1: Charset::Ascii,
            active_gl: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SavedRendition {
    fg: [u8; 3],
    fg_origin: ColorOrigin,
    bg: [u8; 3],
    bg_origin: ColorOrigin,
    attrs: CellAttrs,
    underline: UnderlineStyle,
    underline_color: Option<[u8; 3]>,
}

struct SavedScreen {
    cells: Vec<Cell>,
    cursor: (usize, usize),
    saved_cursor: SavedCursor,
    wraps: Vec<bool>,
    margins: (usize, usize),
    lr_margins: (usize, usize),
    style: Option<CursorStyle>,
    visible: bool,
    rendition: SavedRendition,
    kitty_keyboard_stack: Vec<u8>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DcsKind {
    None,
    Decrqss,
    Xtgettcap,
    Sixel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApcScanState {
    Ground,
    SawEsc,
    InApc,
    InApcSawEsc,
}

pub struct Terminal {
    parser: Parser,
    screen: Screen,
    apc_state: ApcScanState,
    apc_buf: Vec<u8>,
    apc_overflow: bool,
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
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    pub origin_mode: bool,
    pub auto_wrap: bool,
    pub reverse_wrap: bool,
    pub backarrow_key: bool,
    pub left_right_margin_mode: bool,
    pub insert_mode: bool,
    pub linefeed_new_line: bool,
    pub focus_reporting: bool,
    pub alternate_scroll: bool,
    pub synchronized_output: bool,
    sync_started: Option<Instant>,
    pub color_scheme_updates: bool,
    pub modify_other_keys: u8,
    kitty_keyboard_stack: Vec<u8>,
    mouse: crate::mouse::Mode,
    mouse_encoding: crate::mouse::Encoding,
    layout: LayoutMode,
    fg: [u8; 3],
    bg: [u8; 3],
    fg_origin: ColorOrigin,
    bg_origin: ColorOrigin,
    attrs: CellAttrs,
    underline: UnderlineStyle,
    underline_color: Option<[u8; 3]>,
    saved: SavedCursor,
    alternate: Option<SavedScreen>,
    scroll_top: usize,
    scroll_bottom: usize,
    left_margin: usize,
    right_margin: usize,
    tab_stops: Vec<bool>,
    charset_g0: Charset,
    charset_g1: Charset,
    active_gl: u8,
    last_printed_char: Option<char>,
    responses: Vec<u8>,
    theme: crate::theme::Theme,
    palette_overrides: [Option<[u8; 3]>; 256],
    fg_override: Option<[u8; 3]>,
    bg_override: Option<[u8; 3]>,
    cursor_color_override: Option<[u8; 3]>,
    history: VecDeque<(Vec<Cell>, bool)>,
    history_base: usize,
    history_bytes: usize,
    pub selection_epoch: u64,
    viewport_offset: usize,
    wraps: Vec<bool>,
    layout_cache: LayoutCache<Option<(u64, Option<usize>)>>,
    command_anchors: VecDeque<(usize, u64)>,
    next_command_id: u64,
    pending_shell_events: VecDeque<ShellEvent>,
    pub title: Option<String>,
    title_dirty: bool,
    pub cwd: Option<String>,
    pub progress: Option<(u8, u8)>,
    bell_pending: bool,
    pending_notifications: VecDeque<(String, String)>,
    hyperlinks: Vec<String>,
    active_hyperlink: u32,
    pending_clipboard_write: Option<String>,
    pub osc52_read_allowed: bool,
    pixel_size: (usize, usize),
    cell_pixel_size: (usize, usize),
    dcs_kind: DcsKind,
    dcs_buf: Vec<u8>,
    pub graphics: GraphicsState,
}

impl Screen {
    pub fn theme(&self) -> &crate::theme::Theme {
        &self.theme
    }
    pub fn is_alternate_screen(&self) -> bool {
        self.alternate.is_some()
    }
    pub fn has_custom_scroll_margins(&self) -> bool {
        self.scroll_top != 0 || self.scroll_bottom != self.rows.saturating_sub(1)
    }
    pub fn input_modes(&self) -> TerminalInputModes {
        TerminalInputModes {
            app_cursor_keys: self.app_cursor_keys,
            app_keypad: self.app_keypad,
            backarrow_key: self.backarrow_key,
            modify_other_keys: self.modify_other_keys,
            kitty_flags: self.kitty_keyboard_flags(),
            bracketed_paste: self.bracketed_paste,
            focus_reporting: self.focus_reporting,
            alternate_scroll: self.alternate_scroll,
            alternate_screen: self.alternate.is_some(),
        }
    }
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.kitty_keyboard_stack.last().copied().unwrap_or(0)
    }
    pub fn is_synchronized_active(&self) -> bool {
        self.synchronized_output
            && self
                .sync_started
                .is_some_and(|t| t.elapsed() < SYNC_OUTPUT_TIMEOUT)
    }
    pub fn set_pixel_metrics(&mut self, window_px: (usize, usize), cell_px: (usize, usize)) {
        self.pixel_size = window_px;
        self.cell_pixel_size = (cell_px.0.max(1), cell_px.1.max(1));
    }
    pub fn take_title(&mut self) -> Option<String> {
        if self.title_dirty {
            self.title_dirty = false;
            self.title.clone()
        } else {
            None
        }
    }
    #[allow(dead_code)]
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending)
    }
    pub fn take_clipboard_write(&mut self) -> Option<String> {
        self.pending_clipboard_write.take()
    }
    pub fn drain_shell_events(&mut self) -> Vec<ShellEvent> {
        self.pending_shell_events.drain(..).collect()
    }
    #[allow(dead_code)]
    pub fn drain_notifications(&mut self) -> Vec<(String, String)> {
        self.pending_notifications.drain(..).collect()
    }
    pub fn hyperlink_url(&self, id: u32) -> Option<&str> {
        if id == 0 {
            return None;
        }
        self.hyperlinks.get((id - 1) as usize).map(String::as_str)
    }
    pub fn latest_inline_display_row(&self) -> Option<usize> {
        let &(abs_row, _) = self.command_anchors.back()?;
        let top_abs = self.history_base + self.history.len().saturating_sub(self.viewport_offset);
        let rel = abs_row.checked_sub(top_abs)?;
        (rel < self.rows).then_some(rel)
    }
    pub fn prev_command_row(&self, from_abs_row: usize) -> Option<usize> {
        self.command_anchors
            .iter()
            .rev()
            .find(|(r, _)| *r < from_abs_row)
            .map(|(r, _)| *r)
    }
    pub fn next_command_row(&self, from_abs_row: usize) -> Option<usize> {
        self.command_anchors
            .iter()
            .find(|(r, _)| *r > from_abs_row)
            .map(|(r, _)| *r)
    }
    /// Search scrollback and live grid for `query`, returning `(abs_row, col_start, col_end)`.
    #[allow(dead_code)]
    pub fn search_scrollback(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<(usize, usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let mut results = Vec::new();
        let oldest = self.oldest_row();
        let newest = self.newest_row();
        for abs_row in oldest..newest {
            let Some(cells) = self.absolute_row(abs_row) else {
                continue;
            };
            let mut text = String::with_capacity(cells.len());
            let mut col_map = Vec::with_capacity(cells.len());
            for (col, cell) in cells.iter().enumerate() {
                if cell.continuation {
                    continue;
                }
                let start_byte = text.len();
                text.push(cell.character);
                for &c in &cell.combining {
                    text.push(c);
                }
                col_map.push((start_byte, col, if cell.wide { 2 } else { 1 }));
            }
            let hay = if case_sensitive {
                text.clone()
            } else {
                text.to_lowercase()
            };
            let mut search_from = 0usize;
            while let Some(pos) = hay[search_from..].find(&needle) {
                let byte_start = search_from + pos;
                let byte_end = byte_start + needle.len();
                let col_start = col_map
                    .iter()
                    .rev()
                    .find(|(b, _, _)| *b <= byte_start)
                    .map_or(0, |(_, c, _)| *c);
                let col_end = col_map
                    .iter()
                    .find(|(b, _, _)| *b >= byte_end)
                    .map_or(cells.len(), |(_, c, _)| *c);
                results.push((abs_row, col_start, col_end.max(col_start + 1)));
                if results.len() >= 1000 {
                    return results;
                }
                search_from = byte_start + needle.len().max(1);
                if search_from >= hay.len() {
                    break;
                }
            }
        }
        results
    }
    pub fn scroll_to_absolute_row(&mut self, abs_row: usize) -> bool {
        if self.alternate.is_some() {
            return false;
        }
        let live_start = self.history_base + self.history.len();
        let target_offset = live_start.saturating_sub(abs_row).min(self.history.len());
        let prev = self.viewport_offset;
        self.viewport_offset = target_offset;
        self.viewport_offset != prev
    }
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
        let tab_stops = default_tab_stops(columns);
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
            app_cursor_keys: false,
            app_keypad: false,
            origin_mode: false,
            auto_wrap: true,
            reverse_wrap: false,
            backarrow_key: false,
            left_right_margin_mode: false,
            insert_mode: false,
            linefeed_new_line: false,
            focus_reporting: false,
            alternate_scroll: true,
            synchronized_output: false,
            sync_started: None,
            color_scheme_updates: false,
            modify_other_keys: 0,
            kitty_keyboard_stack: Vec::new(),
            mouse: crate::mouse::Mode::Off,
            mouse_encoding: crate::mouse::Encoding::Legacy,
            layout,
            fg: theme.foreground,
            bg: theme.background,
            fg_origin: ColorOrigin::DefaultForeground,
            bg_origin: ColorOrigin::DefaultBackground,
            attrs: CellAttrs::default(),
            underline: UnderlineStyle::None,
            underline_color: None,
            saved: SavedCursor {
                fg: theme.foreground,
                bg: theme.background,
                ..SavedCursor::default()
            },
            alternate: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            left_margin: 0,
            right_margin: columns.saturating_sub(1),
            tab_stops,
            charset_g0: Charset::Ascii,
            charset_g1: Charset::Ascii,
            active_gl: 0,
            last_printed_char: None,
            responses: Vec::new(),
            theme,
            palette_overrides: [None; 256],
            fg_override: None,
            bg_override: None,
            cursor_color_override: None,
            history: VecDeque::new(),
            history_base: 0,
            history_bytes: 0,
            selection_epoch: 0,
            viewport_offset: 0,
            wraps: vec![false; rows],
            layout_cache: LayoutCache::new(None),
            command_anchors: VecDeque::new(),
            next_command_id: 1,
            pending_shell_events: VecDeque::new(),
            title: None,
            title_dirty: false,
            cwd: None,
            progress: None,
            bell_pending: false,
            pending_notifications: VecDeque::new(),
            hyperlinks: Vec::new(),
            active_hyperlink: 0,
            pending_clipboard_write: None,
            osc52_read_allowed: false,
            pixel_size: (columns * 9, rows * 18),
            cell_pixel_size: (9, 18),
            dcs_kind: DcsKind::None,
            dcs_buf: Vec::new(),
            graphics: GraphicsState::default(),
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

    pub fn mouse_encoding(&self) -> crate::mouse::Encoding {
        self.mouse_encoding
    }

    #[cfg(test)]
    pub fn mouse_sgr(&self) -> bool {
        matches!(
            self.mouse_encoding,
            crate::mouse::Encoding::Sgr | crate::mouse::Encoding::SgrPixels
        )
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

    fn effective_fg_default(&self) -> [u8; 3] {
        self.fg_override.unwrap_or(self.theme.foreground)
    }
    fn effective_bg_default(&self) -> [u8; 3] {
        self.bg_override.unwrap_or(self.theme.background)
    }
    fn effective_cursor_color(&self) -> [u8; 3] {
        self.cursor_color_override.unwrap_or(self.theme.cursor)
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
            change(&mut saved.rendition.fg, saved.rendition.fg_origin);
            change(&mut saved.rendition.bg, saved.rendition.bg_origin);
        }
        change(&mut self.fg, self.fg_origin);
        change(&mut self.bg, self.bg_origin);
        self.set_layout(theme.layout);
        self.theme = theme;
        self.trim_history();
        if self.color_scheme_updates {
            let code = self.color_scheme_code();
            self.responses
                .extend_from_slice(format!("\x1b[?997;{code}n").as_bytes());
        }
    }

    fn color_scheme_code(&self) -> u8 {
        let [r, g, b] = self.effective_bg_default();
        let luma = (u16::from(r) * 299 + u16::from(g) * 587 + u16::from(b) * 114) / 1000;
        if luma < 128 { 1 } else { 2 }
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
        self.graphics.prune_before(self.history_base);
    }

    fn save_top_rows(&mut self, count: usize) {
        if self.theme.scrollback_limit == 0 {
            self.history_base += count;
            self.graphics.prune_before(self.history_base);
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
        self.saved.y = self.saved.y.saturating_sub(shift);
        self.selection_epoch += 1;
        self.revision += 1;
        let alt_fg = self.effective_fg_default();
        let alt_bg = self.effective_bg_default();
        if let Some(saved) = &mut self.alternate {
            let blank = Cell {
                fg: alt_fg,
                bg: alt_bg,
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
            saved.saved_cursor.y = saved.saved_cursor.y.saturating_sub(main_shift);
            saved.saved_cursor.x = saved.saved_cursor.x.min(columns - 1);
            saved.saved_cursor.y = saved.saved_cursor.y.min(rows - 1);
            saved.wraps.resize(rows, false);
            saved.margins = (0, rows - 1);
            saved.lr_margins = (0, columns - 1);
        }
        self.wraps.resize(rows, false);
        self.columns = columns;
        self.rows = rows;
        self.tab_stops = default_tab_stops(columns);
        if old_columns != columns {
            self.cursor_x = self.cursor_x.min(columns - 1);
        }
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.saved.x = self.saved.x.min(columns - 1);
        self.saved.y = self.saved.y.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.left_margin = 0;
        self.right_margin = columns - 1;
    }

    fn active_charset(&self) -> Charset {
        if self.active_gl == 0 {
            self.charset_g0
        } else {
            self.charset_g1
        }
    }

    fn effective_left_margin(&self) -> usize {
        if self.left_right_margin_mode {
            self.left_margin.min(self.columns.saturating_sub(1))
        } else {
            0
        }
    }

    fn effective_right_margin(&self) -> usize {
        if self.left_right_margin_mode {
            self.right_margin
                .max(self.effective_left_margin())
                .min(self.columns.saturating_sub(1))
        } else {
            self.columns.saturating_sub(1)
        }
    }

    fn save_cursor_state(&mut self) {
        self.saved = SavedCursor {
            x: self.cursor_x,
            y: self.cursor_y,
            fg: self.fg,
            fg_origin: self.fg_origin,
            bg: self.bg,
            bg_origin: self.bg_origin,
            attrs: self.attrs,
            underline: self.underline,
            underline_color: self.underline_color,
            origin_mode: self.origin_mode,
            auto_wrap: self.auto_wrap,
            charset_g0: self.charset_g0,
            charset_g1: self.charset_g1,
            active_gl: self.active_gl,
        };
    }

    fn restore_cursor_state(&mut self) {
        let s = self.saved;
        self.cursor_x = s.x.min(self.columns.saturating_sub(1));
        self.cursor_y = s.y.min(self.rows.saturating_sub(1));
        self.fg = s.fg;
        self.fg_origin = s.fg_origin;
        self.bg = s.bg;
        self.bg_origin = s.bg_origin;
        self.attrs = s.attrs;
        self.underline = s.underline;
        self.underline_color = s.underline_color;
        self.origin_mode = s.origin_mode;
        self.auto_wrap = s.auto_wrap;
        self.charset_g0 = s.charset_g0;
        self.charset_g1 = s.charset_g1;
        self.active_gl = s.active_gl;
    }

    fn soft_reset(&mut self) {
        self.cursor_visible = true;
        self.origin_mode = false;
        self.auto_wrap = true;
        self.reverse_wrap = false;
        self.insert_mode = false;
        self.app_cursor_keys = false;
        self.app_keypad = false;
        self.left_right_margin_mode = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.left_margin = 0;
        self.right_margin = self.columns.saturating_sub(1);
        self.fg = self.effective_fg_default();
        self.bg = self.effective_bg_default();
        self.fg_origin = ColorOrigin::DefaultForeground;
        self.bg_origin = ColorOrigin::DefaultBackground;
        self.attrs = CellAttrs::default();
        self.underline = UnderlineStyle::None;
        self.underline_color = None;
        self.charset_g0 = Charset::Ascii;
        self.charset_g1 = Charset::Ascii;
        self.active_gl = 0;
        self.active_hyperlink = 0;
        self.saved = SavedCursor {
            fg: self.fg,
            bg: self.bg,
            ..SavedCursor::default()
        };
        self.revision += 1;
    }

    fn full_reset(&mut self) {
        self.alternate = None;
        self.soft_reset();
        self.bracketed_paste = false;
        self.focus_reporting = false;
        self.alternate_scroll = true;
        self.synchronized_output = false;
        self.sync_started = None;
        self.modify_other_keys = 0;
        self.kitty_keyboard_stack.clear();
        self.mouse = crate::mouse::Mode::Off;
        self.mouse_encoding = crate::mouse::Encoding::Legacy;
        self.cursor_style = None;
        self.linefeed_new_line = false;
        self.backarrow_key = false;
        self.palette_overrides = [None; 256];
        self.fg_override = None;
        self.bg_override = None;
        self.cursor_color_override = None;
        self.tab_stops = default_tab_stops(self.columns);
        self.cursor_x = 0;
        self.cursor_y = 0;
        let blank = self.blank();
        self.cells.fill(blank);
        self.wraps.fill(false);
        self.graphics.clear_all();
        self.selection_epoch += 1;
        self.revision += 1;
    }

    fn put_char(&mut self, raw_char: char) {
        if self.columns == 0 || self.rows == 0 || raw_char.is_control() {
            return;
        }
        let character = self.active_charset().map(raw_char);

        // Handle ZWJ continuation: if the previous cell ended with U+200D (ZWJ),
        // attach this character to the same grapheme cluster instead of advancing.
        if self.cursor_x > 0 {
            let mut prev_x = self.cursor_x.min(self.columns) - 1;
            if self.cells[self.cursor_y * self.columns + prev_x].continuation && prev_x > 0 {
                prev_x -= 1;
            }
            let prev_idx = self.cursor_y * self.columns + prev_x;
            if self.cells[prev_idx].combining.last() == Some(&'\u{200d}')
                && self.cells[prev_idx].combining.len() < 32
            {
                self.cells[prev_idx].combining.push(character);
                self.revision += 1;
                return;
            }
        }

        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            if self.cursor_x > 0 {
                let mut x = self.cursor_x.min(self.columns) - 1;
                if self.cells[self.cursor_y * self.columns + x].continuation && x > 0 {
                    x -= 1;
                }
                let idx = self.cursor_y * self.columns + x;
                let marks = &mut self.cells[idx].combining;
                if marks.len() < 32 {
                    marks.push(character);
                    // Emoji Presentation Selector (VS16): promote narrow base to wide if room.
                    if character == '\u{fe0f}'
                        && !self.cells[idx].wide
                        && x + 1 < self.columns
                        && self.cursor_x == x + 1
                    {
                        self.cells[idx].wide = true;
                        let cont_fg = self.cells[idx].fg;
                        let cont_bg = self.cells[idx].bg;
                        let cont_fg_o = self.cells[idx].fg_origin;
                        let cont_bg_o = self.cells[idx].bg_origin;
                        let cont_attrs = self.cells[idx].attrs;
                        let cont_ul = self.cells[idx].underline;
                        let cont_ulc = self.cells[idx].underline_color;
                        let cont_hl = self.cells[idx].hyperlink_id;
                        self.clear_cell(idx + 1);
                        self.cells[idx + 1] = Cell {
                            continuation: true,
                            written: true,
                            fg: cont_fg,
                            bg: cont_bg,
                            fg_origin: cont_fg_o,
                            bg_origin: cont_bg_o,
                            attrs: cont_attrs,
                            underline: cont_ul,
                            underline_color: cont_ulc,
                            hyperlink_id: cont_hl,
                            ..Cell::default()
                        };
                        self.cursor_x = x + 2;
                    }
                    self.revision += 1;
                }
            }
            return;
        }
        let left_m = self.effective_left_margin();
        let right_m = self.effective_right_margin();
        let margin_width = right_m - left_m + 1;
        if width > margin_width {
            return;
        }
        if self.cursor_x + width > right_m + 1 {
            if self.auto_wrap {
                if !self.left_right_margin_mode {
                    self.wraps[self.cursor_y] = true;
                }
                self.cursor_x = left_m;
                self.line_feed();
            } else {
                self.cursor_x = (right_m + 1).saturating_sub(width);
            }
        }
        if self.insert_mode {
            let start = self.cursor_y * self.columns + self.cursor_x;
            let end = self.cursor_y * self.columns + right_m + 1;
            if start + width <= end {
                self.cells[start..end].rotate_right(width);
                for idx in start..start + width {
                    self.cells[idx] = self.blank();
                }
            }
        }
        let index = self.cursor_y * self.columns + self.cursor_x;
        self.clear_cell(index);
        if width == 2 {
            self.clear_cell(index + 1);
        }
        let inverse = self.attrs.contains(CellAttrs::INVERSE);
        let (fg, bg) = if inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        let (fg_origin, bg_origin) = if inverse {
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
            attrs: self.attrs,
            underline: self.underline,
            underline_color: self.underline_color,
            hyperlink_id: self.active_hyperlink,
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
                attrs: self.attrs,
                underline: self.underline,
                underline_color: self.underline_color,
                hyperlink_id: self.active_hyperlink,
                ..Cell::default()
            };
        }
        self.cursor_x += width;
        self.last_printed_char = Some(character);
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
        let inverse = self.attrs.contains(CellAttrs::INVERSE);
        if inverse {
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
        let left_m = self.effective_left_margin();
        let right_m = self.effective_right_margin();
        let region_rows = self.scroll_bottom - self.scroll_top + 1;
        let lines = count.min(region_rows);
        let blank = self.blank();

        if self.left_right_margin_mode && (left_m > 0 || right_m + 1 < self.columns) {
            self.selection_epoch += 1;
            if up {
                for r in self.scroll_top..=self.scroll_bottom {
                    if r + lines <= self.scroll_bottom {
                        let dst_start = r * self.columns + left_m;
                        let src_start = (r + lines) * self.columns + left_m;
                        for offset in 0..=(right_m - left_m) {
                            self.cells[dst_start + offset] = self.cells[src_start + offset].clone();
                        }
                    } else {
                        let dst_start = r * self.columns + left_m;
                        for offset in 0..=(right_m - left_m) {
                            self.cells[dst_start + offset] = blank.clone();
                        }
                    }
                }
            } else {
                for r in (self.scroll_top..=self.scroll_bottom).rev() {
                    if r >= self.scroll_top + lines {
                        let dst_start = r * self.columns + left_m;
                        let src_start = (r - lines) * self.columns + left_m;
                        for offset in 0..=(right_m - left_m) {
                            self.cells[dst_start + offset] = self.cells[src_start + offset].clone();
                        }
                    } else {
                        let dst_start = r * self.columns + left_m;
                        for offset in 0..=(right_m - left_m) {
                            self.cells[dst_start + offset] = blank.clone();
                        }
                    }
                }
            }
            return;
        }

        let start = self.scroll_top * self.columns;
        let end = (self.scroll_bottom + 1) * self.columns;
        let n = lines * self.columns;
        if up {
            if self.alternate.is_none()
                && self.scroll_top == 0
                && self.scroll_bottom + 1 == self.rows
            {
                self.save_top_rows(lines);
            } else {
                self.selection_epoch += 1;
            }
            self.wraps[self.scroll_top..=self.scroll_bottom].rotate_left(lines);
            for row in (self.scroll_bottom + 1 - lines)..=self.scroll_bottom {
                self.wraps[row] = false;
            }
            self.shift_cells(start, end, n, true, &blank);
        } else {
            self.selection_epoch += 1;
            self.wraps[self.scroll_top..=self.scroll_bottom].rotate_right(lines);
            for row in self.scroll_top..self.scroll_top + lines {
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
        if let Some(rgb) = self.palette_overrides[index as usize] {
            return rgb;
        }
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
                    self.fg = self.effective_fg_default();
                    self.bg = self.effective_bg_default();
                    self.fg_origin = ColorOrigin::DefaultForeground;
                    self.bg_origin = ColorOrigin::DefaultBackground;
                    self.attrs = CellAttrs::default();
                    self.underline = UnderlineStyle::None;
                    self.underline_color = None;
                }
                1 => self.attrs.set(CellAttrs::BOLD),
                2 => self.attrs.set(CellAttrs::DIM),
                3 => self.attrs.set(CellAttrs::ITALIC),
                4 => {
                    self.underline = match param.get(1).copied() {
                        None | Some(1) => UnderlineStyle::Single,
                        Some(0) => UnderlineStyle::None,
                        Some(2) => UnderlineStyle::Double,
                        Some(3) => UnderlineStyle::Curly,
                        Some(4) => UnderlineStyle::Dotted,
                        Some(5) => UnderlineStyle::Dashed,
                        _ => UnderlineStyle::Single,
                    };
                }
                5 | 6 => self.attrs.set(CellAttrs::BLINK),
                7 => self.attrs.set(CellAttrs::INVERSE),
                8 => self.attrs.set(CellAttrs::CONCEAL),
                9 => self.attrs.set(CellAttrs::STRIKETHROUGH),
                21 => self.underline = UnderlineStyle::Double,
                22 => {
                    self.attrs.clear(CellAttrs::BOLD);
                    self.attrs.clear(CellAttrs::DIM);
                }
                23 => self.attrs.clear(CellAttrs::ITALIC),
                24 => self.underline = UnderlineStyle::None,
                25 => self.attrs.clear(CellAttrs::BLINK),
                27 => self.attrs.clear(CellAttrs::INVERSE),
                28 => self.attrs.clear(CellAttrs::CONCEAL),
                29 => self.attrs.clear(CellAttrs::STRIKETHROUGH),
                53 => self.attrs.set(CellAttrs::OVERLINE),
                55 => self.attrs.clear(CellAttrs::OVERLINE),
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
                    self.fg = self.effective_fg_default();
                    self.fg_origin = ColorOrigin::DefaultForeground;
                }
                49 => {
                    self.bg = self.effective_bg_default();
                    self.bg_origin = ColorOrigin::DefaultBackground;
                }
                59 => self.underline_color = None,
                38 | 48 | 58 => {
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
                        } else if p == 48 {
                            self.bg = color;
                            self.bg_origin = origin;
                        } else {
                            self.underline_color = Some(color);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn enter_alternate_screen(&mut self, clear: bool, save_cursor: bool) {
        if save_cursor {
            self.save_cursor_state();
        }
        if self.alternate.is_some() {
            return;
        }
        self.selection_epoch += 1;
        self.revision += 1;
        let new_cells = if clear {
            vec![self.blank(); self.columns * self.rows]
        } else {
            self.cells.clone()
        };
        let saved_kitty = std::mem::take(&mut self.kitty_keyboard_stack);
        self.alternate = Some(SavedScreen {
            cells: std::mem::replace(&mut self.cells, new_cells),
            cursor: (self.cursor_x, self.cursor_y),
            saved_cursor: self.saved,
            wraps: std::mem::replace(&mut self.wraps, vec![false; self.rows]),
            margins: (self.scroll_top, self.scroll_bottom),
            lr_margins: (self.left_margin, self.right_margin),
            style: self.cursor_style,
            visible: self.cursor_visible,
            rendition: SavedRendition {
                fg: self.fg,
                fg_origin: self.fg_origin,
                bg: self.bg,
                bg_origin: self.bg_origin,
                attrs: self.attrs,
                underline: self.underline,
                underline_color: self.underline_color,
            },
            kitty_keyboard_stack: saved_kitty,
        });
        if clear {
            self.cursor_x = 0;
            self.cursor_y = 0;
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.left_margin = 0;
        self.right_margin = self.columns.saturating_sub(1);
    }

    fn leave_alternate_screen(&mut self, restore_cursor: bool) {
        if let Some(saved) = self.alternate.take() {
            self.graphics.clear_for_screen(true);
            self.selection_epoch += 1;
            self.revision += 1;
            self.wraps = saved.wraps;
            self.cells = saved.cells;
            (self.cursor_x, self.cursor_y) = saved.cursor;
            self.saved = saved.saved_cursor;
            (self.scroll_top, self.scroll_bottom) = saved.margins;
            (self.left_margin, self.right_margin) = saved.lr_margins;
            self.cursor_style = saved.style;
            self.cursor_visible = saved.visible;
            self.fg = saved.rendition.fg;
            self.fg_origin = saved.rendition.fg_origin;
            self.bg = saved.rendition.bg;
            self.bg_origin = saved.rendition.bg_origin;
            self.attrs = saved.rendition.attrs;
            self.underline = saved.rendition.underline;
            self.underline_color = saved.rendition.underline_color;
            self.kitty_keyboard_stack = saved.kitty_keyboard_stack;
            if restore_cursor {
                self.restore_cursor_state();
            }
        }
    }

    fn handle_decrqss(&mut self, query: &[u8]) {
        match query {
            b"m" => {
                let mut parts = vec!["0".to_string()];
                if self.attrs.contains(CellAttrs::BOLD) {
                    parts.push("1".into());
                }
                if self.attrs.contains(CellAttrs::DIM) {
                    parts.push("2".into());
                }
                if self.attrs.contains(CellAttrs::ITALIC) {
                    parts.push("3".into());
                }
                match self.underline {
                    UnderlineStyle::None => {}
                    UnderlineStyle::Single => parts.push("4".into()),
                    UnderlineStyle::Double => parts.push("4:2".into()),
                    UnderlineStyle::Curly => parts.push("4:3".into()),
                    UnderlineStyle::Dotted => parts.push("4:4".into()),
                    UnderlineStyle::Dashed => parts.push("4:5".into()),
                }
                if self.attrs.contains(CellAttrs::BLINK) {
                    parts.push("5".into());
                }
                if self.attrs.contains(CellAttrs::INVERSE) {
                    parts.push("7".into());
                }
                if self.attrs.contains(CellAttrs::CONCEAL) {
                    parts.push("8".into());
                }
                if self.attrs.contains(CellAttrs::STRIKETHROUGH) {
                    parts.push("9".into());
                }
                if self.attrs.contains(CellAttrs::OVERLINE) {
                    parts.push("53".into());
                }
                let reply = format!("\x1bP1$r{}m\x1b\\", parts.join(";"));
                self.responses.extend_from_slice(reply.as_bytes());
            }
            b"r" => {
                let reply = format!(
                    "\x1bP1$r{};{}r\x1b\\",
                    self.scroll_top + 1,
                    self.scroll_bottom + 1
                );
                self.responses.extend_from_slice(reply.as_bytes());
            }
            b" q" => {
                let code = match self.cursor_style {
                    None => 0,
                    Some(CursorStyle { shape, blink }) => match (shape, blink) {
                        (crate::theme::CursorShape::Block, true) => 1,
                        (crate::theme::CursorShape::Block, false) => 2,
                        (crate::theme::CursorShape::Underline, true) => 3,
                        (crate::theme::CursorShape::Underline, false) => 4,
                        (crate::theme::CursorShape::Beam, true) => 5,
                        (crate::theme::CursorShape::Beam, false) => 6,
                    },
                };
                self.responses
                    .extend_from_slice(format!("\x1bP1$r{code} q\x1b\\").as_bytes());
            }
            _ => {
                self.responses.extend_from_slice(b"\x1bP0$r\x1b\\");
            }
        }
    }

    fn handle_xtgettcap(&mut self, query: &[u8]) {
        for hex_cap in query.split(|&b| b == b';') {
            if hex_cap.is_empty() {
                continue;
            }
            let Some(name_bytes) = hex_decode(hex_cap) else {
                continue;
            };
            let name = String::from_utf8_lossy(&name_bytes);
            let value: Option<&[u8]> = match name.as_ref() {
                "TN" | "name" => Some(b"xterm-256color"),
                "RGB" | "Tc" => Some(b""),
                "colors" | "Co" => Some(b"256"),
                "Su" => Some(b""),
                "Sync" => Some(b""),
                "setrgbf" => Some(b"\x1b[38;2;%p1%d;%p2%d;%p3%dm"),
                "setrgbb" => Some(b"\x1b[48;2;%p1%d;%p2%d;%p3%dm"),
                _ => None,
            };
            let hex_name = hex_encode(&name_bytes);
            if let Some(val) = value {
                let reply = if val.is_empty() {
                    format!("\x1bP1+r{hex_name}\x1b\\")
                } else {
                    format!("\x1bP1+r{hex_name}={}\x1b\\", hex_encode(val))
                };
                self.responses.extend_from_slice(reply.as_bytes());
            } else {
                self.responses
                    .extend_from_slice(format!("\x1bP0+r{hex_name}\x1b\\").as_bytes());
            }
        }
    }

    fn handle_sixel(&mut self, payload: &[u8]) {
        if let Some((w, h, rgba)) = decode_sixel(payload) {
            let (cols, rows) = compute_cell_span(w, h, 0, 0, self.cell_pixel_size, self.columns);
            let abs_row = self.history_base + self.history.len() + self.cursor_y;
            let internal_id = self.graphics.next_id();
            self.graphics.add_placement(TerminalImage {
                id: internal_id,
                kitty_id: 0,
                abs_row,
                col: self.cursor_x.min(self.columns.saturating_sub(1)),
                cols,
                rows,
                src_w: w,
                src_h: h,
                rgba: Arc::new(rgba),
                alternate: self.alternate.is_some(),
            });
            for _ in 0..rows {
                self.line_feed();
            }
            self.cursor_x = self.effective_left_margin();
            self.revision += 1;
        }
    }

    fn handle_kitty_apc(&mut self, body: &[u8]) {
        let abs_row = self.history_base + self.history.len() + self.cursor_y;
        let col = self.cursor_x.min(self.columns.saturating_sub(1));
        let (reply, advance_rows) = self.graphics.handle_kitty_apc(
            body,
            abs_row,
            col,
            self.cell_pixel_size,
            self.columns,
            self.alternate.is_some(),
        );
        if !reply.is_empty() {
            self.responses.extend_from_slice(&reply);
        }
        if advance_rows > 0 {
            for _ in 0..advance_rows {
                self.line_feed();
            }
            self.revision += 1;
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

fn default_tab_stops(columns: usize) -> Vec<bool> {
    let mut stops = vec![false; columns];
    for i in (8..columns).step_by(8) {
        stops[i] = true;
    }
    stops
}

fn parse_osc_color(spec: &str) -> Option<[u8; 3]> {
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let parse_comp = |s: &str| -> Option<u8> {
            let v = u32::from_str_radix(s, 16).ok()?;
            match s.len() {
                1 => Some((v * 17) as u8),
                2 => Some(v as u8),
                3 => Some((v >> 4) as u8),
                4 => Some((v >> 8) as u8),
                _ => None,
            }
        };
        let r = parse_comp(parts.next()?)?;
        let g = parse_comp(parts.next()?)?;
        let b = parse_comp(parts.next()?)?;
        return Some([r, g, b]);
    }
    if let Some(hex) = spec.strip_prefix('#')
        && hex.len() == 6
    {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some([r, g, b]);
    }
    None
}

fn hex_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let s = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02X}");
    }
    out
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
            0x07 => {
                self.bell_pending = true;
            }
            b'\n' | 0x0b | 0x0c => {
                self.cursor_x = self.cursor_x.min(self.columns.saturating_sub(1));
                self.line_feed();
                if self.linefeed_new_line {
                    self.cursor_x = self.effective_left_margin();
                }
            }
            b'\r' => {
                let left = self.effective_left_margin();
                self.cursor_x = if self.cursor_x >= left { left } else { 0 };
            }
            0x08 => {
                let left = self.effective_left_margin();
                if self.cursor_x > left {
                    self.cursor_x -= 1;
                } else if self.reverse_wrap
                    && self.auto_wrap
                    && self.cursor_x == 0
                    && self.cursor_y > 0
                    && self.wraps[self.cursor_y - 1]
                {
                    self.cursor_y -= 1;
                    self.cursor_x = self.columns.saturating_sub(1);
                } else {
                    self.cursor_x = self.cursor_x.saturating_sub(1);
                }
            }
            b'\t' => {
                let right = self.effective_right_margin();
                let mut next = self.cursor_x + 1;
                while next < right && !self.tab_stops.get(next).copied().unwrap_or(false) {
                    next += 1;
                }
                self.cursor_x = next.min(right);
            }
            0x0e => self.active_gl = 1, // SO (Shift Out -> G1)
            0x0f => self.active_gl = 0, // SI (Shift In -> G0)
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        // DECRQM for DEC private modes: CSI ? Ps $ p
        if intermediates == b"?$" && action == 'p' {
            for mode in params.iter().filter_map(|p| p.first()) {
                let status = match mode {
                    1 => {
                        if self.app_cursor_keys {
                            1
                        } else {
                            2
                        }
                    }
                    6 => {
                        if self.origin_mode {
                            1
                        } else {
                            2
                        }
                    }
                    7 => {
                        if self.auto_wrap {
                            1
                        } else {
                            2
                        }
                    }
                    9 => {
                        if self.mouse == crate::mouse::Mode::X10 {
                            1
                        } else {
                            2
                        }
                    }
                    25 => {
                        if self.cursor_visible {
                            1
                        } else {
                            2
                        }
                    }
                    45 => {
                        if self.reverse_wrap {
                            1
                        } else {
                            2
                        }
                    }
                    47 | 1047 | 1049 => {
                        if self.alternate.is_some() {
                            1
                        } else {
                            2
                        }
                    }
                    67 => {
                        if self.backarrow_key {
                            1
                        } else {
                            2
                        }
                    }
                    69 => {
                        if self.left_right_margin_mode {
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
                    1004 => {
                        if self.focus_reporting {
                            1
                        } else {
                            2
                        }
                    }
                    1005 => {
                        if self.mouse_encoding == crate::mouse::Encoding::Utf8 {
                            1
                        } else {
                            2
                        }
                    }
                    1006 => {
                        if self.mouse_encoding == crate::mouse::Encoding::Sgr {
                            1
                        } else {
                            2
                        }
                    }
                    1007 => {
                        if self.alternate_scroll {
                            1
                        } else {
                            2
                        }
                    }
                    1015 => {
                        if self.mouse_encoding == crate::mouse::Encoding::Urxvt {
                            1
                        } else {
                            2
                        }
                    }
                    1016 => {
                        if self.mouse_encoding == crate::mouse::Encoding::SgrPixels {
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
                    2026 => {
                        if self.synchronized_output {
                            1
                        } else {
                            2
                        }
                    }
                    2031 => {
                        if self.color_scheme_updates {
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
        // DECRQM for ANSI modes: CSI Ps $ p
        if intermediates == b"$" && action == 'p' {
            for mode in params.iter().filter_map(|p| p.first()) {
                let status = match mode {
                    4 => {
                        if self.insert_mode {
                            1
                        } else {
                            2
                        }
                    }
                    20 => {
                        if self.linefeed_new_line {
                            1
                        } else {
                            2
                        }
                    }
                    _ => 0,
                };
                self.responses
                    .extend_from_slice(format!("\x1b[{mode};{status}$y").as_bytes());
            }
            return;
        }
        // DECSTR Soft Terminal Reset: CSI ! p
        if intermediates == b"!" && action == 'p' {
            self.soft_reset();
            return;
        }
        // Kitty Keyboard Protocol:
        // CSI ? u (query), CSI > flags u (push), CSI < count u (pop), CSI = flags ; mode u (set)
        if action == 'u' && matches!(intermediates, b"?" | b">" | b"<" | b"=") {
            match intermediates {
                b"?" => {
                    let flags = self.kitty_keyboard_flags();
                    self.responses
                        .extend_from_slice(format!("\x1b[?{flags}u").as_bytes());
                }
                b">" => {
                    let flags = params
                        .iter()
                        .next()
                        .and_then(|p| p.first())
                        .copied()
                        .unwrap_or(0) as u8
                        & 0x1f;
                    if self.kitty_keyboard_stack.len() >= MAX_KITTY_STACK {
                        self.kitty_keyboard_stack.remove(0);
                    }
                    self.kitty_keyboard_stack.push(flags);
                }
                b"<" => {
                    let count = params
                        .iter()
                        .next()
                        .and_then(|p| p.first())
                        .copied()
                        .unwrap_or(1)
                        .max(1) as usize;
                    let new_len = self.kitty_keyboard_stack.len().saturating_sub(count);
                    self.kitty_keyboard_stack.truncate(new_len);
                }
                b"=" => {
                    let mut it = params.iter();
                    let flags =
                        it.next().and_then(|p| p.first()).copied().unwrap_or(0) as u8 & 0x1f;
                    let mode = it.next().and_then(|p| p.first()).copied().unwrap_or(1);
                    if self.kitty_keyboard_stack.is_empty() {
                        self.kitty_keyboard_stack.push(0);
                    }
                    if let Some(top) = self.kitty_keyboard_stack.last_mut() {
                        match mode {
                            1 => *top = flags,
                            2 => *top |= flags,
                            3 => *top &= !flags,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        // Secondary DA (CSI > c), XTVERSION (CSI > q), modifyOtherKeys (CSI > 4 ; m)
        if intermediates == b">" {
            let mut it = params.iter();
            let p0 = it.next().and_then(|p| p.first()).copied().unwrap_or(0);
            let p1 = it.next().and_then(|p| p.first()).copied();
            match action {
                'c' if p0 == 0 => {
                    // VT220-compatible secondary DA
                    self.responses.extend_from_slice(b"\x1b[>1;100;0c");
                }
                'q' if p0 == 0 => {
                    let ver = env!("CARGO_PKG_VERSION");
                    self.responses
                        .extend_from_slice(format!("\x1bP>|Beyrix {ver}\x1b\\").as_bytes());
                }
                'm' if p0 == 4 => {
                    self.modify_other_keys = p1.unwrap_or(0).min(2) as u8;
                }
                _ => {}
            }
            return;
        }
        // DEC Private Modes & Color Scheme Query (CSI ? ...)
        if intermediates == b"?" {
            if action == 'n' {
                let first = params
                    .iter()
                    .next()
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(0);
                if first == 996 {
                    let code = self.color_scheme_code();
                    self.responses
                        .extend_from_slice(format!("\x1b[?997;{code}n").as_bytes());
                } else if first == 6 {
                    self.responses.extend_from_slice(
                        format!(
                            "\x1b[?{};{}R",
                            self.cursor_y + 1,
                            self.cursor_x.min(self.columns.saturating_sub(1)) + 1
                        )
                        .as_bytes(),
                    );
                }
                return;
            }
            for param in params.iter().filter_map(|p| p.first()) {
                match (*param, action) {
                    (1, 'h') => self.app_cursor_keys = true,
                    (1, 'l') => self.app_cursor_keys = false,
                    (6, 'h') => {
                        self.origin_mode = true;
                        self.cursor_x = self.effective_left_margin();
                        self.cursor_y = self.scroll_top;
                    }
                    (6, 'l') => {
                        self.origin_mode = false;
                        self.cursor_x = 0;
                        self.cursor_y = 0;
                    }
                    (7, 'h') => self.auto_wrap = true,
                    (7, 'l') => self.auto_wrap = false,
                    (9 | 1000 | 1002 | 1003, 'h') => {
                        if let Some(mode) = crate::mouse::Mode::from_decimal((*param).into()) {
                            self.mouse = mode;
                        }
                    }
                    (9 | 1000 | 1002 | 1003, 'l') => {
                        if crate::mouse::Mode::from_decimal((*param).into()) == Some(self.mouse) {
                            self.mouse = crate::mouse::Mode::Off;
                        }
                    }
                    (25, 'h') => self.cursor_visible = true,
                    (25, 'l') => self.cursor_visible = false,
                    (45, 'h') => self.reverse_wrap = true,
                    (45, 'l') => self.reverse_wrap = false,
                    (47, 'h') => self.enter_alternate_screen(false, false),
                    (47, 'l') => self.leave_alternate_screen(false),
                    (67, 'h') => self.backarrow_key = true,
                    (67, 'l') => self.backarrow_key = false,
                    (69, 'h') => self.left_right_margin_mode = true,
                    (69, 'l') => {
                        self.left_right_margin_mode = false;
                        self.left_margin = 0;
                        self.right_margin = self.columns.saturating_sub(1);
                    }
                    (1004, 'h') => self.focus_reporting = true,
                    (1004, 'l') => self.focus_reporting = false,
                    (1005 | 1006 | 1015 | 1016, 'h') => {
                        if let Some(enc) = crate::mouse::Encoding::from_decimal((*param).into()) {
                            self.mouse_encoding = enc;
                        }
                    }
                    (1005 | 1006 | 1015 | 1016, 'l') => {
                        if crate::mouse::Encoding::from_decimal((*param).into())
                            == Some(self.mouse_encoding)
                        {
                            self.mouse_encoding = crate::mouse::Encoding::Legacy;
                        }
                    }
                    (1007, 'h') => self.alternate_scroll = true,
                    (1007, 'l') => self.alternate_scroll = false,
                    (1047, 'h') => self.enter_alternate_screen(true, false),
                    (1047, 'l') => self.leave_alternate_screen(false),
                    (1048, 'h') => self.save_cursor_state(),
                    (1048, 'l') => self.restore_cursor_state(),
                    (1049, 'h') => self.enter_alternate_screen(true, true),
                    (1049, 'l') => self.leave_alternate_screen(true),
                    (2004, 'h') => self.bracketed_paste = true,
                    (2004, 'l') => self.bracketed_paste = false,
                    (2026, 'h') => {
                        self.synchronized_output = true;
                        self.sync_started = Some(Instant::now());
                    }
                    (2026, 'l') => {
                        self.synchronized_output = false;
                        self.sync_started = None;
                        self.revision += 1;
                    }
                    (2031, 'h') => self.color_scheme_updates = true,
                    (2031, 'l') => self.color_scheme_updates = false,
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
            'h' => {
                for param in params.iter().filter_map(|p| p.first()) {
                    match *param {
                        4 => self.insert_mode = true,
                        20 => self.linefeed_new_line = true,
                        _ => {}
                    }
                }
            }
            'l' => {
                for param in params.iter().filter_map(|p| p.first()) {
                    match *param {
                        4 => self.insert_mode = false,
                        20 => self.linefeed_new_line = false,
                        _ => {}
                    }
                }
            }
            'A' => {
                let top = if self.cursor_y >= self.scroll_top {
                    self.scroll_top
                } else {
                    0
                };
                self.cursor_y = self.cursor_y.saturating_sub(first.max(1)).max(top);
            }
            'B' => {
                let bottom = if self.cursor_y <= self.scroll_bottom {
                    self.scroll_bottom
                } else {
                    self.rows.saturating_sub(1)
                };
                self.cursor_y = (self.cursor_y + first.max(1)).min(bottom);
            }
            'C' => {
                let right = self.effective_right_margin();
                self.cursor_x = (self.cursor_x + first.max(1)).min(right);
            }
            'D' => {
                let left = self.effective_left_margin();
                let cur = self.cursor_x.min(self.columns.saturating_sub(1));
                self.cursor_x =
                    cur.saturating_sub(first.max(1))
                        .max(if cur >= left { left } else { 0 });
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

                if self.origin_mode {
                    self.cursor_y = (self.scroll_top + row - 1).min(self.scroll_bottom);
                    self.cursor_x =
                        (self.effective_left_margin() + col - 1).min(self.effective_right_margin());
                } else {
                    self.cursor_y = (row - 1).min(self.rows.saturating_sub(1));
                    self.cursor_x = (col - 1).min(self.columns.saturating_sub(1));
                }
            }
            'I' => {
                // CHT: Cursor Forward Tabulation
                let n = first.max(1);
                let right = self.effective_right_margin();
                for _ in 0..n {
                    let mut next = self.cursor_x + 1;
                    while next < right && !self.tab_stops.get(next).copied().unwrap_or(false) {
                        next += 1;
                    }
                    self.cursor_x = next.min(right);
                }
            }
            'Z' => {
                // CBT: Cursor Backward Tabulation
                let n = first.max(1);
                let left = self.effective_left_margin();
                for _ in 0..n {
                    let mut prev = self.cursor_x.saturating_sub(1);
                    while prev > left && !self.tab_stops.get(prev).copied().unwrap_or(false) {
                        prev -= 1;
                    }
                    self.cursor_x = prev.max(left);
                }
            }
            'g' => {
                // TBC: Tab Clear
                match first {
                    0 => {
                        if let Some(slot) = self.tab_stops.get_mut(self.cursor_x) {
                            *slot = false;
                        }
                    }
                    3 => self.tab_stops.fill(false),
                    _ => {}
                }
            }
            'b' => {
                // REP: Repeat preceding graphic character
                if let Some(ch) = self.last_printed_char {
                    let count = first.max(1).min(self.columns * self.rows);
                    for _ in 0..count {
                        self.put_char(ch);
                    }
                }
            }
            'n' if first == 5 => self.responses.extend_from_slice(b"\x1b[0n"),
            'n' if first == 6 => {
                let (ry, rx) = if self.origin_mode {
                    (
                        self.cursor_y.saturating_sub(self.scroll_top) + 1,
                        self.cursor_x
                            .min(self.columns.saturating_sub(1))
                            .saturating_sub(self.effective_left_margin())
                            + 1,
                    )
                } else {
                    (
                        self.cursor_y + 1,
                        self.cursor_x.min(self.columns.saturating_sub(1)) + 1,
                    )
                };
                self.responses
                    .extend_from_slice(format!("\x1b[{ry};{rx}R").as_bytes());
            }
            'c' if first == 0 => {
                // VT220 (62) with ANSI color (22)
                self.responses.extend_from_slice(b"\x1b[?62;22c");
            }
            't' => {
                // Window manipulation / size queries
                match first {
                    14 => {
                        let (w, h) = self.pixel_size;
                        self.responses
                            .extend_from_slice(format!("\x1b[4;{h};{w}t").as_bytes());
                    }
                    16 => {
                        let (cw, ch) = self.cell_pixel_size;
                        self.responses
                            .extend_from_slice(format!("\x1b[6;{ch};{cw}t").as_bytes());
                    }
                    18 => {
                        self.responses.extend_from_slice(
                            format!("\x1b[8;{};{}t", self.rows, self.columns).as_bytes(),
                        );
                    }
                    _ => {}
                }
            }
            'm' => self.sgr(params),
            's' => {
                if self.left_right_margin_mode {
                    let left = first.max(1);
                    let right = params
                        .iter()
                        .nth(1)
                        .and_then(|p| p.first())
                        .copied()
                        .unwrap_or(self.columns as u16) as usize;
                    if left < right && right <= self.columns {
                        self.left_margin = left - 1;
                        self.right_margin = right - 1;
                        self.cursor_x = if self.origin_mode {
                            self.left_margin
                        } else {
                            0
                        };
                        self.cursor_y = if self.origin_mode { self.scroll_top } else { 0 };
                    }
                } else {
                    self.save_cursor_state();
                }
            }
            'u' => {
                self.restore_cursor_state();
            }
            'G' | '`' => {
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
                self.cursor_x = self.effective_left_margin();
            }
            'F' => {
                self.cursor_y = self.cursor_y.saturating_sub(first.max(1));
                self.cursor_x = self.effective_left_margin();
            }
            'J' => {
                let pos = self.cursor_y * self.columns
                    + self.cursor_x.min(self.columns.saturating_sub(1));
                match first {
                    0 => self.erase(pos, self.cells.len()),
                    1 => self.erase(0, pos + 1),
                    2 => {
                        self.erase(0, self.cells.len());
                        self.graphics.clear_for_screen(self.alternate.is_some());
                    }
                    3 if self.alternate.is_none() => {
                        self.history_base += self.history.len();
                        self.history.clear();
                        self.history_bytes = 0;
                        self.viewport_offset = 0;
                        self.graphics.prune_before(self.history_base);
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
                let right = self.effective_right_margin();
                let end = self.cursor_y * self.columns + right + 1;
                if start >= end {
                    return;
                }
                let n = first.max(1).min(end - start);
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
                    self.cursor_x = if self.origin_mode {
                        self.effective_left_margin()
                    } else {
                        0
                    };
                    self.cursor_y = if self.origin_mode { self.scroll_top } else { 0 };
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.dcs_buf.clear();
        if ignore {
            self.dcs_kind = DcsKind::None;
            return;
        }
        self.dcs_kind = match (intermediates, action) {
            (b"$", 'q') => DcsKind::Decrqss,
            (b"+", 'q') => DcsKind::Xtgettcap,
            ([], 'q') => DcsKind::Sixel,
            _ => DcsKind::None,
        };
    }

    fn put(&mut self, byte: u8) {
        let limit = match self.dcs_kind {
            DcsKind::None => return,
            DcsKind::Sixel => MAX_SIXEL_PAYLOAD_BYTES,
            DcsKind::Decrqss | DcsKind::Xtgettcap => 4096,
        };
        if self.dcs_buf.len() < limit {
            self.dcs_buf.push(byte);
        }
    }

    fn unhook(&mut self) {
        let kind = self.dcs_kind;
        self.dcs_kind = DcsKind::None;
        let payload = std::mem::take(&mut self.dcs_buf);
        match kind {
            DcsKind::Decrqss => self.handle_decrqss(&payload),
            DcsKind::Xtgettcap => self.handle_xtgettcap(&payload),
            DcsKind::Sixel => self.handle_sixel(&payload),
            DcsKind::None => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let end = if bell_terminated { "\x07" } else { "\x1b\\" };
        match params {
            // OSC 0 / 2: Set window title
            [b"0" | b"2", title_bytes, ..] => {
                let clean: String = String::from_utf8_lossy(title_bytes)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect();
                self.title = Some(clean);
                self.title_dirty = true;
            }
            // OSC 4: Query or set indexed palette color (0..255)
            [b"4", rest @ ..] => {
                for pair in rest.as_chunks::<2>().0 {
                    let Ok(idx) = std::str::from_utf8(pair[0]).unwrap_or("").parse::<u8>() else {
                        continue;
                    };
                    let val = std::str::from_utf8(pair[1]).unwrap_or("");
                    if val == "?" {
                        let [r, g, b] = self.color(idx);
                        let (r, g, b) =
                            (u16::from(r) * 257, u16::from(g) * 257, u16::from(b) * 257);
                        self.responses.extend_from_slice(
                            format!("\x1b]4;{idx};rgb:{r:04x}/{g:04x}/{b:04x}{end}").as_bytes(),
                        );
                    } else if let Some(rgb) = parse_osc_color(val) {
                        self.palette_overrides[idx as usize] = Some(rgb);
                        self.revision += 1;
                    }
                }
            }
            // OSC 104: Reset palette color(s)
            [b"104"] | [b"104", b""] => {
                self.palette_overrides = [None; 256];
                self.revision += 1;
            }
            [b"104", rest @ ..] => {
                for item in rest {
                    if let Ok(idx) = std::str::from_utf8(item).unwrap_or("").parse::<u8>() {
                        self.palette_overrides[idx as usize] = None;
                    }
                }
                self.revision += 1;
            }
            // OSC 7: Current working directory (file://host/path)
            [b"7", uri_bytes, ..] => {
                let uri = String::from_utf8_lossy(uri_bytes);
                if let Some(after_scheme) = uri.strip_prefix("file://")
                    && let Some(slash) = after_scheme.find('/')
                {
                    let raw_path = &after_scheme[slash..];
                    let decoded = percent_decode(raw_path);
                    if !decoded.is_empty() && !decoded.chars().any(|c| c.is_control()) {
                        self.cwd = Some(decoded);
                    }
                }
            }
            // OSC 8: Hyperlinks (OSC 8 ; params ; uri ST)
            [b"8", _params, uri_bytes, ..] => {
                let uri = String::from_utf8_lossy(uri_bytes);
                if uri.is_empty() {
                    self.active_hyperlink = 0;
                } else if uri.len() <= 2048 && !uri.chars().any(|c| c.is_control()) {
                    if let Some(pos) = self.hyperlinks.iter().position(|u| u == uri.as_ref()) {
                        self.active_hyperlink = (pos + 1) as u32;
                    } else if self.hyperlinks.len() < MAX_HYPERLINKS {
                        self.hyperlinks.push(uri.into_owned());
                        self.active_hyperlink = self.hyperlinks.len() as u32;
                    }
                }
            }
            [b"8", ..] => {
                self.active_hyperlink = 0;
            }
            // OSC 9;4;state;progress (ConEmu / Windows Terminal progress) or OSC 9;message
            [b"9", b"4", state_b, rest @ ..] => {
                let st = std::str::from_utf8(state_b)
                    .ok()
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(0);
                let pct = rest
                    .first()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(0)
                    .min(100);
                self.progress = if st == 0 { None } else { Some((st, pct)) };
                self.revision += 1;
            }
            [b"9", msg_b, ..] => {
                let msg: String = String::from_utf8_lossy(msg_b)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect();
                if !msg.is_empty() {
                    if self.pending_notifications.len() >= 8 {
                        self.pending_notifications.pop_front();
                    }
                    self.pending_notifications.push_back(("Beyrix".into(), msg));
                }
            }
            // OSC 777;notify;title;body
            [b"777", b"notify", title_b, body_b, ..] => {
                let title: String = String::from_utf8_lossy(title_b)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(128)
                    .collect();
                let body: String = String::from_utf8_lossy(body_b)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect();
                if self.pending_notifications.len() >= 8 {
                    self.pending_notifications.pop_front();
                }
                self.pending_notifications.push_back((title, body));
            }
            // OSC 52: Clipboard integration (write allowed up to limit; read disabled by default)
            [b"52", _sel, payload, ..] => {
                if *payload == b"?" {
                    if self.osc52_read_allowed {
                        // Empty clipboard response when allowed and no buffer attached
                        let b64 = base64_encode(b"");
                        self.responses
                            .extend_from_slice(format!("\x1b]52;c;{b64}{end}").as_bytes());
                    }
                } else if payload.len() <= MAX_OSC52_BYTES * 2
                    && let Some(decoded) = base64_decode(payload)
                    && decoded.len() <= MAX_OSC52_BYTES
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    self.pending_clipboard_write = Some(text);
                }
            }
            // OSC 133: Shell integration (A=prompt, B=input, C=exec, D=finished[;exit_code])
            [b"133", kind, rest @ ..] => {
                let abs_row = self.history_base + self.history.len() + self.cursor_y;
                match kind.first() {
                    Some(b'A') => {
                        let id = self.next_command_id;
                        self.next_command_id += 1;
                        if self.command_anchors.len() >= 256 {
                            self.command_anchors.pop_front();
                        }
                        self.command_anchors.push_back((abs_row, id));
                        self.push_shell_event(ShellEvent::PromptStart { abs_row });
                    }
                    Some(b'B') => {
                        self.push_shell_event(ShellEvent::CommandInputStart { abs_row });
                    }
                    Some(b'C') => {
                        let id = self.next_command_id;
                        self.next_command_id += 1;
                        if self.command_anchors.len() >= 256 {
                            self.command_anchors.pop_front();
                        }
                        self.command_anchors.push_back((abs_row, id));
                        self.push_shell_event(ShellEvent::CommandExecuted { abs_row });
                    }
                    Some(b'D') => {
                        let exit_code = rest
                            .first()
                            .and_then(|b| std::str::from_utf8(b).ok())
                            .and_then(|s| s.split(';').next())
                            .and_then(|s| s.parse::<i32>().ok());
                        self.push_shell_event(ShellEvent::CommandFinished { abs_row, exit_code });
                    }
                    _ => {}
                }
            }
            // OSC 110 / 111 / 112: Reset dynamic fg / bg / cursor colors
            [b"110", ..] => {
                self.fg_override = None;
                self.revision += 1;
            }
            [b"111", ..] => {
                self.bg_override = None;
                self.revision += 1;
            }
            [b"112", ..] => {
                self.cursor_color_override = None;
                self.revision += 1;
            }
            // OSC 10 / 11 / 12: Query or set dynamic fg / bg / cursor colors
            [b"10", values @ ..] | [b"11", values @ ..] | [b"12", values @ ..] => {
                let base_slot = match params[0] {
                    b"10" => 10u8,
                    b"11" => 11,
                    _ => 12,
                };
                for (slot, value) in (base_slot..).zip(values.iter()) {
                    if slot > 12 {
                        break;
                    }
                    let text = std::str::from_utf8(value).unwrap_or("");
                    if text == "?" {
                        let [r, g, b] = match slot {
                            10 => self.effective_fg_default(),
                            11 => self.effective_bg_default(),
                            _ => self.effective_cursor_color(),
                        };
                        let (r, g, b) =
                            (u16::from(r) * 257, u16::from(g) * 257, u16::from(b) * 257);
                        self.responses.extend_from_slice(
                            format!("\x1b]{slot};rgb:{r:04x}/{g:04x}/{b:04x}{end}").as_bytes(),
                        );
                    } else if let Some(rgb) = parse_osc_color(text) {
                        match slot {
                            10 => self.fg_override = Some(rgb),
                            11 => self.bg_override = Some(rgb),
                            _ => self.cursor_color_override = Some(rgb),
                        }
                        self.revision += 1;
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor_state(),
            ([], b'8') => self.restore_cursor_state(),
            ([], b'D') => self.line_feed(),
            ([], b'E') => {
                self.line_feed();
                self.cursor_x = self.effective_left_margin();
            }
            ([], b'H') => {
                // HTS: Horizontal Tab Set
                if let Some(slot) = self.tab_stops.get_mut(self.cursor_x) {
                    *slot = true;
                }
            }
            ([], b'M') => {
                if self.cursor_y == self.scroll_top {
                    self.scroll_region(false, 1);
                } else {
                    self.cursor_y = self.cursor_y.saturating_sub(1);
                }
            }
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.app_keypad = true,
            ([], b'>') => self.app_keypad = false,
            (b"(", b'0') => self.charset_g0 = Charset::DecSpecialGraphics,
            (b"(", b'B') => self.charset_g0 = Charset::Ascii,
            (b")", b'0') => self.charset_g1 = Charset::DecSpecialGraphics,
            (b")", b'B') => self.charset_g1 = Charset::Ascii,
            _ => {}
        }
    }
}

impl Screen {
    fn push_shell_event(&mut self, ev: ShellEvent) {
        if self.pending_shell_events.len() >= 64 {
            self.pending_shell_events.pop_front();
        }
        self.pending_shell_events.push_back(ev);
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(s) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(s, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Terminal {
    #[cfg(test)]
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::new(columns, rows),
            apc_state: ApcScanState::Ground,
            apc_buf: Vec::new(),
            apc_overflow: false,
        }
    }

    pub fn with_theme(columns: usize, rows: usize, theme: crate::theme::Theme) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::with_theme(columns, rows, theme),
            apc_state: ApcScanState::Ground,
            apc_buf: Vec::new(),
            apc_overflow: false,
        }
    }

    pub fn apply_theme(&mut self, theme: crate::theme::Theme) {
        self.screen.apply_theme(theme);
    }

    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.screen.resize(columns, rows);
    }

    /// Process PTY output bytes, intercepting Kitty Graphics APC (`ESC _ G ... ESC \`)
    /// across arbitrary chunk boundaries with zero extra copies on the common path.
    pub fn process(&mut self, bytes: &[u8]) {
        if self.apc_state == ApcScanState::Ground && !bytes.contains(&0x1b) {
            self.parser.advance(&mut self.screen, bytes);
            return;
        }
        let mut passthrough_start = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            match self.apc_state {
                ApcScanState::Ground => {
                    if b == 0x1b {
                        if i > passthrough_start {
                            self.parser
                                .advance(&mut self.screen, &bytes[passthrough_start..i]);
                        }
                        self.apc_state = ApcScanState::SawEsc;
                        passthrough_start = i + 1;
                    }
                }
                ApcScanState::SawEsc => {
                    if b == b'_' {
                        self.apc_buf.clear();
                        self.apc_overflow = false;
                        self.apc_state = ApcScanState::InApc;
                        passthrough_start = i + 1;
                    } else {
                        // Not an APC: feed the delayed ESC and current byte to vte.
                        self.parser.advance(&mut self.screen, &[0x1b, b]);
                        self.apc_state = ApcScanState::Ground;
                        passthrough_start = i + 1;
                    }
                }
                ApcScanState::InApc => {
                    if b == 0x1b {
                        self.apc_state = ApcScanState::InApcSawEsc;
                    } else if b == 0x07 {
                        self.dispatch_apc();
                        self.apc_state = ApcScanState::Ground;
                        passthrough_start = i + 1;
                    } else if !self.apc_overflow {
                        if self.apc_buf.len() < MAX_APC_PAYLOAD_BYTES {
                            self.apc_buf.push(b);
                        } else {
                            self.apc_overflow = true;
                            self.apc_buf.clear();
                        }
                    }
                }
                ApcScanState::InApcSawEsc => {
                    if b == b'\\' {
                        self.dispatch_apc();
                        self.apc_state = ApcScanState::Ground;
                        passthrough_start = i + 1;
                    } else {
                        if !self.apc_overflow && self.apc_buf.len() + 2 <= MAX_APC_PAYLOAD_BYTES {
                            self.apc_buf.push(0x1b);
                            self.apc_buf.push(b);
                        }
                        self.apc_state = ApcScanState::InApc;
                    }
                }
            }
            i += 1;
        }
        if self.apc_state == ApcScanState::Ground && passthrough_start < bytes.len() {
            self.parser
                .advance(&mut self.screen, &bytes[passthrough_start..]);
        }
    }

    fn dispatch_apc(&mut self) {
        if self.apc_overflow {
            self.apc_overflow = false;
            return;
        }
        let buf = std::mem::take(&mut self.apc_buf);
        if let Some(body) = buf.strip_prefix(b"G") {
            self.screen.handle_kitty_apc(body);
        }
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
    fn parses_full_sgr_attributes_and_underline_styles() {
        let mut t = Terminal::new(10, 1);
        t.process(b"\x1b[1;2;3;4:3;58;2;255;0;128;9;53mA\x1b[22;23;24;29;55;59mB");
        let a = t.screen().get(0, 0).unwrap();
        assert!(a.attrs.contains(CellAttrs::BOLD));
        assert!(a.attrs.contains(CellAttrs::DIM));
        assert!(a.attrs.contains(CellAttrs::ITALIC));
        assert!(a.attrs.contains(CellAttrs::STRIKETHROUGH));
        assert!(a.attrs.contains(CellAttrs::OVERLINE));
        assert_eq!(a.underline, UnderlineStyle::Curly);
        assert_eq!(a.underline_color, Some([255, 0, 128]));

        let b = t.screen().get(1, 0).unwrap();
        assert_eq!(b.attrs.0, 0);
        assert_eq!(b.underline, UnderlineStyle::None);
        assert_eq!(b.underline_color, None);
    }

    #[test]
    fn dec_special_graphics_rep_irm_and_tabs_work() {
        let mut t = Terminal::new(20, 3);
        // DEC Special Graphics box top: l q [REP 2] k -> ┌───┐
        t.process(b"\x1b(0lq\x1b[2bk\x1b(B");
        assert!(t.screen().line(0).starts_with("┌───┐"));
        // Insert mode (IRM): insert 'X' at col 0
        t.process(b"\x1b[2;1Habcd\x1b[2;2H\x1b[4hX\x1b[4l");
        assert!(t.screen().line(1).starts_with("aXbcd"));
        // Custom tab stop at col 3
        t.process(b"\x1b[3g\x1b[3;4H\x1bH\x1b[3;1H\tZ");
        assert_eq!(t.screen().get(3, 2).unwrap().character, 'Z');
    }

    #[test]
    fn decstbm_decom_declrmm_and_sync_output_work() {
        let mut t = Terminal::new(10, 5);
        t.process(b"\x1b[2;4r\x1b[?6h\x1b[1;1H!");
        assert_eq!(t.screen().cursor_y, 1);
        assert_eq!(t.screen().get(0, 1).unwrap().character, '!');
        t.process(b"\x1b[?2026h");
        assert!(t.screen().is_synchronized_active());
        t.process(b"\x1b[?2026l");
        assert!(!t.screen().is_synchronized_active());
    }

    #[test]
    fn kitty_keyboard_stack_and_dcs_queries_work() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?u\x1b[>5u\x1b[?u\x1b[<1u\x1b[?u");
        assert_eq!(t.take_responses(), b"\x1b[?0u\x1b[?5u\x1b[?0u");

        t.process(b"\x1b[>c\x1b[18t");
        let resp = String::from_utf8(t.take_responses()).unwrap();
        assert!(resp.contains("\x1b[>1;100;0c"));
        assert!(resp.contains("\x1b[8;24;80t"));

        // XTGETTCAP for RGB and unknown
        t.process(b"\x1bP+q524742;585858\x1b\\");
        let tcap = String::from_utf8(t.take_responses()).unwrap();
        assert!(tcap.contains("\x1bP1+r524742\x1b\\"));
        assert!(tcap.contains("\x1bP0+r585858\x1b\\"));
    }

    #[test]
    fn osc_title_cwd_hyperlink_osc52_and_osc133_lifecycle() {
        let mut t = Terminal::new(40, 4);
        t.process(b"\x1b]2;My Title\x07");
        assert_eq!(t.screen_mut().take_title().as_deref(), Some("My Title"));

        t.process(b"\x1b]7;file://host/home/user/proj%201\x1b\\");
        assert_eq!(t.screen().cwd.as_deref(), Some("/home/user/proj 1"));

        t.process(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07");
        let hid = t.screen().get(0, 0).unwrap().hyperlink_id;
        assert_eq!(t.screen().hyperlink_url(hid), Some("https://example.com"));
        assert_eq!(t.screen().get(4, 0).unwrap().hyperlink_id, 0);

        // OSC 52 write works, read is blocked by default.
        t.process(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07");
        assert_eq!(
            t.screen_mut().take_clipboard_write().as_deref(),
            Some("hello")
        );
        assert!(t.take_responses().is_empty());

        // OSC 133 A/B/C/D;42
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;42\x07");
        let evs = t.screen_mut().drain_shell_events();
        assert_eq!(evs.len(), 4);
        assert!(matches!(
            evs[3],
            ShellEvent::CommandFinished {
                exit_code: Some(42),
                ..
            }
        ));
    }

    #[test]
    fn chunked_kitty_graphics_apc_across_pty_reads() {
        let mut t = Terminal::new(20, 5);
        let rgba = [255u8, 0, 0, 255];
        let seq = format!("\x1b_Ga=T,f=32,s=1,v=1,i=9;{}\x1b\\", base64_encode(&rgba));
        // Feed 1 byte at a time to test chunk boundary resilience!
        for b in seq.as_bytes() {
            t.process(&[*b]);
        }
        assert_eq!(t.take_responses(), b"\x1b_Gi=9;OK\x1b\\");
        assert_eq!(t.screen().graphics.placements.len(), 1);
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

        // Focus reporting (?1004) is now supported: 2 when reset, 1 when set;
        // unrecognized mode (?9999) returns 0.
        t.process(b"\x1b[?2004h\x1b[?1004h");
        assert_eq!(t.screen().mouse_mode(), Mode::Drag);
        assert!(t.screen().focus_reporting);
        t.process(b"\x1b[?1004$p\x1b[?2004$p\x1b[?9999$p");
        assert_eq!(
            t.take_responses(),
            b"\x1b[?1004;1$y\x1b[?2004;1$y\x1b[?9999;0$y"
        );

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
            b"\x1b[3;9R\x1b[0n\x1b[?62;22c\x1b[?25;1$y\x1b[?9999;0$y"
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

    #[test]
    fn inline_osc133_anchor_survives_scrollback_eviction() {
        let mut term = Terminal::new(40, 5);
        term.process(b"\x1b]133;A\x07prompt> ");
        assert_eq!(term.screen().latest_inline_display_row(), Some(0));
        for i in 0..20 {
            term.process(format!("\r\nline {i}").as_bytes());
        }
        assert_eq!(term.screen().latest_inline_display_row(), None);
        term.process(b"\x1b]133;A\x07prompt2> ");
        assert_eq!(term.screen().latest_inline_display_row(), Some(4));
    }
}
