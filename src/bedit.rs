// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    editor::{Command, Document, EditorView, Effect, Metrics},
    project::Project,
};
use std::{
    collections::{HashSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, PartialEq, Eq)]
pub struct OpenRequest {
    pub root: Option<PathBuf>,
    pub files: Vec<PathBuf>,
}

impl OpenRequest {
    pub fn from_paths(paths: &[PathBuf]) -> io::Result<Self> {
        let mut request = Self {
            root: None,
            files: Vec::new(),
        };
        for path in paths {
            let path = std::path::absolute(path)?;
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    if request.root.is_some() {
                        return Err(io::Error::other(
                            "specify one project folder per invocation",
                        ));
                    }
                    request.root = Some(fs::canonicalize(path)?);
                }
                Ok(_) => request.files.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => request.files.push(path),
                Err(error) => return Err(error),
            }
        }
        if paths.is_empty() {
            request.root = Some(std::env::current_dir()?);
        }
        Ok(request)
    }

    fn initial_root(&self) -> io::Result<PathBuf> {
        if let Some(root) = &self.root {
            return Ok(root.clone());
        }
        let cwd = std::env::current_dir()?;
        let parent = self.files.first().and_then(|path| path.parent());
        Ok(parent
            .filter(|path| !path.starts_with(&cwd))
            .and_then(|path| fs::canonicalize(path).ok())
            .unwrap_or(cwd))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Panel,
    Active,
    Muted,
    Accent,
}

pub struct Chrome {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub text: String,
    pub tone: Tone,
}

struct Tab {
    id: u64,
    doc: Document,
}

enum Opened {
    Existing(u64),
    Loaded(Box<Document>),
}

