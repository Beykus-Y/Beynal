// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text editor model, editing operations and view layout.
//!
//! The file is stored as a list of logical lines. The terminal screen is never
//! used as storage: the editor owns its text and only borrows pixels for
//! presentation. Cursor and selection positions use character indices; display
//! columns are computed with `unicode-width`, so wide characters and combining
//! marks are handled without splitting UTF-8 sequences.

use crate::syntax::{Language, Style};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use unicode_width::UnicodeWidthChar;

const UNDO_LIMIT: usize = 200;
const UNDO_BUDGET: usize = 32 * 1024 * 1024;
const TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Debug)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Position,
    anchor: Option<Position>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Insert,
    Backspace,
    Delete,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Insert(String),
    Backspace,
    Delete,
    Tab,
    Move {
        direction: Direction,
        extend: bool,
        by_word: bool,
        document: bool,
    },
    SelectAll,
    Undo,
    Redo,
    Save,
    Copy,
    Cut,
    PasteRequest,
    Escape,
    Enter,
    /// Cancel the unsaved-changes warning and keep editing.
    ConfirmCancel,
    /// Exit without saving.
    ConfirmDiscard,
    /// Save and exit; stays open when saving fails.
    ConfirmSave,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Changed,
    Copy(String),
    Cut(String),
    PasteRequest,
    /// Leave the editor and return to the terminal.
    Close,
    /// Close the whole window (a compositor close request was confirmed).
    CloseWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
    ino: u64,
    dev: u64,
    changed: (i64, i64),
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            mtime: meta.modified().ok(),
            len: meta.len(),
            ino: meta.ino(),
            dev: meta.dev(),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// Pixel metrics needed to map pointer coordinates onto text positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    pub cell_width: usize,
    pub cell_height: usize,
    pub padding: usize,
}

pub struct Document {
    path: PathBuf,
    target: PathBuf,
    lines: Vec<String>,
    eol: &'static str,
    bom: bool,
    cursor: Position,
    anchor: Option<Position>,
    goal_visual: Option<usize>,
    scroll: usize,
    h_scroll: usize,
    follow_cursor: bool,
    view_rows: usize,
    mouse_down: bool,
    last_click: Option<(u32, usize, f64, f64)>,
    undo: Vec<Snapshot>,
    undo_bytes: usize,
    redo: Vec<Snapshot>,
    saved_at: Option<usize>,
    last_op: Option<Op>,
    stamp: Option<Stamp>,
    // A retry may overwrite only the disk version shown in the warning.
    overwrite: Option<Option<Stamp>>,
    language: Language,
    read_only: bool,
    message: Option<String>,
    status_error: bool,
}

pub struct EditorView {
    pub rows: Vec<EditorRow>,
    pub status: String,
    pub status_error: bool,
    pub gutter_cells: usize,
    pub text_columns: usize,
    pub status_row: usize,
    /// Visible cursor as (view row, display cell column, cell width).
    pub cursor: Option<(usize, usize, usize)>,
    pub chrome: Vec<crate::bedit::Chrome>,
    pub text_offset: (usize, usize),
    pub columns: usize,
}

pub struct EditorRow {
    pub gutter: String,
    pub text: String,
    /// Selected display cells relative to the text area.
    pub selection: Option<(usize, usize)>,
    pub current: bool,
    pub styles: Vec<(std::ops::Range<usize>, Style)>,
}