struct LoadedRequest {
    root: Option<PathBuf>,
    items: Vec<Opened>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NextTab(bool),
    CloseTab,
    ToggleExplorer,
    QuickOpen,
    Find,
    Replace,
    Refresh,
    Leave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Close {
    Tab(usize),
    Editor,
    Window,
}

struct Confirmation {
    close: Close,
    remaining: VecDeque<usize>,
    original: Option<usize>,
}

#[derive(Default)]
enum Panel {
    #[default]
    None,
    Quick {
        query: String,
        selected: usize,
        results: Vec<(PathBuf, String)>,
        all: bool,
    },
    Search {
        query: String,
        replacement: String,
        replace: bool,
        field: bool,
        all: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Hit {
    Explorer,
    Quick,
    Search,
    Tab(usize),
    CloseTab(usize),
    TabsBack,
    TabsForward,
    Entry(PathBuf, bool),
    Result(PathBuf),
    Field(bool),
    Find,
    Replace,
    ReplaceAll,
    ConfirmSave,
    ConfirmDiscard,
    ConfirmCancel,
}

struct Region {
    row: usize,
    col: usize,
    width: usize,
    hit: Hit,
}

struct Layout {
    columns: usize,
    rows: usize,
    left: usize,
    top: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteTarget {
    tab: Option<u64>,
    focus: u64,
}

fn unique_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct BEdit {
    project: Project,
    tabs: Vec<Tab>,
    active: Option<usize>,
    tab_start: usize,
    reveal_tab: bool,
    explorer: bool,
    expanded: HashSet<PathBuf>,
    explorer_scroll: usize,
    explorer_total: usize,
    panel: Panel,
    confirmation: Option<Confirmation>,
    notice: Option<(String, bool)>,
    focus: u64,
    hits: Vec<Region>,
    layout: Layout,
    open_queue: VecDeque<OpenRequest>,
    open_job: Option<mpsc::Receiver<Result<LoadedRequest, String>>>,
}

impl BEdit {
    pub fn open(request: OpenRequest) -> Result<Self, String> {
        let mut editor = Self::new(&request)?;
        let opened = Self::load(request, &[])?;
        editor.apply_open(opened);
        Ok(editor)
    }

    pub fn new(request: &OpenRequest) -> Result<Self, String> {
        let root = request.initial_root().map_err(|e| e.to_string())?;
        Ok(Self {
            project: Project::new(root),
            tabs: Vec::new(),
            active: None,
            tab_start: 0,
            reveal_tab: true,
            explorer: true,
            expanded: HashSet::new(),
            explorer_scroll: 0,
            explorer_total: 0,
            panel: Panel::None,
            confirmation: None,
            notice: None,
            focus: unique_id(),
            hits: Vec::new(),
            layout: Layout {
                columns: 1,
                rows: 1,
                left: 0,
                top: 0,
            },
            open_queue: VecDeque::new(),
            open_job: None,
        })
    }

    pub fn title(&self) -> String {
        format!("BEdit — {}", self.project_name())
    }

    fn project_name(&self) -> String {
        self.project
            .root
            .file_name()
            .unwrap_or(self.project.root.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    pub fn open_request(&mut self, request: OpenRequest) {
        self.open_queue.push_back(request);
        self.load_next();
    }

    fn load_next(&mut self) {
        while self.open_job.is_none() {
            let Some(request) = self.open_queue.pop_front() else {
                return;
            };
            if request.files.is_empty() {
                self.apply_open(LoadedRequest {
                    root: request.root,
                    items: Vec::new(),
                });
                continue;
            }
            let known: Vec<_> = self
                .tabs
                .iter()
                .map(|tab| {
                    (
                        tab.id,
                        tab.doc.path().to_path_buf(),
                        tab.doc.target().to_path_buf(),
                    )
                })
                .collect();
            let (tx, rx) = mpsc::channel();
            match std::thread::Builder::new()
                .name("bedit-open".into())
                .spawn(move || {
                    let _ = tx.send(Self::load(request, &known));
                }) {
                Ok(_) => self.open_job = Some(rx),
                Err(error) => self.set_message(format!("Cannot start file loader: {error}"), true),
            }
        }
    }

    // Load all files before applying a batch. The worker never sees an
    // editable buffer, only paths and stable IDs for already open tabs.
    fn load(
        request: OpenRequest,
        known: &[(u64, PathBuf, PathBuf)],
    ) -> Result<LoadedRequest, String> {
        let mut items = Vec::new();
        for path in request.files {
            let target = crate::editor::target_for(&path);
            if let Some((id, _, _)) = known
                .iter()
                .find(|(_, existing, canonical)| *existing == path || *canonical == target)
            {
                items.push(Opened::Existing(*id));
            } else {
                items.push(Opened::Loaded(Box::new(Document::open(path)?)));
            }
        }
        Ok(LoadedRequest {
            root: request.root,
            items,
        })
    }

    fn apply_open(&mut self, opened: LoadedRequest) {
        let mut selection = None;
        for item in opened.items {
            let index = match item {
                Opened::Existing(id) => self.tabs.iter().position(|tab| tab.id == id),
                Opened::Loaded(doc) => {
                    let existing = self
                        .tabs
                        .iter()
                        .position(|tab| tab.doc.target() == doc.target());
                    Some(existing.unwrap_or_else(|| {
                        let index = self.tabs.len();
                        self.tabs.push(Tab {
                            id: unique_id(),
                            doc: *doc,
                        });
                        index
                    }))
                }
            };
            selection = index.or(selection);
        }
        if let Some(root) = opened.root.filter(|root| *root != self.project.root) {
            self.project = Project::new(root);
            self.expanded.clear();
            self.explorer_scroll = 0;
            self.explorer_total = 0;
            self.hits.clear();
            self.update_quick_results();
        }
        if self.confirmation.is_none() {
            if let Some(index) = selection {
                self.activate(index);
            }
            self.notice = None;
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        self.open_request(OpenRequest {
            root: None,
            files: vec![path],
        });
        self.set_panel(Panel::None);
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = match self.project.poll() {
            Some(Ok(())) => {
                self.update_quick_results();
                true
            }
            Some(Err(error)) => {
                self.set_message(format!("Cannot scan project: {error}"), true);
                true
            }
            None => false,
        };
        if let Some(job) = &self.open_job {
            let result = match job.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return changed,
                Err(mpsc::TryRecvError::Disconnected) => Err("File loader stopped".into()),
            };
            self.open_job = None;
            match result {
                Ok(opened) => self.apply_open(opened),
                Err(error) => self.set_message(error, true),
            }
            self.load_next();
            changed = true;
        }
        changed
    }

    fn document(&self) -> Option<&Document> {
        Some(&self.tabs.get(self.active?)?.doc)
    }

    fn document_mut(&mut self) -> Option<&mut Document> {
        Some(&mut self.tabs.get_mut(self.active?)?.doc)
    }

    fn activate(&mut self, index: usize) {
        self.mouse_release();
        self.active = Some(index);
        self.hits.clear();
        self.reveal_tab = true;
        self.focus = unique_id();
    }

    fn set_panel(&mut self, panel: Panel) {
        self.panel = panel;
        self.update_quick_results();
        self.hits.clear();
        self.focus = unique_id();
    }

    pub fn set_message(&mut self, message: impl Into<String>, error: bool) {
        self.notice = Some((message.into(), error));
    }

    pub fn selection_text(&self) -> Option<String> {
        self.document()?.selection_text()
    }

    pub fn follow_cursor(&mut self) {
        if let Some(doc) = self.document_mut() {
            doc.follow_cursor();
        }
    }

    pub fn is_confirming_close(&self) -> bool {
        self.confirmation.is_some()
    }

    fn request_close(&mut self, close: Close) -> Effect {
        if self.confirmation.is_some() {
            return Effect::Changed;
        }
        let mut remaining: VecDeque<_> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(index, tab)| {
                tab.doc.is_modified()
                    && match close {
                        Close::Tab(tab) => *index == tab,
                        Close::Editor | Close::Window => true,
                    }
            })
            .map(|(index, _)| index)
            .collect();
        if let Some(next) = remaining.pop_front() {
            let original = self.active;
            self.set_panel(Panel::None);
            self.activate(next);
            self.confirmation = Some(Confirmation {
                close,
                remaining,
                original,
            });
            Effect::Changed
        } else {
            self.finish_close(close)
        }
    }

    pub fn request_window_close(&mut self) -> bool {
        self.request_close(Close::Window) == Effect::CloseWindow
    }

    fn finish_close(&mut self, close: Close) -> Effect {
        match close {
            Close::Editor => Effect::Close,
            Close::Window => Effect::CloseWindow,
            Close::Tab(index) => {
                self.hits.clear();
                self.tabs.remove(index);
                match self.active.filter(|_| !self.tabs.is_empty()) {
                    Some(active) => self.activate(if active > index {
                        active - 1
                    } else {
                        active.min(self.tabs.len() - 1)
                    }),
                    None => self.active = None,
                }
                self.set_panel(Panel::None);
                Effect::Changed
            }
        }
    }

    fn confirm(&mut self, command: Command) -> Effect {
        let mut confirmation = self.confirmation.take().unwrap();
        match command {
            Command::ConfirmCancel | Command::Escape => {
                if let Some(index) = confirmation.original {
                    self.activate(index);
                }
                return Effect::Changed;
            }
            Command::ConfirmSave => {
                if self.document_mut().unwrap().save_for_close().is_err() {
                    // Keep all buffers and abandon the close transaction.
                    return Effect::Changed;
                }
            }
            Command::ConfirmDiscard => {}
            _ => {
                self.confirmation = Some(confirmation);
                return Effect::None;
            }
        }
        if let Some(next) = confirmation.remaining.pop_front() {
            self.activate(next);
            self.confirmation = Some(confirmation);
            Effect::Changed
        } else {
            self.finish_close(confirmation.close)
        }
    }

    pub fn action(&mut self, action: Action) -> Effect {
        if self.confirmation.is_some() {
            return Effect::None;
        }
        self.notice = None;
        match action {
            Action::NextTab(reverse) => {
                if let Some(index) = self.active {
                    let count = self.tabs.len();
                    self.activate((index + if reverse { count - 1 } else { 1 }) % count);
                    self.set_panel(Panel::None);
                }
            }
            Action::CloseTab => {
                if let Some(index) = self.active {
                    return self.request_close(Close::Tab(index));
                }
            }
            Action::ToggleExplorer => self.explorer = !self.explorer,
            Action::QuickOpen => self.set_panel(Panel::Quick {
                query: String::new(),
                selected: 0,
                results: Vec::new(),
                all: false,
            }),
            Action::Find | Action::Replace if self.active.is_some() => {
                let query = self
                    .selection_text()
                    .filter(|text| !text.contains('\n') && text.len() <= 1024)
                    .unwrap_or_default();
                self.set_panel(Panel::Search {
                    all: !query.is_empty(),
                    query,
                    replacement: String::new(),
                    replace: action == Action::Replace,
                    field: false,
                });
            }
            Action::Refresh => self.project.refresh(),
            Action::Leave => return self.request_close(Close::Editor),
            _ => {}
        }
        Effect::Changed
    }

    pub fn handle(&mut self, command: Command) -> Effect {
        if self.confirmation.is_some() {
            return self.confirm(command);
        }
        self.notice = None;
        if !matches!(self.panel, Panel::None) {
            return self.handle_panel(command);
        }
        if command == Command::Escape {
            return self.request_close(Close::Editor);
        }
        self.document_mut()
            .map_or(Effect::None, |doc| doc.handle(command))
    }

    pub fn panel_enter(&mut self, shift: bool, control: bool) -> Option<Effect> {
        if self.confirmation.is_some() || matches!(self.panel, Panel::None) {
            return None;
        }
        self.notice = None;
        self.run_panel(shift, control);
        Some(Effect::Changed)
    }

    fn run_panel(&mut self, shift: bool, control: bool) {
        match &self.panel {
            Panel::Quick {
                results, selected, ..
            } => {
                if let Some((path, _)) = results.get(*selected) {
                    self.open_path(path.clone());
                }
            }
            Panel::Search {
                query,
                replacement,
                field,
                replace,
                ..
            } => {
                let (query, replacement) = (query.clone(), replacement.clone());
                let replace_match = *replace && *field;
                let replace_all = *replace && control;
                if let Some(doc) = self.document_mut() {
                    if replace_all {
                        doc.replace_all(&query, &replacement);
                    } else if replace_match {
                        doc.replace_match(&query, &replacement);
                    } else {
                        doc.find(&query, shift);
                    }
                }
            }
            Panel::None => {}
        }
    }

    fn handle_panel(&mut self, command: Command) -> Effect {
        match command {
            Command::Escape => self.set_panel(Panel::None),
            Command::Enter => self.run_panel(false, false),
            Command::Tab => {
                if let Panel::Search {
                    field,
                    replace: true,
                    all,
                    ..
                } = &mut self.panel
                {
                    *field = !*field;
                    *all = false;
                    self.focus = unique_id();
                }
            }
            Command::Move {
                direction: crate::editor::Direction::Up | crate::editor::Direction::Down,
                ..
            } => {
                if let Panel::Quick {
                    results, selected, ..
                } = &mut self.panel
                {
                    *selected = if matches!(
                        command,
                        Command::Move {
                            direction: crate::editor::Direction::Up,
                            ..
                        }
                    ) {
                        selected.saturating_sub(1)
                    } else {
                        (*selected + 1).min(results.len().saturating_sub(1))
                    };
                }
            }
            Command::Insert(text) => self.insert_field(&text),
            Command::Backspace | Command::Delete => {
                if let Some((value, all)) = self.field_mut() {
                    if *all {
                        value.clear();
                        *all = false;
                    } else if command == Command::Backspace {
                        value.pop();
                    }
                }
                self.update_quick_results();
            }
            Command::SelectAll => {
                if let Some((_, all)) = self.field_mut() {
                    *all = true;
                }
            }
            Command::Copy | Command::Cut => {
                if let Some((value, true)) = self.field_mut() {
                    if command == Command::Copy {
                        return Effect::Copy(value.clone());
                    }
                    let text = std::mem::take(value);
                    self.update_quick_results();
                    return Effect::Cut(text);
                }
            }
            Command::PasteRequest => return Effect::PasteRequest,
            Command::Save => {
                if let Some(doc) = self.document_mut() {
                    let _ = doc.save();
                }
            }
            _ => return Effect::None,
        }
        Effect::Changed
    }

    fn field_mut(&mut self) -> Option<(&mut String, &mut bool)> {
        match &mut self.panel {
            Panel::Quick { query, all, .. } => Some((query, all)),
            Panel::Search {
                query,
                replacement,
                field,
                all,
                ..
            } => Some((if *field { replacement } else { query }, all)),
            Panel::None => None,
        }
    }

    fn insert_field(&mut self, text: &str) {
        if let Some((value, all)) = self.field_mut() {
            if *all {
                value.clear();
                *all = false;
            }
            let available = 1024usize.saturating_sub(value.len());
            let text = &text[..text.floor_char_boundary(text.len().min(available))];
            value.extend(text.chars().filter(|ch| !ch.is_control()));
            self.update_quick_results();
        }
    }

    fn update_quick_results(&mut self) {
        if let Panel::Quick {
            query,
            results,
            selected,
            ..
        } = &mut self.panel
        {
            *results = self
                .project
                .index
                .as_ref()
                .map_or_else(Vec::new, |index| index.find(query, 128));
            *selected = 0;
            self.hits.clear();
        }
    }

    pub fn paste_target(&self) -> PasteTarget {
        PasteTarget {
            tab: self.active.map(|index| self.tabs[index].id),
            focus: self.focus,
        }
    }

    pub fn paste(&mut self, target: PasteTarget, text: &str) -> bool {
        if target != self.paste_target() || self.confirmation.is_some() {
            return false;
        }
        if matches!(self.panel, Panel::None) {
            self.document_mut().is_some_and(|doc| doc.insert_text(text))
        } else {
            self.insert_field(text);
            true
        }
    }
}

#[derive(Default)]
struct ChromeView {
    items: Vec<Chrome>,
    hits: Vec<Region>,
}

impl ChromeView {
    fn item(
        &mut self,
        row: usize,
        col: usize,
        width: usize,
        text: &str,
        tone: Tone,
        hit: Option<Hit>,
    ) {
        if width == 0 {
            return;
        }
        self.items.push(Chrome {
            row,
            col,
            width,
            text: fitted(text, width),
            tone,
        });
        if let Some(hit) = hit {
            self.hits.push(Region {
                row,
                col,
                width,
                hit,
            });
        }
    }
}

fn fitted(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        used += ch.width().unwrap_or(0);
        if used > width {
            return out;
        }
        out.push(ch);
    }
    out
}

fn field_text(label: &str, input: &str, active: bool, all: bool, width: usize) -> String {
    let caret = if active && !all { "│" } else { "" };
    let space = width.saturating_sub(label.width() + caret.width());
    let start = input
        .char_indices()
        .rev()
        .scan(0usize, |used, (byte, ch)| {
            *used += ch.width().unwrap_or(0);
            (*used <= space).then_some(byte)
        })
        .last()
        .unwrap_or(input.len());
    format!("{label}{}{caret}", &input[start..])
}

impl BEdit {
    pub fn view(&mut self, columns: usize, rows: usize) -> EditorView {
        let mut chrome = ChromeView::default();
        let header_rows = rows.saturating_sub(2).min(2);
        let sidebar = if self.explorer && columns >= 40 && header_rows == 2 {
            (columns / 4).clamp(16, 30)
        } else {
            0
        };
        let body_width = columns - sidebar;
        let panel_rows = self.layout_panel(
            &mut chrome,
            sidebar,
            header_rows,
            body_width,
            rows.saturating_sub(header_rows + 2),
        );
        let text_top = header_rows + panel_rows;

        if header_rows > 0 {
            self.layout_header(&mut chrome, columns);
        }
        if header_rows > 1 {
            self.layout_tabs(&mut chrome, sidebar, body_width);
        }
        if sidebar > 0 {
            self.layout_explorer(&mut chrome, sidebar, rows);
        }

        let mut view = if let Some(doc) = self.document_mut() {
            doc.view(body_width, rows.saturating_sub(text_top))
        } else {
            if panel_rows == 0 && text_top + 1 < rows {
                chrome.item(
                    text_top,
                    sidebar,
                    body_width,
                    "  Open a project file  ·  Ctrl+P",
                    Tone::Muted,
                    None,
                );
            }
            EditorView {
                rows: Vec::new(),
                status: "Ctrl+P Open  |  Ctrl+B Explorer".into(),
                status_error: false,
                gutter_cells: 0,
                text_columns: body_width,
                status_row: 0,
                cursor: None,
                chrome: Vec::new(),
                text_offset: (0, 0),
                columns,
            }
        };
        if let Some((message, error)) = &self.notice {
            view.status = format!("{message}  |  {}", view.status);
            view.status_error = *error;
        }
        if self.open_job.is_some() && self.notice.is_none() {
            view.status = format!("Opening…  |  {}", view.status);
        }
        if self.confirmation.is_some() {
            let label = self.document().map(Document::label).unwrap_or_default();
            view.status = format!("Unsaved · {label}   |   Y Save  N Discard  Esc Cancel");
            view.status_error = true;
        }
        if self.confirmation.is_some() || !matches!(self.panel, Panel::None) {
            view.cursor = None;
        }
        view.text_offset = (sidebar, text_top);
        view.status_row = rows.saturating_sub(1);
        view.columns = columns;
        view.chrome = chrome.items;
        self.hits = chrome.hits;
        self.layout = Layout {
            columns,
            rows,
            left: sidebar,
            top: text_top,
        };
        view
    }

    fn layout_header(&self, chrome: &mut ChromeView, columns: usize) {
        let explorer = columns.min(12);
        let actions = if columns >= 54 { 14 } else { 0 };
        chrome.item(
            0,
            0,
            explorer,
            " Explorer",
            if self.explorer {
                Tone::Active
            } else {
                Tone::Panel
            },
            Some(Hit::Explorer),
        );
        chrome.item(
            0,
            explorer,
            columns - explorer - actions,
            &format!("  BEdit  /  {}", self.project_name()),
            Tone::Panel,
            None,
        );
        if actions > 0 {
            chrome.item(
                0,
                columns - actions,
                7,
                " Open",
                Tone::Accent,
                Some(Hit::Quick),
            );
            chrome.item(0, columns - 7, 7, " Find", Tone::Accent, Some(Hit::Search));
        }
    }

    fn layout_tabs(&mut self, chrome: &mut ChromeView, left: usize, width: usize) {
        chrome.item(1, left, width, "", Tone::Panel, None);
        if self.tabs.is_empty() || width < 4 {
            return;
        }
        let max_slots = (width / 16).max(1);
        let overflow = self.tabs.len() > max_slots;
        let arrows = usize::from(overflow) * 2;
        let slots = ((width - 2 * arrows) / 16).max(1).min(self.tabs.len());
        let max_start = self.tabs.len() - slots;
        if self.reveal_tab
            && let Some(index) = self.active
        {
            if index < self.tab_start {
                self.tab_start = index;
            } else if index >= self.tab_start + slots {
                self.tab_start = index + 1 - slots;
            }
        }
        self.reveal_tab = false;
        self.tab_start = self.tab_start.min(max_start);
        if overflow {
            chrome.item(1, left, arrows, "‹", Tone::Accent, Some(Hit::TabsBack));
            chrome.item(
                1,
                left + width - arrows,
                arrows,
                "›",
                Tone::Accent,
                Some(Hit::TabsForward),
            );
        }
        let tab_width = ((width - 2 * arrows) / slots).min(25);
        if tab_width < 4 {
            return;
        }
        for (slot, tab) in self
            .tabs
            .iter()
            .skip(self.tab_start)
            .take(slots)
            .enumerate()
        {
            let index = self.tab_start + slot;
            let col = left + arrows + slot * tab_width;
            let tone = if self.active == Some(index) {
                Tone::Active
            } else {
                Tone::Panel
            };
            let label = tab.doc.label();
            let marker = if tab.doc.is_modified() { "●" } else { " " };
            chrome.item(
                1,
                col,
                tab_width - 2,
                &format!("{marker} {label}"),
                tone,
                Some(Hit::Tab(index)),
            );
            chrome.item(
                1,
                col + tab_width - 2,
                2,
                "×",
                tone,
                Some(Hit::CloseTab(index)),
            );
        }
    }

    fn layout_explorer(&mut self, chrome: &mut ChromeView, width: usize, rows: usize) {
        let entries_start = 2;
        let visible = rows.saturating_sub(entries_start + 1);
        let title = if self.project.loading() {
            "  Scanning…"
        } else if self
            .project
            .index
            .as_ref()
            .is_some_and(|index| index.limited)
        {
            "  FILES (partial)"
        } else {
            "  FILES"
        };
        chrome.item(1, 0, width, title, Tone::Muted, None);
        for row in entries_start..rows.saturating_sub(1) {
            chrome.item(row, 0, width, "", Tone::Muted, None);
        }
        let Some(index) = &self.project.index else {
            return;
        };
        let children = |path: &Path| index.children.get(path).into_iter().flatten();
        let mut levels = vec![children(&self.project.root)];
        let mut tree = Vec::new();
        while let Some(level) = levels.last_mut() {
            if let Some(entry) = level.next() {
                tree.push((levels.len() - 1, entry));
                if entry.directory && self.expanded.contains(&entry.path) {
                    levels.push(children(&entry.path));
                }
            } else {
                levels.pop();
            }
        }
        self.explorer_total = tree.len();
        self.explorer_scroll = self.explorer_scroll.min(tree.len().saturating_sub(visible));
        let active = self.document().map(Document::path);
        for (offset, (depth, entry)) in tree
            .into_iter()
            .skip(self.explorer_scroll)
            .take(visible)
            .enumerate()
        {
            let prefix = if !entry.directory {
                "  "
            } else if self.expanded.contains(&entry.path) {
                "▾ "
            } else {
                "▸ "
            };
            let indent = " ".repeat((2 + depth * 2).min(width.saturating_sub(5)));
            let tone = if active == Some(entry.path.as_path()) {
                Tone::Active
            } else {
                Tone::Muted
            };
            chrome.item(
                entries_start + offset,
                0,
                width,
                &format!("{indent}{prefix}{}", entry.name),
                tone,
                Some(Hit::Entry(entry.path.clone(), entry.directory)),
            );
        }
    }

    fn layout_panel(
        &mut self,
        chrome: &mut ChromeView,
        left: usize,
        row: usize,
        width: usize,
        available: usize,
    ) -> usize {
        if available == 0 {
            return 0;
        }
        if self.confirmation.is_some() {
            let buttons = [
                (8, " Y Save", Hit::ConfirmSave),
                (11, " N Discard", Hit::ConfirmDiscard),
                (12, " Esc Cancel", Hit::ConfirmCancel),
            ];
            let mut col = left;
            for (size, text, hit) in buttons {
                let size = size.min(left + width - col);
                chrome.item(row, col, size, text, Tone::Accent, Some(hit));
                col += size;
            }
            return 1;
        }
        match &self.panel {
            Panel::None => 0,
            Panel::Quick {
                query,
                selected,
                all,
                results,
            } => {
                let shown = available.saturating_sub(1).min(8).min(results.len());
                let selected = (*selected).min(results.len().saturating_sub(1));
                let first = (selected + 1).saturating_sub(shown);
                let tone = if *all { Tone::Active } else { Tone::Accent };
                let label = if self.project.loading() {
                    " Open (scanning)  "
                } else {
                    " Open  "
                };
                chrome.item(
                    row,
                    left,
                    width,
                    &field_text(label, query, true, *all, width),
                    tone,
                    None,
                );
                for (slot, (path, name)) in results.iter().enumerate().skip(first).take(shown) {
                    let tone = if slot == selected {
                        Tone::Active
                    } else {
                        Tone::Panel
                    };
                    chrome.item(
                        row + 1 + slot - first,
                        left,
                        width,
                        &format!("  {name}"),
                        tone,
                        Some(Hit::Result(path.clone())),
                    );
                }
                if shown == 0 && available > 1 && !self.project.loading() {
                    chrome.item(
                        row + 1,
                        left,
                        width,
                        "  No matching files",
                        Tone::Muted,
                        None,
                    );
                    return 2;
                }
                1 + shown
            }
            Panel::Search {
                query,
                replacement,
                replace,
                field,
                all,
            } => {
                let row_count = if *replace && available > 1 { 2 } else { 1 };
                let display_replacement = *replace && *field && row_count == 1;
                let first_action = if display_replacement {
                    (" Replace", Hit::Replace)
                } else {
                    (" Next", Hit::Find)
                };
                let first_input = if display_replacement {
                    replacement
                } else {
                    query
                };
                let input_width = if width >= 19 { width - 8 } else { width };
                chrome.item(
                    row,
                    left,
                    input_width,
                    &field_text(
                        if display_replacement {
                            " Replace  "
                        } else {
                            " Find  "
                        },
                        first_input,
                        !*field || display_replacement,
                        *all && (!*field || display_replacement),
                        input_width,
                    ),
                    if !*field || display_replacement {
                        Tone::Active
                    } else {
                        Tone::Panel
                    },
                    Some(Hit::Field(display_replacement)),
                );
                chrome.item(
                    row,
                    left + input_width,
                    width - input_width,
                    first_action.0,
                    Tone::Accent,
                    Some(first_action.1),
                );
                if row_count == 2 {
                    let input_width = if width >= 28 { width - 16 } else { width };
                    let button_width = (width - input_width) / 2;
                    chrome.item(
                        row + 1,
                        left,
                        input_width,
                        &field_text(
                            " Replace  ",
                            replacement,
                            *field,
                            *all && *field,
                            input_width,
                        ),
                        if *field { Tone::Active } else { Tone::Panel },
                        Some(Hit::Field(true)),
                    );
                    chrome.item(
                        row + 1,
                        left + input_width,
                        button_width,
                        " Replace",
                        Tone::Accent,
                        Some(Hit::Replace),
                    );
                    chrome.item(
                        row + 1,
                        left + input_width + button_width,
                        button_width,
                        " All",
                        Tone::Accent,
                        Some(Hit::ReplaceAll),
                    );
                }
                row_count
            }
        }
    }

    pub fn mouse_press(&mut self, x: f64, y: f64, metrics: Metrics, time: u32) {
        let Some((col, row)) = self.cell_at(x, y, metrics) else {
            return;
        };
        if let Some(hit) = self
            .hits
            .iter()
            .find(|item| item.row == row && col >= item.col && col < item.col + item.width)
            .map(|item| item.hit.clone())
        {
            if self.confirmation.is_some() {
                return;
            }
            match hit {
                Hit::Explorer => {
                    self.action(Action::ToggleExplorer);
                }
                Hit::Quick => {
                    self.action(Action::QuickOpen);
                }
                Hit::Search => {
                    self.action(Action::Find);
                }
                Hit::Tab(index) => {
                    self.activate(index);
                    self.set_panel(Panel::None);
                }
                Hit::CloseTab(index) => {
                    self.request_close(Close::Tab(index));
                }
                Hit::TabsBack => self.tab_start = self.tab_start.saturating_sub(1),
                Hit::TabsForward => self.tab_start = self.tab_start.saturating_add(1),
                Hit::Entry(path, true) => {
                    if !self.expanded.remove(&path) {
                        self.expanded.insert(path);
                    }
                }
                Hit::Entry(path, false) | Hit::Result(path) => self.open_path(path),
                Hit::Field(next) => {
                    if let Panel::Search { field, all, .. } = &mut self.panel {
                        *field = next;
                        *all = false;
                        self.focus = unique_id();
                    }
                }
                Hit::Find | Hit::Replace | Hit::ReplaceAll => {
                    if let Panel::Search { field, .. } = &mut self.panel {
                        *field = hit != Hit::Find;
                    }
                    self.focus = unique_id();
                    self.run_panel(false, hit == Hit::ReplaceAll);
                }
                Hit::ConfirmSave | Hit::ConfirmDiscard | Hit::ConfirmCancel => {}
            }
        } else if self.confirmation.is_none()
            && col >= self.layout.left
            && row >= self.layout.top
            && row + 1 < self.layout.rows
        {
            if !matches!(self.panel, Panel::None) {
                self.set_panel(Panel::None);
                return;
            }
            let (x, y) = self.document_coordinates(x, y, metrics);
            if let Some(doc) = self.document_mut() {
                doc.mouse_press(x, y, metrics, time);
            }
        }
    }

    pub fn mouse_confirmation(&mut self, x: f64, y: f64, metrics: Metrics) -> Option<Effect> {
        self.confirmation.as_ref()?;
        let (col, row) = self.cell_at(x, y, metrics)?;
        let hit = &self
            .hits
            .iter()
            .find(|item| item.row == row && col >= item.col && col < item.col + item.width)?
            .hit;
        let command = match hit {
            Hit::ConfirmSave => Command::ConfirmSave,
            Hit::ConfirmDiscard => Command::ConfirmDiscard,
            Hit::ConfirmCancel => Command::ConfirmCancel,
            _ => return None,
        };
        Some(self.confirm(command))
    }

    fn cell_at(&self, x: f64, y: f64, metrics: Metrics) -> Option<(usize, usize)> {
        let pad = metrics.padding as f64;
        if x < pad || y < pad {
            return None;
        }
        let col = ((x - pad) / metrics.cell_width as f64) as usize;
        let row = ((y - pad) / metrics.cell_height as f64) as usize;
        (col < self.layout.columns && row < self.layout.rows).then_some((col, row))
    }

    fn document_coordinates(&self, x: f64, y: f64, metrics: Metrics) -> (f64, f64) {
        (
            x - (self.layout.left * metrics.cell_width) as f64,
            y - (self.layout.top * metrics.cell_height) as f64,
        )
    }

    pub fn mouse_release(&mut self) {
        if let Some(doc) = self.document_mut() {
            doc.mouse_release();
        }
    }

    pub fn mouse_drag(&mut self, x: f64, y: f64, metrics: Metrics, tick: bool) -> bool {
        let (x, y) = self.document_coordinates(x, y, metrics);
        let Some(doc) = self.document_mut().filter(|doc| doc.is_mouse_dragging()) else {
            return false;
        };
        let top = metrics.padding as f64;
        let bottom = top + (doc.view_rows() * metrics.cell_height) as f64;
        let scrolled = tick
            && (if y < top {
                doc.scroll_lines(-1)
            } else if y >= bottom {
                doc.scroll_lines(1)
            } else {
                false
            });
        doc.mouse_drag(x, y.clamp(top, (bottom - 1.0).max(top)), metrics) || scrolled
    }

    pub fn scroll(&mut self, delta: isize, x: f64, y: f64, metrics: Metrics) -> bool {
        if self
            .cell_at(x, y, metrics)
            .is_some_and(|(col, _)| col < self.layout.left)
        {
            let old = self.explorer_scroll;
            let visible = self.layout.rows.saturating_sub(3);
            self.explorer_scroll = old
                .saturating_add_signed(delta)
                .min(self.explorer_total.saturating_sub(visible));
            self.explorer_scroll != old
        } else {
            self.document_mut()
                .is_some_and(|doc| doc.scroll_lines(delta))
        }
    }
}

#[cfg(test)]
impl BEdit {
    pub fn test_text(&self) -> String {
        self.document().unwrap().text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, BEdit) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let files = vec![root.join("a.rs"), root.join("b.py")];
        fs::write(&files[0], b"\xef\xbb\xbflet x = 1;\r\n").unwrap();
        fs::write(&files[1], "世界\n").unwrap();
        let editor = BEdit::open(OpenRequest {
            root: Some(root),
            files,
        })
        .unwrap();
        (dir, editor)
    }

    fn wait_for_open(editor: &mut BEdit) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while editor.open_job.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "file loader timed out"
            );
            editor.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn switch_tabs_preserves_dirty_text_selection_viewport_and_undo() {
        let (dir, mut editor) = fixture();
        editor.handle(Command::Insert("Привет ".into()));
        editor.handle(Command::SelectAll);
        editor.action(Action::NextTab(false));
        editor.handle(Command::Insert("// edit ".into()));
        editor.handle(Command::Save);
        assert_eq!(
            fs::read(dir.path().join("a.rs")).unwrap(),
            b"\xef\xbb\xbf// edit let x = 1;\r\n"
        );
        editor.action(Action::NextTab(false));
        assert_eq!(editor.selection_text().as_deref(), Some("Привет 世界\n"));
        assert!(editor.document().unwrap().is_modified());
        editor.handle(Command::Undo);
        assert_eq!(editor.test_text(), "世界\n");
        assert!(!editor.document().unwrap().is_modified());
        let view = editor.view(80, 24);
        assert!(view.status.contains("Python"));
        assert!(view.chrome.iter().any(|row| row.text.contains("BEdit")));
        assert!(view.text_offset.0 > 0);
        editor.action(Action::ToggleExplorer);
        assert_eq!(editor.view(80, 24).text_offset.0, 0);
    }

    #[test]
    fn canceling_multi_file_close_preserves_earlier_discards_and_failed_saves() {
        let (dir, mut editor) = fixture();
        editor.handle(Command::Insert("b".into()));
        editor.action(Action::NextTab(false));
        editor.handle(Command::Insert("a".into()));
        assert!(!editor.request_window_close());
        assert_eq!(
            editor.handle(Command::Insert("ignored".into())),
            Effect::None
        );
        assert_eq!(editor.handle(Command::ConfirmDiscard), Effect::Changed);
        assert_eq!(editor.handle(Command::Escape), Effect::Changed);
        assert!(editor.tabs.iter().all(|tab| tab.doc.is_modified()));
        assert!(editor.tabs[0].doc.text().starts_with("a"));
        assert!(editor.tabs[1].doc.text().starts_with("b"));

        assert!(!editor.request_window_close());
        fs::write(dir.path().join("a.rs"), "external").unwrap();
        assert_eq!(editor.handle(Command::ConfirmSave), Effect::Changed);
        assert!(!editor.is_confirming_close());
        assert!(
            editor
                .document()
                .unwrap()
                .message()
                .unwrap()
                .contains("changed on disk")
        );
        assert!(editor.tabs.iter().all(|tab| tab.doc.is_modified()));
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "external"
        );
        editor.handle(Command::Save);
        assert!(!editor.tabs[0].doc.is_modified());

        assert_eq!(editor.action(Action::Leave), Effect::Changed);
        assert_eq!(editor.handle(Command::ConfirmSave), Effect::Close);
        assert_eq!(
            fs::read_to_string(dir.path().join("b.py")).unwrap(),
            "b世界\n"
        );
    }