impl Document {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let target = target_for(&path);
        let (lines, eol, bom, stamp, read_only) = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&target)
        {
            Ok(mut file) => {
                let read_error = |e| format!("cannot read {}: {e}", path.display());
                let meta = file.metadata().map_err(read_error)?;
                if !meta.is_file() {
                    return Err(format!(
                        "cannot read {}: not a regular file",
                        path.display()
                    ));
                }
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(read_error)?;
                let (bytes, bom) = match bytes.strip_prefix(b"\xef\xbb\xbf") {
                    Some(rest) => (rest, true),
                    None => (bytes.as_slice(), false),
                };
                let text = std::str::from_utf8(bytes)
                    .map_err(|e| format!("{}: not valid UTF-8 ({e})", path.display()))?;
                let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
                let lines = text
                    .split_inclusive('\n')
                    .map(|line| {
                        line.strip_suffix("\r\n")
                            .or_else(|| line.strip_suffix('\n'))
                            .unwrap_or(line)
                            .to_owned()
                    })
                    .chain((text.is_empty() || text.ends_with('\n')).then(String::new))
                    .collect();
                (
                    lines,
                    eol,
                    bom,
                    Some(Stamp::of(&meta)),
                    meta.permissions().mode() & 0o222 == 0,
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (vec![String::new()], "\n", false, None, false)
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let language = Language::of(&path);
        Ok(Self {
            path,
            target,
            lines,
            eol,
            bom,
            cursor: Position::default(),
            anchor: None,
            goal_visual: None,
            scroll: 0,
            h_scroll: 0,
            follow_cursor: true,
            view_rows: 10,
            mouse_down: false,
            last_click: None,
            undo: Vec::new(),
            undo_bytes: 0,
            redo: Vec::new(),
            saved_at: Some(0),
            last_op: None,
            stamp,
            overwrite: None,
            language,
            read_only,
            message: None,
            status_error: false,
        })
    }

    #[cfg(test)]
    fn from_text(path: &str, text: &str) -> Self {
        let mut editor = Self::open(path).unwrap_or_else(|_| {
            // `open` fails only for unreadable paths; tests use fresh paths.
            unreachable!("test path must be readable or missing")
        });
        editor.lines = text.split('\n').map(str::to_owned).collect();
        editor.cursor = Position::default();
        editor.anchor = None;
        editor
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn label(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    pub fn is_modified(&self) -> bool {
        self.saved_at != Some(self.undo.len())
    }

    #[cfg(test)]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn set_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(text.into());
        self.status_error = error;
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.lines.join(self.eol)
    }

    /// Ask the view to keep the cursor on screen after the next layout.
    pub fn follow_cursor(&mut self) {
        self.follow_cursor = true;
    }

    #[cfg(test)]
    pub fn cursor(&self) -> Position {
        self.cursor
    }

    pub fn view_rows(&self) -> usize {
        self.view_rows.max(1)
    }

    fn line(&self, row: usize) -> &str {
        self.lines.get(row).map_or("", String::as_str)
    }

    fn line_len(&self, row: usize) -> usize {
        self.line(row).chars().count()
    }

    fn clamp_cursor(&mut self) {
        self.cursor.row = self.cursor.row.min(self.lines.len().saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.line_len(self.cursor.row));
        if let Some(mut anchor) = self.anchor {
            let last = self.lines.len().saturating_sub(1);
            anchor.row = anchor.row.min(last);
            anchor.col = anchor.col.min(self.line_len(anchor.row));
            self.anchor = if anchor == self.cursor {
                None
            } else {
                Some(anchor)
            };
        }
    }

    fn char_width(ch: char) -> usize {
        if ch == '\t' {
            TAB_WIDTH
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        }
    }

    fn visual_col_in(line: &str, col: usize) -> usize {
        line.chars().take(col).map(Self::char_width).sum()
    }

    pub fn visual_column(&self, row: usize, col: usize) -> usize {
        Self::visual_col_in(self.line(row), col)
    }

    fn column_at_visual(&self, row: usize, target: usize) -> usize {
        let mut visual = 0;
        for (index, ch) in self.line(row).chars().enumerate() {
            let width = Self::char_width(ch);
            if visual + width > target {
                return index;
            }
            visual += width;
            if visual >= target {
                return index + 1;
            }
        }
        self.line_len(row)
    }

    fn next_col(&self, row: usize, col: usize) -> usize {
        let chars: Vec<char> = self.line(row).chars().collect();
        if col >= chars.len() {
            return col;
        }
        let mut next = col + 1;
        while next < chars.len() && Self::char_width(chars[next]) == 0 {
            next += 1;
        }
        next
    }

    fn prev_col(&self, row: usize, col: usize) -> usize {
        if col == 0 {
            return 0;
        }
        let chars: Vec<char> = self.line(row).chars().collect();
        let mut previous = col - 1;
        while previous > 0 && Self::char_width(chars[previous]) == 0 {
            previous -= 1;
        }
        previous
    }

    fn selection(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        let mut parts = Vec::new();
        for row in start.row..=end.row {
            let line = self.line(row);
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row {
                end.col
            } else {
                line.chars().count()
            };
            parts.push(chars_slice(line, from, to));
        }
        Some(parts.join("\n"))
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }

    fn snapshot_bytes(snapshot: &Snapshot) -> usize {
        64 + snapshot
            .lines
            .iter()
            .map(|line| line.len() + 24)
            .sum::<usize>()
    }

    fn record(&mut self, op: Op) {
        if self.saved_at.is_some_and(|saved| saved > self.undo.len()) {
            self.saved_at = None;
        }
        let coalesce = self.anchor.is_none()
            && self.last_op == Some(op)
            && matches!(op, Op::Insert | Op::Backspace)
            && !self.undo.is_empty();
        if !coalesce {
            let snapshot = self.snapshot();
            self.undo_bytes += Self::snapshot_bytes(&snapshot);
            self.undo.push(snapshot);
            while self.undo.len() > UNDO_LIMIT
                || (self.undo_bytes > UNDO_BUDGET && self.undo.len() > 1)
            {
                let removed = self.undo.remove(0);
                self.undo_bytes = self
                    .undo_bytes
                    .saturating_sub(Self::snapshot_bytes(&removed));
                self.saved_at = match self.saved_at {
                    Some(0) => None,
                    Some(n) => Some(n - 1),
                    None => None,
                };
            }
        }
        self.redo.clear();
        self.last_op = Some(op);
        self.overwrite = None;
        self.message = None;
        self.status_error = false;
        self.follow_cursor = true;
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
        self.clamp_cursor();
    }

    pub fn undo(&mut self) -> bool {
        self.last_op = None;
        self.overwrite = None;
        let Some(snapshot) = self.undo.pop() else {
            self.set_message("Nothing to undo", false);
            return false;
        };
        self.undo_bytes = self
            .undo_bytes
            .saturating_sub(Self::snapshot_bytes(&snapshot));
        let current = self.snapshot();
        self.redo.push(current);
        self.restore(snapshot);
        self.follow_cursor = true;
        self.set_message("Undo", false);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.last_op = None;
        self.overwrite = None;
        let Some(snapshot) = self.redo.pop() else {
            self.set_message("Nothing to redo", false);
            return false;
        };
        self.undo_bytes += Self::snapshot_bytes(&snapshot);
        self.undo.push(self.snapshot());
        self.restore(snapshot);
        self.follow_cursor = true;
        self.set_message("Redo", false);
        true
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let prefix = chars_slice(self.line(start.row), 0, start.col);
        let suffix = chars_slice(self.line(end.row), end.col, self.line_len(end.row));
        self.lines[start.row] = prefix + &suffix;
        self.lines.drain(start.row + 1..=end.row);
        self.cursor = start;
        self.anchor = None;
        true
    }

    fn insert_at_cursor(&mut self, text: &str) {
        let text = sanitize(text);
        if text.is_empty() {
            return;
        }
        let parts: Vec<&str> = text.split('\n').collect();
        let row = self.cursor.row;
        let col = self.cursor.col;
        let line = self.line(row);
        let prefix = chars_slice(line, 0, col);
        let suffix = chars_slice(line, col, self.line_len(row));
        if parts.len() == 1 {
            self.lines[row] = prefix + parts[0] + &suffix;
            self.cursor.col = col + parts[0].chars().count();
        } else {
            let mut replacement = Vec::with_capacity(parts.len());
            replacement.push(prefix + parts[0]);
            replacement.extend(parts[1..parts.len() - 1].iter().map(|s| (*s).to_owned()));
            replacement.push(parts[parts.len() - 1].to_owned() + &suffix);
            let added = replacement.len();
            self.lines.splice(row..=row, replacement);
            self.cursor = Position {
                row: row + added - 1,
                col: parts[parts.len() - 1].chars().count(),
            };
        }
        self.clamp_cursor();
    }

    fn insert(&mut self, text: &str, op: Op) -> bool {
        if text.is_empty() {
            return false;
        }
        self.record(if self.selection().is_some() {
            Op::Other
        } else {
            op
        });
        self.delete_selection();
        self.insert_at_cursor(text);
        self.follow_cursor = true;
        true
    }

    #[cfg(test)]
    pub fn insert_char(&mut self, ch: char) -> bool {
        if ch == '\n' || ch == '\r' {
            return self.enter();
        }
        if ch.is_control() && ch != '\t' {
            return false;
        }
        self.insert(&ch.to_string(), Op::Insert)
    }

    pub fn insert_text(&mut self, text: &str) -> bool {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert(&text, Op::Other)
    }

    pub fn enter(&mut self) -> bool {
        self.insert("\n", Op::Other)
    }

    pub fn tab(&mut self) -> bool {
        self.insert("\t", Op::Insert)
    }

    pub fn backspace(&mut self) -> bool {
        if self.selection().is_some() {
            self.record(Op::Other);
            self.delete_selection();
            self.follow_cursor = true;
            return true;
        }
        if self.cursor.col > 0 {
            self.record(Op::Backspace);
            let row = self.cursor.row;
            let previous = self.prev_col(row, self.cursor.col);
            let before = chars_slice(self.line(row), 0, previous);
            let after = chars_slice(self.line(row), self.cursor.col, self.line_len(row));
            self.lines[row] = before + &after;
            self.cursor.col = previous;
        } else if self.cursor.row > 0 {
            self.record(Op::Other);
            let row = self.cursor.row;
            let previous_len = self.line_len(row - 1);
            let current = self.lines.remove(row);
            self.lines[row - 1].push_str(&current);
            self.cursor = Position {
                row: row - 1,
                col: previous_len,
            };
        } else {
            return false;
        }
        self.follow_cursor = true;
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.selection().is_some() {
            self.record(Op::Other);
            self.delete_selection();
            self.follow_cursor = true;
            return true;
        }
        let row = self.cursor.row;
        if self.cursor.col < self.line_len(row) {
            self.record(Op::Delete);
            let next = self.next_col(row, self.cursor.col);
            let before = chars_slice(self.line(row), 0, self.cursor.col);
            let after = chars_slice(self.line(row), next, self.line_len(row));
            self.lines[row] = before + &after;
        } else if row + 1 < self.lines.len() {
            self.record(Op::Other);
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        } else {
            return false;
        }
        self.follow_cursor = true;
        true
    }

    pub fn select_all(&mut self) {
        self.last_op = None;
        self.anchor = Some(Position::default());
        let row = self.lines.len().saturating_sub(1);
        self.cursor = Position {
            row,
            col: self.line_len(row),
        };
        self.follow_cursor = true;
    }

    fn word_class(ch: char) -> u8 {
        if ch.is_whitespace() {
            0
        } else if ch.is_alphanumeric() || ch == '_' {
            1
        } else {
            2
        }
    }

    fn word_right(&mut self) {
        let row = self.cursor.row;
        let chars: Vec<char> = self.line(row).chars().collect();
        let mut col = self.cursor.col;
        if col >= chars.len() {
            if row + 1 < self.lines.len() {
                self.cursor = Position {
                    row: row + 1,
                    col: 0,
                };
            }
            return;
        }
        let class = Self::word_class(chars[col]);
        while col < chars.len() && Self::word_class(chars[col]) == class && class != 0 {
            col += 1;
        }
        while col < chars.len() && Self::word_class(chars[col]) == 0 {
            col += 1;
        }
        if col == self.cursor.col {
            col = self.next_col(row, col);
        }
        self.cursor.col = col;
    }

    fn word_left(&mut self) {
        let row = self.cursor.row;
        let chars: Vec<char> = self.line(row).chars().collect();
        let mut col = self.cursor.col;
        if col == 0 {
            if row > 0 {
                self.cursor = Position {
                    row: row - 1,
                    col: self.line_len(row - 1),
                };
            }
            return;
        }
        col = self.prev_col(row, col);
        while col > 0 && Self::word_class(chars[col]) == 0 {
            col = self.prev_col(row, col);
        }
        let class = Self::word_class(chars[col]);
        while col > 0 {
            let previous = self.prev_col(row, col);
            if Self::word_class(chars[previous]) != class {
                break;
            }
            col = previous;
        }
        self.cursor.col = col;
    }

    pub fn move_cursor(
        &mut self,
        direction: Direction,
        extend: bool,
        by_word: bool,
        document: bool,
    ) {
        self.last_op = None;
        let old = self.cursor;
        if !extend
            && matches!(direction, Direction::Left | Direction::Up)
            && let Some((start, _)) = self.selection()
        {
            self.cursor = start;
            self.anchor = None;
            self.follow_cursor = true;
            return;
        }
        if !extend
            && matches!(direction, Direction::Right | Direction::Down)
            && let Some((_, end)) = self.selection()
        {
            self.cursor = end;
            self.anchor = None;
            self.follow_cursor = true;
            return;
        }
        match direction {
            Direction::Left if by_word => self.word_left(),
            Direction::Right if by_word => self.word_right(),
            Direction::Left => {
                if self.cursor.col > 0 {
                    self.cursor.col = self.prev_col(self.cursor.row, self.cursor.col);
                } else if self.cursor.row > 0 {
                    self.cursor.row -= 1;
                    self.cursor.col = self.line_len(self.cursor.row);
                }
                self.goal_visual = None;
            }
            Direction::Right => {
                if self.cursor.col < self.line_len(self.cursor.row) {
                    self.cursor.col = self.next_col(self.cursor.row, self.cursor.col);
                } else if self.cursor.row + 1 < self.lines.len() {
                    self.cursor.row += 1;
                    self.cursor.col = 0;
                }
                self.goal_visual = None;
            }
            Direction::Up | Direction::Down | Direction::PageUp | Direction::PageDown => {
                let goal = self
                    .goal_visual
                    .unwrap_or_else(|| self.visual_column(self.cursor.row, self.cursor.col));
                self.goal_visual = Some(goal);
                let step = match direction {
                    Direction::Up => 1,
                    Direction::Down => 1,
                    Direction::PageUp => self.view_rows.max(1),
                    _ => self.view_rows.max(1),
                };
                let row = match direction {
                    Direction::Up | Direction::PageUp => self.cursor.row.saturating_sub(step),
                    _ => (self.cursor.row + step).min(self.lines.len().saturating_sub(1)),
                };
                self.cursor = Position {
                    row,
                    col: self.column_at_visual(row, goal),
                };
            }
            Direction::Home => {
                self.cursor = if document {
                    Position::default()
                } else {
                    Position {
                        row: self.cursor.row,
                        col: 0,
                    }
                };
                self.goal_visual = None;
            }
            Direction::End => {
                self.cursor = if document {
                    let row = self.lines.len().saturating_sub(1);
                    Position {
                        row,
                        col: self.line_len(row),
                    }
                } else {
                    Position {
                        row: self.cursor.row,
                        col: self.line_len(self.cursor.row),
                    }
                };
                self.goal_visual = None;
            }
        }
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(old);
            }
        } else {
            self.anchor = None;
        }
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
        self.follow_cursor = true;
    }

    pub fn scroll_lines(&mut self, delta: isize) -> bool {
        let max = self.lines.len().saturating_sub(self.view_rows.max(1));
        let next = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            (self.scroll + delta as usize).min(max)
        };
        self.follow_cursor = false;
        if next == self.scroll {
            return false;
        }
        self.scroll = next;
        true
    }

    pub fn mouse_press(&mut self, x: f64, y: f64, metrics: Metrics, time: u32) -> bool {
        let Some(position) = self.position_at(x, y, metrics) else {
            return false;
        };
        let double_click = self.last_click.is_some_and(|(previous, row, px, py)| {
            time.abs_diff(previous) <= 400
                && row == position.row
                && (x - px).abs() <= 4.0
                && (y - py).abs() <= 4.0
        });
        self.last_click = Some((time, position.row, x, y));
        self.goal_visual = None;
        self.last_op = None;
        self.follow_cursor = true;
        if double_click {
            let (start, end) = self.word_bounds(position.row, position.col);
            self.anchor = Some(Position {
                row: position.row,
                col: start,
            });
            self.cursor = Position {
                row: position.row,
                col: end,
            };
            self.mouse_down = false;
        } else {
            self.cursor = position;
            self.anchor = Some(position);
            self.mouse_down = true;
        }
        true
    }

    pub fn mouse_drag(&mut self, x: f64, y: f64, metrics: Metrics) -> bool {
        if !self.mouse_down {
            return false;
        }
        let Some(position) = self.position_at(x, y, metrics) else {
            return false;
        };
        if position == self.cursor {
            return false;
        }
        self.last_click = None;
        self.cursor = position;
        self.follow_cursor = false;
        true
    }

    fn word_bounds(&self, row: usize, col: usize) -> (usize, usize) {
        let chars: Vec<char> = self.line(row).chars().collect();
        if chars.is_empty() {
            return (0, 0);
        }
        let mut index = col.min(chars.len() - 1);
        while index > 0 && Self::char_width(chars[index]) == 0 {
            index -= 1;
        }
        let class = Self::word_class(chars[index]);
        let mut start = index;
        while start > 0 {
            let previous = self.prev_col(row, start);
            if Self::word_class(chars[previous]) != class {
                break;
            }
            start = previous;
        }
        let mut end = self.next_col(row, index);
        while end < chars.len() && Self::word_class(chars[end]) == class {
            end = self.next_col(row, end);
        }
        (start, end)
    }

    pub fn mouse_release(&mut self) -> bool {
        self.mouse_down = false;
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
        false
    }

    pub fn is_mouse_dragging(&self) -> bool {
        self.mouse_down
    }

    fn position_at(&self, x: f64, y: f64, metrics: Metrics) -> Option<Position> {
        if metrics.cell_width == 0 || metrics.cell_height == 0 {
            return None;
        }
        let top = metrics.padding as f64;
        if y < top {
            return None;
        }
        let row_in_view = ((y - top) / metrics.cell_height as f64) as usize;
        if row_in_view >= self.view_rows {
            return None;
        }
        let row = (self.scroll + row_in_view).min(self.lines.len().saturating_sub(1));
        let cell = ((x - metrics.padding as f64) / metrics.cell_width as f64).floor();
        let cell = if cell.is_finite() && cell > 0.0 {
            cell as usize
        } else {
            0
        };
        let gutter = self.gutter_cells();
        let col = if cell < gutter {
            0
        } else {
            let visual = self.h_scroll + cell - gutter;
            self.column_at_visual(row, visual)
        };
        Some(Position { row, col })
    }

    pub fn gutter_cells(&self) -> usize {
        self.lines.len().to_string().len().max(2).saturating_add(2)
    }

    fn status_text(&self) -> String {
        let status = format!(
            "{}  Ln {}, Col {}  {}{}  {}  |  {}",
            self.language.name(),
            self.cursor.row + 1,
            self.cursor.col + 1,
            if self.bom { "UTF-8 BOM " } else { "UTF-8 " },
            if self.eol == "\r\n" { "CRLF" } else { "LF" },
            if self.read_only {
                "Read-only"
            } else if self.stamp.is_none() {
                "New"
            } else {
                "Edit"
            },
            self.label(),
        );
        match &self.message {
            Some(message) => format!("{message}  |  {status}"),
            None => status,
        }
    }

    fn slice_visual(
        &self,
        line: &str,
        max_cells: usize,
    ) -> (String, Vec<(std::ops::Range<usize>, Style)>) {
        let skip = self.h_scroll;
        let mut visual = 0;
        let mut start = 0;
        let mut end = 0;
        let mut leading_spaces = 0;
        // Limit output from pathological runs of zero-width code points.
        let byte_budget = max_cells.saturating_mul(16).saturating_add(128);
        for (byte, ch) in line.char_indices() {
            let width = Self::char_width(ch);
            if visual < skip {
                start = byte + ch.len_utf8();
                leading_spaces = (visual + width).saturating_sub(skip);
            } else if byte - start >= byte_budget
                || (visual >= skip.saturating_add(max_cells) && width > 0)
            {
                break;
            }
            end = byte + ch.len_utf8();
            visual += width;
        }
        let styles = if start >= 8192 {
            Vec::new()
        } else {
            self.language
                .highlight(line, end)
                .into_iter()
                .filter(|(range, _)| range.end > start)
                .map(|(range, style)| {
                    let from = leading_spaces + range.start.saturating_sub(start);
                    let to = leading_spaces + range.end - start;
                    (from..to, style)
                })
                .collect()
        };
        let mut text = " ".repeat(leading_spaces);
        text.push_str(&line[start..end]);
        (text, styles)
    }

    /// Literal, case-sensitive search. Document positions remain character
    /// indices, while Rust's substring search returns UTF-8 byte offsets.
    pub fn find(&mut self, query: &str, backwards: bool) -> bool {
        if query.is_empty() || query.contains('\n') {
            return false;
        }
        let position = if backwards {
            self.selection().map_or(self.cursor, |(start, _)| start)
        } else {
            self.cursor
        };
        let line = self.line(position.row);
        let byte = byte_at(line, position.col);
        let found = if backwards {
            line[..byte]
                .rfind(query)
                .map(|i| (position.row, i))
                .or_else(|| {
                    (0..position.row)
                        .rev()
                        .find_map(|row| self.line(row).rfind(query).map(|i| (row, i)))
                })
                .or_else(|| {
                    (position.row + 1..self.lines.len())
                        .rev()
                        .find_map(|row| self.line(row).rfind(query).map(|i| (row, i)))
                })
                .or_else(|| line[byte..].rfind(query).map(|i| (position.row, byte + i)))
        } else {
            line[byte..]
                .find(query)
                .map(|i| (position.row, byte + i))
                .or_else(|| {
                    (position.row + 1..self.lines.len())
                        .find_map(|row| self.line(row).find(query).map(|i| (row, i)))
                })
                .or_else(|| {
                    (0..position.row).find_map(|row| self.line(row).find(query).map(|i| (row, i)))
                })
                .or_else(|| line[..byte].find(query).map(|i| (position.row, i)))
        };
        let Some((row, byte)) = found else {
            self.set_message("No match", false);
            return false;
        };
        let col = self.line(row)[..byte].chars().count();
        self.anchor = Some(Position { row, col });
        self.cursor = Position {
            row,
            col: col + query.chars().count(),
        };
        self.last_op = None;
        self.follow_cursor = true;
        self.message = None;
        self.status_error = false;
        true
    }

    pub fn replace_match(&mut self, query: &str, replacement: &str) {
        if query.is_empty() {
            return;
        }
        if !self.selection_is(query) && !self.find(query, false) {
            return;
        }
        self.record(Op::Other);
        self.delete_selection();
        self.insert_at_cursor(replacement);
        self.set_message("Replaced 1 match", false);
    }

    fn selection_is(&self, query: &str) -> bool {
        self.selection().is_some_and(|(start, end)| {
            if start.row != end.row {
                return false;
            }
            let line = self.line(start.row);
            &line[byte_at(line, start.col)..byte_at(line, end.col)] == query
        })
    }

    pub fn replace_all(&mut self, query: &str, replacement: &str) {
        if query.is_empty() || query == replacement {
            return;
        }
        let matches = self
            .lines
            .iter()
            .map(|line| line.matches(query).count())
            .sum::<usize>();
        if matches == 0 {
            self.set_message("No match", false);
            return;
        }
        self.record(Op::Other);
        for line in &mut self.lines {
            if line.contains(query) {
                *line = line.replace(query, replacement);
            }
        }
        self.anchor = None;
        self.clamp_cursor();
        self.set_message(format!("Replaced {matches} matches"), false);
    }

    /// Layout the visible text rows. This mutates only viewport state.
    pub fn view(&mut self, columns: usize, rows: usize) -> EditorView {
        let gutter_cells = self.gutter_cells();
        let text_columns = columns.saturating_sub(gutter_cells).max(1);
        let view_rows = rows.saturating_sub(1).max(1);
        self.view_rows = view_rows;
        let max_scroll = self.lines.len().saturating_sub(view_rows);
        self.scroll = self.scroll.min(max_scroll);
        if self.follow_cursor {
            if self.cursor.row < self.scroll {
                self.scroll = self.cursor.row;
            }
            if self.cursor.row >= self.scroll + view_rows {
                self.scroll = self.cursor.row + 1 - view_rows;
            }
            let visual = self.visual_column(self.cursor.row, self.cursor.col);
            let width = self
                .line(self.cursor.row)
                .chars()
                .nth(self.cursor.col)
                .map_or(1, |ch| Self::char_width(ch).max(1));
            if visual < self.h_scroll {
                self.h_scroll = visual;
            } else if visual + width > self.h_scroll + text_columns {
                self.h_scroll = visual + width - text_columns;
            }
            self.follow_cursor = false;
        }
        let selection = self.selection();
        let mut view_rows_out = Vec::with_capacity(view_rows);
        for offset in 0..view_rows {
            let row = self.scroll + offset;
            if row >= self.lines.len() {
                break;
            }
            let line = self.line(row);
            let (text, styles) = self.slice_visual(line, text_columns);
            let selection_cells = selection.and_then(|(start, end)| {
                if row < start.row || row > end.row {
                    return None;
                }
                let from = if row == start.row { start.col } else { 0 };
                let to = if row == end.row {
                    end.col
                } else {
                    self.line_len(row)
                };
                if to <= from {
                    return None;
                }
                let visual_start = Self::visual_col_in(line, from);
                let visual_end = Self::visual_col_in(line, to);
                let start_cell = visual_start.saturating_sub(self.h_scroll);
                let end_cell = visual_end.saturating_sub(self.h_scroll).min(text_columns);
                (end_cell > start_cell).then_some((start_cell, end_cell))
            });
            let number = row + 1;
            view_rows_out.push(EditorRow {
                gutter: format!(
                    "{:>width$}  ",
                    number,
                    width = gutter_cells.saturating_sub(2)
                ),
                text,
                selection: selection_cells,
                current: row == self.cursor.row,
                styles,
            });
        }
        let cursor = if self.cursor.row >= self.scroll && self.cursor.row < self.scroll + view_rows
        {
            let visual = self.visual_column(self.cursor.row, self.cursor.col);
            if visual >= self.h_scroll && visual < self.h_scroll + text_columns {
                let width = self
                    .line(self.cursor.row)
                    .chars()
                    .nth(self.cursor.col)
                    .map_or(1, |ch| Self::char_width(ch).max(1));
                Some((self.cursor.row - self.scroll, visual - self.h_scroll, width))
            } else {
                None
            }
        } else {
            None
        };
        EditorView {
            rows: view_rows_out,
            status: self.status_text(),
            status_error: self.status_error,
            gutter_cells,
            text_columns,
            status_row: view_rows,
            cursor,
            chrome: Vec::new(),
            text_offset: (0, 0),
            columns,
        }
    }

    pub fn save_for_close(&mut self) -> Result<(), String> {
        self.overwrite = None;
        self.save()
    }

    pub fn save(&mut self) -> Result<(), String> {
        let existing = match self.disk_metadata() {
            Ok(existing) => existing,
            Err(error) => return self.save_error(error),
        };
        let disk = existing.as_ref().map(Stamp::of);
        let read_only = existing
            .as_ref()
            .is_some_and(|meta| meta.permissions().mode() & 0o222 == 0);
        if (disk != self.stamp || read_only) && self.overwrite != Some(disk) {
            self.overwrite = Some(disk);
            let reason = if disk != self.stamp {
                "changed on disk"
            } else {
                "is read-only"
            };
            return self.save_error(format!(
                "{} {reason}; Ctrl+S again to overwrite",
                self.path.display()
            ));
        }
        let parent = self
            .target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let result = (|| -> Result<Stamp, String> {
            let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
                format!("cannot create temporary file in {}: {e}", parent.display())
            })?;
            {
                let mut writer = std::io::BufWriter::new(temp.as_file_mut());
                let mut write = || -> std::io::Result<()> {
                    if self.bom {
                        writer.write_all(b"\xef\xbb\xbf")?;
                    }
                    for (index, line) in self.lines.iter().enumerate() {
                        if index != 0 {
                            writer.write_all(self.eol.as_bytes())?;
                        }
                        writer.write_all(line.as_bytes())?;
                    }
                    writer.flush()
                };
                write().map_err(|e| format!("cannot write {}: {e}", self.path.display()))?;
            }
            let permissions = existing.as_ref().map_or_else(
                || std::fs::Permissions::from_mode(0o644),
                |meta| meta.permissions(),
            );
            temp.as_file()
                .set_permissions(permissions)
                .and_then(|()| temp.as_file().sync_all())
                .map_err(|e| format!("cannot sync {}: {e}", self.path.display()))?;
            if self.disk_metadata()?.as_ref().map(Stamp::of) != disk {
                return Err(format!(
                    "{} changed during save; try Ctrl+S again",
                    self.path.display()
                ));
            }
            let persisted = if disk.is_some() {
                temp.persist(&self.target)
            } else {
                temp.persist_noclobber(&self.target)
            }
            .map_err(|e| format!("cannot replace {}: {}", self.path.display(), e.error))?;
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
            persisted
                .metadata()
                .map(|meta| Stamp::of(&meta))
                .map_err(|error| {
                    format!("saved but cannot verify {}: {error}", self.path.display())
                })
        })();
        match result {
            Ok(stamp) => {
                self.stamp = Some(stamp);
                self.read_only = read_only;
                self.saved_at = Some(self.undo.len());
                self.last_op = None;
                self.overwrite = None;
                self.set_message("Saved", false);
                Ok(())
            }
            Err(error) => {
                self.overwrite = None;
                self.save_error(error)
            }
        }
    }

    fn disk_metadata(&self) -> Result<Option<std::fs::Metadata>, String> {
        match std::fs::symlink_metadata(&self.target) {
            Ok(meta) if meta.is_file() => Ok(Some(meta)),
            Ok(_) => Err(format!(
                "cannot save {}: not a regular file",
                self.path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("cannot inspect {}: {error}", self.path.display())),
        }
    }

    fn save_error(&mut self, message: String) -> Result<(), String> {
        self.set_message(message.clone(), true);
        Err(message)
    }

    pub fn handle(&mut self, command: Command) -> Effect {
        match command {
            Command::Insert(text) => {
                if self.insert_text(&text) {
                    Effect::Changed
                } else {
                    Effect::None
                }
            }
            Command::Backspace => changed(self.backspace()),
            Command::Delete => changed(self.delete()),
            Command::Tab => changed(self.tab()),
            Command::Move {
                direction,
                extend,
                by_word,
                document,
            } => {
                self.move_cursor(direction, extend, by_word, document);
                Effect::Changed
            }
            Command::SelectAll => {
                self.select_all();
                Effect::Changed
            }
            Command::Undo => changed(self.undo()),
            Command::Redo => changed(self.redo()),
            Command::Save => match self.save() {
                Ok(()) => Effect::Changed,
                Err(_) => Effect::Changed,
            },
            Command::Copy => match self.selection_text() {
                Some(text) if !text.is_empty() => Effect::Copy(text),
                _ => {
                    self.set_message("Nothing selected", false);
                    Effect::Changed
                }
            },
            Command::Cut => match self.selection_text() {
                Some(text) if !text.is_empty() => {
                    self.record(Op::Other);
                    self.delete_selection();
                    self.set_message("Cut", false);
                    Effect::Cut(text)
                }
                _ => {
                    self.set_message("Nothing selected", false);
                    Effect::Changed
                }
            },
            Command::PasteRequest => Effect::PasteRequest,
            Command::Enter => changed(self.enter()),
            Command::Escape
            | Command::ConfirmCancel
            | Command::ConfirmDiscard
            | Command::ConfirmSave => Effect::None,
        }
    }
}