    #[test]
    fn closing_one_tab_preserves_others_and_stale_pastes_do_not_cross_tabs() {
        let (_dir, mut editor) = fixture();
        let old_focus = editor.paste_target();
        editor.handle(Command::Insert("b".into()));
        editor.action(Action::NextTab(false));
        editor.handle(Command::Insert("a".into()));
        assert!(!editor.paste(old_focus, "wrong tab"));
        assert_eq!(editor.action(Action::CloseTab), Effect::Changed);
        assert_eq!(editor.handle(Command::ConfirmDiscard), Effect::Changed);
        assert_eq!(editor.tabs.len(), 1);
        assert_eq!(editor.test_text(), "b世界\n");
        assert!(!editor.paste(old_focus, "stale focus"));
        assert!(editor.paste(editor.paste_target(), "é"));
        assert_eq!(editor.test_text(), "bé世界\n");
    }

    #[test]
    fn aliases_reuse_unsaved_tab_and_batch_errors_are_atomic() {
        let (dir, mut editor) = fixture();
        editor.handle(Command::Insert("unsaved ".into()));
        let alias = dir.path().join("alias.py");
        std::os::unix::fs::symlink(dir.path().join("b.py"), &alias).unwrap();
        editor.open_request(OpenRequest {
            root: None,
            files: vec![alias],
        });
        wait_for_open(&mut editor);
        assert_eq!(editor.tabs.len(), 2);
        assert_eq!(editor.test_text(), "unsaved 世界\n");
        let invalid = dir.path().join("invalid");
        fs::write(&invalid, b"\xff").unwrap();
        editor.open_request(OpenRequest {
            root: None,
            files: vec![dir.path().join("new"), invalid],
        });
        wait_for_open(&mut editor);
        assert_eq!(editor.tabs.len(), 2);
        assert!(editor.view(80, 24).status.contains("not valid UTF-8"));
        assert_eq!(editor.test_text(), "unsaved 世界\n");
    }