fn changed(changed: bool) -> Effect {
    if changed {
        Effect::Changed
    } else {
        Effect::None
    }
}

fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map_or(line.len(), |(byte, _)| byte)
}

fn chars_slice(line: &str, from: usize, to: usize) -> String {
    line.chars()
        .skip(from)
        .take(to.saturating_sub(from))
        .collect()
}

fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect()
}

pub fn target_for(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if let Some((parent, name)) = path.parent().zip(path.file_name())
            && let Ok(parent) = std::fs::canonicalize(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })
        {
            return parent.join(name);
        }
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Document {
        let mut editor = Document::from_text("<test>", text);
        editor.lines = text.split('\n').map(str::to_owned).collect();
        editor
    }

    fn set_cursor(editor: &mut Document, row: usize, col: usize) {
        editor.cursor = Position { row, col };
        editor.follow_cursor = true;
    }

    #[test]
    fn edit_after_undo_invalidates_the_saved_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.txt");
        let mut doc = Document::open(&path).unwrap();
        doc.insert_text("original");
        doc.save().unwrap();
        doc.undo();
        assert!(doc.is_modified());
        doc.insert_text("new text");
        assert!(doc.is_modified());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn overwrite_confirmation_expires_on_a_second_external_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflict.txt");
        std::fs::write(&path, "initial").unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.insert_text("ours ");
        std::fs::write(&path, "theirs").unwrap();
        assert!(doc.save().is_err());
        std::fs::write(&path, "theirs once more").unwrap();
        assert!(doc.save().is_err());
        assert!(doc.is_modified());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs once more");
        doc.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ours initial");
    }

    #[test]
    fn literal_unicode_search_wraps_and_replace_all_is_one_undo() {
        let mut doc = editor("a 世界 b 世界\nслово 世界");
        assert!(doc.find("世界", false));
        assert_eq!(doc.cursor(), Position { row: 0, col: 4 });
        assert!(doc.find("世界", true));
        assert_eq!(doc.cursor(), Position { row: 1, col: 8 });
        doc.replace_all("世界", "мир");
        assert_eq!(doc.text(), "a мир b мир\nслово мир");
        assert!(doc.undo());
        assert_eq!(doc.text(), "a 世界 b 世界\nслово 世界");
    }

    #[test]
    fn large_document_layout_only_returns_visible_rows_and_styles() {
        let text = format!(
            "{}\n{}",
            "let β = 42;\n".repeat(100_000),
            "x".repeat(1_000_000)
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.rs");
        std::fs::write(&path, &text).unwrap();
        let mut doc = Document::open(&path).unwrap();
        set_cursor(&mut doc, 99_999, 0);
        let view = doc.view(70, 30);
        assert_eq!(view.rows.len(), 29);
        assert_eq!(view.rows.last().unwrap().styles.len(), 2);
        set_cursor(&mut doc, 100_001, 0);
        let view = doc.view(70, 30);
        assert!(view.rows.last().unwrap().text.len() < 70);
    }

    #[test]
    fn inserts_unicode_and_wide_characters() {
        let mut editor = editor("");
        assert!(editor.insert_text("Привет, 世界! 🚀"));
        assert_eq!(editor.text(), "Привет, 世界! 🚀");
        assert_eq!(editor.cursor(), Position { row: 0, col: 13 });
        assert_eq!(editor.visual_column(0, 13), 16);
    }

    #[test]
    fn backspace_keeps_combining_clusters_and_wide_characters_intact() {
        let mut editor = editor("a\u{0301}世");
        set_cursor(&mut editor, 0, 3);
        assert!(editor.backspace());
        assert_eq!(editor.text(), "a\u{0301}");
        assert!(editor.backspace());
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn enter_split_and_backspace_join_lines() {
        let mut editor = editor("hello world");
        set_cursor(&mut editor, 0, 5);
        assert!(editor.enter());
        assert_eq!(editor.text(), "hello\n world");
        assert!(editor.backspace());
        assert_eq!(editor.text(), "hello world");
    }

    #[test]
    fn arrow_movement_uses_visual_columns_for_wide_text() {
        let mut editor = editor("世界\nabcd");
        set_cursor(&mut editor, 0, 2);
        editor.move_cursor(Direction::Down, false, false, false);
        // Two wide cells equal four narrow cells, so the cursor lands after "abcd".
        assert_eq!(editor.cursor(), Position { row: 1, col: 4 });
        editor.move_cursor(Direction::Up, false, false, false);
        assert_eq!(editor.cursor(), Position { row: 0, col: 2 });
    }

    #[test]
    fn shift_selection_and_multiline_copy() {
        let mut editor = editor("one\ntwo\nthree");
        set_cursor(&mut editor, 0, 1);
        editor.move_cursor(Direction::Right, true, false, false);
        editor.move_cursor(Direction::Right, true, false, false);
        editor.move_cursor(Direction::Down, true, false, false);
        editor.move_cursor(Direction::Left, true, false, false);
        assert_eq!(editor.selection_text().as_deref(), Some("ne\ntw"));
        set_cursor(&mut editor, 0, 0);
        editor.select_all();
        assert_eq!(editor.selection_text().as_deref(), Some("one\ntwo\nthree"));
    }

    #[test]
    fn typing_replaces_selection() {
        let mut editor = editor("abcdef");
        set_cursor(&mut editor, 0, 1);
        editor.move_cursor(Direction::Right, true, false, false);
        editor.move_cursor(Direction::Right, true, false, false);
        assert!(editor.insert_text("X"));
        assert_eq!(editor.text(), "aXdef");
        assert!(editor.selection_text().is_none());
    }

    #[test]
    fn undo_redo_groups_typing_and_tracks_save_point() {
        let mut editor = editor("");
        assert!(!editor.is_modified());
        editor.insert_char('a');
        editor.insert_char('b');
        editor.insert_char('c');
        assert!(editor.is_modified());
        assert!(editor.undo());
        assert_eq!(editor.text(), "");
        assert!(!editor.is_modified());
        assert!(editor.redo());
        assert_eq!(editor.text(), "abc");
        assert!(editor.is_modified());
    }

    #[test]
    fn undo_after_movement_creates_separate_steps() {
        let mut editor = editor("ab");
        set_cursor(&mut editor, 0, 2);
        editor.insert_char('c');
        editor.move_cursor(Direction::Left, false, false, false);
        editor.insert_char('X');
        assert_eq!(editor.text(), "abXc");
        editor.undo();
        assert_eq!(editor.text(), "abc");
        editor.undo();
        assert_eq!(editor.text(), "ab");
    }

    #[test]
    fn undo_is_bounded_but_keeps_recent_history() {
        let mut editor = editor("");
        for _ in 0..(UNDO_LIMIT + 20) {
            editor.insert_char('x');
            editor.move_cursor(Direction::Left, false, false, false);
            editor.move_cursor(Direction::Right, false, false, false);
        }
        assert!(editor.undo.len() <= UNDO_LIMIT);
        assert!(editor.undo());
    }

    #[test]
    fn word_movement_crosses_words_and_whitespace() {
        let mut editor = editor("foo bar_baz  qux");
        set_cursor(&mut editor, 0, 0);
        editor.move_cursor(Direction::Right, false, true, false);
        assert_eq!(editor.cursor().col, 4);
        editor.move_cursor(Direction::Right, false, true, false);
        assert_eq!(editor.cursor().col, 13);
        editor.move_cursor(Direction::Left, false, true, false);
        assert_eq!(editor.cursor().col, 4);
    }

    #[test]
    fn home_end_and_document_navigation() {
        let mut editor = editor("one\ntwo");
        set_cursor(&mut editor, 1, 1);
        editor.move_cursor(Direction::Home, false, false, false);
        assert_eq!(editor.cursor().col, 0);
        editor.move_cursor(Direction::End, false, false, false);
        assert_eq!(editor.cursor().col, 3);
        editor.move_cursor(Direction::End, false, false, true);
        assert_eq!(editor.cursor(), Position { row: 1, col: 3 });
        editor.move_cursor(Direction::Home, false, false, true);
        assert_eq!(editor.cursor(), Position { row: 0, col: 0 });
    }

    #[test]
    fn view_scrolls_to_cursor_and_numbers_lines() {
        let text = (1..=100)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = editor(&text);
        set_cursor(&mut editor, 99, 0);
        let view = editor.view(40, 11);
        assert_eq!(view.rows.len(), 10);
        assert_eq!(view.rows.last().unwrap().gutter.trim(), "100");
        assert_eq!(view.cursor.unwrap().0, 9);
        assert!(view.status.contains("Ln 100, Col 1"));
        assert_eq!(view.status_row, 10);
        set_cursor(&mut editor, 0, 0);
        editor.view(40, 11);
        assert_eq!(editor.scroll, 0);
    }

    #[test]
    fn horizontal_scroll_follows_long_lines() {
        let mut editor = editor(&"x".repeat(200));
        set_cursor(&mut editor, 0, 199);
        let view = editor.view(30, 5);
        assert!(editor.h_scroll > 0);
        assert!(view.cursor.is_some());
        assert!(!view.rows[0].text.is_empty());
    }

    #[test]
    fn wheel_scroll_does_not_snap_back_to_cursor() {
        let text = (1..=50)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = editor(&text);
        editor.view(40, 11);
        assert!(editor.scroll_lines(10));
        assert_eq!(editor.scroll, 10);
        editor.view(40, 11);
        assert_eq!(editor.scroll, 10);
        assert!(editor.scroll_lines(-100));
        assert_eq!(editor.scroll, 0);
    }

    #[test]
    fn mouse_mapping_selects_and_drag_extends() {
        let mut editor = editor("abcd\nefgh");
        let metrics = Metrics {
            cell_width: 10,
            cell_height: 20,
            padding: 10,
        };
        editor.view(40, 10);
        let gutter = editor.gutter_cells();
        assert!(editor.mouse_press(10.0 + (gutter + 2) as f64 * 10.0, 10.0, metrics, 100));
        assert_eq!(editor.cursor(), Position { row: 0, col: 2 });
        assert!(editor.mouse_drag(10.0 + (gutter + 1) as f64 * 10.0, 30.0, metrics));
        assert_eq!(editor.cursor(), Position { row: 1, col: 1 });
        assert_eq!(editor.selection_text().as_deref(), Some("cd\ne"));
        editor.mouse_release();
        assert!(editor.selection_text().is_some());
    }

    #[test]
    fn double_click_selects_unicode_words_and_combining_clusters() {
        let mut editor = editor("привет a\u{0301}bc 世界");
        let metrics = Metrics {
            cell_width: 10,
            cell_height: 20,
            padding: 10,
        };
        editor.view(60, 10);
        let gutter = editor.gutter_cells();
        let x = |col: f64| 10.0 + (gutter as f64 + col) * 10.0;
        // Click the middle of "привет" twice.
        assert!(editor.mouse_press(x(2.0), 10.0, metrics, 100));
        assert!(editor.mouse_press(x(2.0), 10.0, metrics, 200));
        assert_eq!(editor.selection_text().as_deref(), Some("привет"));
        assert!(!editor.is_mouse_dragging());
        // A slow third click collapses to a caret.
        assert!(editor.mouse_press(x(4.0), 10.0, metrics, 1000));
        assert!(editor.selection_text().is_none());
        // "a" plus combining acute plus "bc" is one word.
        assert!(editor.mouse_press(x(8.0), 10.0, metrics, 1400));
        assert!(editor.mouse_press(x(8.0), 10.0, metrics, 1500));
        assert_eq!(
            editor.selection_text().as_deref(),
            Some("a\u{0301}bc"),
            "combining marks must stay inside the selected word"
        );
        // Whitespace and wide characters are selected as their own runs.
        assert!(editor.mouse_press(x(11.0), 10.0, metrics, 2000));
        assert!(editor.mouse_press(x(11.0), 10.0, metrics, 2100));
        assert_eq!(editor.selection_text().as_deref(), Some("世界"));
    }

    #[test]
    fn word_bounds_cover_unicode_words_punctuation_and_whitespace() {
        let editor = editor("foo_bar, привет 世界  x");
        assert_eq!(editor.word_bounds(0, 2), (0, 7));
        assert_eq!(editor.word_bounds(0, 7), (7, 8));
        assert_eq!(editor.word_bounds(0, 8), (8, 9));
        assert_eq!(editor.word_bounds(0, 10), (9, 15));
        assert_eq!(editor.word_bounds(0, 17), (16, 18));
        assert_eq!(editor.word_bounds(0, 19), (18, 20));
    }

    #[test]
    fn cut_and_copy_report_selected_text() {
        let mut editor = editor("alpha beta");
        set_cursor(&mut editor, 0, 6);
        editor.move_cursor(Direction::End, true, false, false);
        assert_eq!(editor.handle(Command::Copy), Effect::Copy("beta".into()));
        assert_eq!(editor.handle(Command::Cut), Effect::Cut("beta".into()));
        assert_eq!(editor.text(), "alpha ");
    }

    #[test]
    fn paste_normalizes_line_endings() {
        let mut editor = editor("");
        assert!(editor.insert_text("a\r\nb\rc"));
        assert_eq!(editor.text(), "a\nb\nc");
    }

    #[test]
    fn loads_bom_and_crlf_and_saves_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"\xef\xbb\xbfone\r\ntwo\r\n").unwrap();
        let mut editor = Document::open(&path).unwrap();
        assert_eq!(editor.text(), "one\r\ntwo\r\n");
        set_cursor(&mut editor, 2, 0);
        assert!(editor.insert_text("three"));
        editor.save().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"\xef\xbb\xbfone\r\ntwo\r\nthree");
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.txt");
        std::fs::write(&path, b"\xff\xfe").unwrap();
        let error = Document::open(&path).err().unwrap();
        assert!(error.contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn creates_missing_file_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let mut editor = Document::open(&path).unwrap();
        editor.insert_text("hello");
        editor.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!editor.is_modified());
    }

    #[test]
    fn external_change_requires_second_save_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let mut editor = Document::open(&path).unwrap();
        editor.insert_text("edited");
        std::fs::write(&path, "external").unwrap();
        let error = editor.save().unwrap_err();
        assert!(error.contains("changed on disk"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
        editor.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "editedoriginal");
    }

    #[test]
    fn read_only_file_requires_second_save() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly.txt");
        std::fs::write(&path, "original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let mut editor = Document::open(&path).unwrap();
        editor.insert_text("new");
        let error = editor.save().unwrap_err();
        assert!(error.contains("read-only"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        editor.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "neworiginal");
    }

    #[test]
    fn save_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "echo hi\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut editor = Document::open(&path).unwrap();
        editor.insert_text("# changed\n");
        editor.save().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn save_failure_does_not_touch_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let mut editor = Document::open(&path).unwrap();
        editor.insert_text("new");
        // Replacing a file inside a non-existent parent fails after loading.
        editor.target = dir.path().join("missing").join("file.txt");
        assert!(editor.save().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn saving_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = Document::open(dir.path()).err().unwrap();
        assert!(error.contains("cannot read"), "{error}");
        let mut editor = editor("x");
        editor.path = dir.path().to_path_buf();
        editor.target = dir.path().to_path_buf();
        editor.stamp = None;
        assert!(editor.save().unwrap_err().contains("not a regular file"));
    }
}