    #[test]
    fn quick_open_and_replace_use_keyboard_and_path_index() {
        let (_dir, mut editor) = fixture();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while editor.project.loading() {
            assert!(std::time::Instant::now() < deadline);
            editor.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        editor.action(Action::QuickOpen);
        editor.handle(Command::Insert(".RS".into()));
        assert!(
            editor
                .view(80, 24)
                .chrome
                .iter()
                .any(|row| row.text.contains("a.rs"))
        );
        editor.panel_enter(false, false);
        wait_for_open(&mut editor);
        assert_eq!(editor.document().unwrap().label(), "a.rs");
        editor.action(Action::Replace);
        editor.handle(Command::Insert("let".into()));
        editor.panel_enter(false, false);
        assert_eq!(editor.selection_text().as_deref(), Some("let"));
        editor.handle(Command::Tab);
        editor.handle(Command::Insert("const".into()));
        editor.panel_enter(false, false);
        editor.handle(Command::Escape);
        assert_eq!(editor.test_text(), "const x = 1;\r\n");
        editor.handle(Command::Undo);
        assert_eq!(editor.test_text(), "let x = 1;\r\n");
    }

    #[test]
    fn pointer_hits_explorer_tabs_document_and_close_prompt() {
        let (_dir, mut editor) = fixture();
        editor.view(80, 24);
        let metrics = Metrics {
            cell_width: 10,
            cell_height: 18,
            padding: 12,
        };
        let click = |editor: &mut BEdit, hit: Hit| {
            let region = editor.hits.iter().find(|region| region.hit == hit).unwrap();
            let x = (metrics.padding + region.col * metrics.cell_width + 1) as f64;
            let y = (metrics.padding + region.row * metrics.cell_height + 1) as f64;
            if let Some(effect) = editor.mouse_confirmation(x, y, metrics) {
                return effect;
            }
            editor.mouse_press(x, y, metrics, 100);
            Effect::Changed
        };
        click(&mut editor, Hit::Tab(0));
        assert_eq!(editor.document().unwrap().label(), "a.rs");
        editor.view(80, 24);
        click(&mut editor, Hit::Explorer);
        assert_eq!(editor.view(80, 24).text_offset.0, 0);
        editor.handle(Command::Insert("dirty ".into()));
        click(&mut editor, Hit::CloseTab(0));
        editor.view(80, 24);
        click(&mut editor, Hit::ConfirmCancel);
        assert!(editor.document().unwrap().is_modified());
        assert_eq!(editor.tabs.len(), 2);
    }
}
