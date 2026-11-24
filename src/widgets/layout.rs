// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Widget Layout Engine & Visual Layout Editor.
//!
//! Supports Overlay, Behind, Dock, Float, Inline, and Hidden placement modes,
//! fullscreen TUI detection with safe fallback (Overlay/Behind/Hidden only),
//! snapping to corners/center/edges/terminal grid, HiDPI scaling, click-through,
//! hover-reveal, layout profiles, and interactive Drag & Drop / resize editing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Placement mode of a widget relative to the terminal grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlacementMode {
    /// Drawn on top of terminal contents without resizing PTY.
    #[default]
    Overlay,
    /// Overlay that shifts above/below the active cursor row to avoid obscuring input.
    SmartOverlay,
    /// Drawn behind terminal contents (visible through transparent/unpainted background).
    Behind,
    /// Reserves a dedicated strip on an edge of the window, shrinking the terminal viewport.
    Dock,
    /// Floating card with visual text-wrap/avoidance where safe; falls back to Overlay in TUI.
    Float,
    /// Anchored inside the shell output stream (e.g. via OSC 133 command blocks), scrolls with output.
    Inline,
    /// Completely hidden.
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockEdge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// Behavior when a fullscreen TUI application (vim, nvim, htop, tmux, etc.) is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TuiBehavior {
    #[default]
    Overlay,
    Behind,
    Hidden,
}

/// Anchor point in the window for responsive positioning on resize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anchor {
    #[default]
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Responsive viewport breakpoint based on logical window width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewportBreakpoint {
    /// Very small window (< 440 logical px wide).
    Micro,
    /// Narrow Niri column (440..720 logical px wide).
    Narrow,
    /// Half-screen window (720..1120 logical px wide).
    Medium,
    /// Wide / fullscreen window (>= 1120 logical px wide).
    #[default]
    Wide,
}

impl ViewportBreakpoint {
    pub fn from_logical_width(logical_w: f32) -> Self {
        if logical_w < 440.0 {
            Self::Micro
        } else if logical_w < 720.0 {
            Self::Narrow
        } else if logical_w < 1120.0 {
            Self::Medium
        } else {
            Self::Wide
        }
    }

    pub fn is_compact(self) -> bool {
        matches!(self, Self::Micro | Self::Narrow)
    }
}

/// Adaptive behavior in narrow/micro windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NarrowBehavior {
    /// Automatically switch to compact mode and shrink within available width.
    #[default]
    Compact,
    /// Keep normal layout but scale/clamp to fit viewport.
    Shrink,
    /// Hide widget in Micro/Narrow windows to preserve terminal legibility.
    Hide,
}

/// Persistent placement & visual styling configuration for a single widget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetPlacementConfig {
    pub mode: PlacementMode,
    pub tui_behavior: TuiBehavior,
    pub dock_edge: DockEdge,
    pub anchor: Anchor,
    /// Offset in logical pixels from the anchor point.
    pub x: f32,
    pub y: f32,
    /// Logical width and height.
    pub width: f32,
    pub height: f32,
    pub min_width: f32,
    pub max_width: f32,
    pub min_height: f32,
    pub max_height: f32,
    pub margin: f32,
    pub z_index: i32,
    /// Higher priority widgets keep their preferred position during overlap avoidance.
    pub priority: i32,
    pub opacity: f32,
    pub scale: f32,
    pub border_radius: f32,
    pub border_width: f32,
    pub background_color: Option<String>,
    pub border_color: Option<String>,
    pub text_color: Option<String>,
    pub click_through: bool,
    pub show_on_hover: bool,
    pub hide_on_unfocus: bool,
    pub snap_to_grid: bool,
    pub avoid_overlap: bool,
    pub locked: bool,
    pub narrow_behavior: NarrowBehavior,
    pub hotkey: Option<String>,
    /// For Inline mode: optional OSC 133 command_id or line anchor.
    pub inline_row: Option<usize>,
}

impl Default for WidgetPlacementConfig {
    fn default() -> Self {
        Self {
            mode: PlacementMode::Overlay,
            tui_behavior: TuiBehavior::Overlay,
            dock_edge: DockEdge::Top,
            anchor: Anchor::TopRight,
            x: -16.0,
            y: 16.0,
            width: 240.0,
            height: 64.0,
            min_width: 72.0,
            max_width: 640.0,
            min_height: 26.0,
            max_height: 480.0,
            margin: 8.0,
            z_index: 10,
            priority: 10,
            opacity: 0.94,
            scale: 1.0,
            border_radius: 10.0,
            border_width: 1.0,
            background_color: None,
            border_color: None,
            text_color: None,
            click_through: false,
            show_on_hover: false,
            hide_on_unfocus: false,
            snap_to_grid: false,
            avoid_overlap: true,
            locked: false,
            narrow_behavior: NarrowBehavior::Compact,
            hotkey: None,
            inline_row: None,
        }
    }
}

/// Resolved physical-pixel rectangle and effective mode for a widget on the current frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedWidgetRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub effective_mode: PlacementMode,
    pub breakpoint: ViewportBreakpoint,
    pub compact: bool,
    pub visible: bool,
    pub interactive: bool,
}

/// Terminal viewport insets produced by `Dock` widgets.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportInsets {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

/// Named layout profile storing placements for multiple widgets.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LayoutProfile {
    pub name: String,
    pub widgets: BTreeMap<String, WidgetPlacementConfig>,
}

/// Manages built-in and user layout profiles (create, rename, copy, delete,
/// switch, import, export).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProfileStore {
    pub active: String,
    pub profiles: BTreeMap<String, LayoutProfile>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        for (name, widgets) in [
            ("Minimal", BTreeMap::new()),
            ("Development", Self::development_preset()),
            ("Music", Self::music_preset()),
            ("Diagnostics", Self::diagnostics_preset()),
        ] {
            profiles.insert(
                name.to_string(),
                LayoutProfile {
                    name: name.to_string(),
                    widgets,
                },
            );
        }
        Self {
            active: "Development".to_string(),
            profiles,
        }
    }
}

impl ProfileStore {
    fn development_preset() -> BTreeMap<String, WidgetPlacementConfig> {
        let mut m = BTreeMap::new();
        m.insert(
            "beyrix.official.keyboard-layout".into(),
            WidgetPlacementConfig {
                mode: PlacementMode::Overlay,
                anchor: Anchor::TopRight,
                x: -12.0,
                y: 10.0,
                width: 84.0,
                height: 30.0,
                min_width: 56.0,
                min_height: 26.0,
                priority: 30,
                click_through: true,
                ..Default::default()
            },
        );
        m
    }

    fn music_preset() -> BTreeMap<String, WidgetPlacementConfig> {
        let mut m = Self::development_preset();
        m.insert(
            "beyrix.official.media-player".into(),
            WidgetPlacementConfig {
                mode: PlacementMode::Overlay,
                anchor: Anchor::BottomRight,
                x: -14.0,
                y: -14.0,
                width: 280.0,
                height: 112.0,
                min_width: 170.0,
                min_height: 44.0,
                priority: 20,
                ..Default::default()
            },
        );
        m
    }

    fn diagnostics_preset() -> BTreeMap<String, WidgetPlacementConfig> {
        Self::development_preset()
    }

    pub fn create(&mut self, name: &str, widgets: BTreeMap<String, WidgetPlacementConfig>) -> bool {
        let name = name.trim();
        if name.is_empty() || self.profiles.contains_key(name) {
            return false;
        }
        self.profiles.insert(
            name.to_string(),
            LayoutProfile {
                name: name.to_string(),
                widgets,
            },
        );
        true
    }

    pub fn rename(&mut self, old: &str, new: &str) -> bool {
        let new = new.trim();
        if new.is_empty() || self.profiles.contains_key(new) {
            return false;
        }
        if let Some(mut p) = self.profiles.remove(old) {
            p.name = new.to_string();
            self.profiles.insert(new.to_string(), p);
            if self.active == old {
                self.active = new.to_string();
            }
            true
        } else {
            false
        }
    }

    pub fn duplicate(&mut self, source: &str, dest: &str) -> bool {
        let Some(p) = self.profiles.get(source).cloned() else {
            return false;
        };
        self.create(dest, p.widgets)
    }

    pub fn delete(&mut self, name: &str) -> bool {
        if self.profiles.len() <= 1 || !self.profiles.contains_key(name) {
            return false;
        }
        self.profiles.remove(name);
        if self.active == name
            && let Some(first) = self.profiles.keys().next().cloned()
        {
            self.active = first;
        }
        true
    }

    pub fn switch(&mut self, name: &str) -> Option<&LayoutProfile> {
        if self.profiles.contains_key(name) {
            self.active = name.to_string();
            self.profiles.get(name)
        } else {
            None
        }
    }

    pub fn export_toml(&self, name: &str) -> Result<String, String> {
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| format!("profile {name:?} not found"))?;
        toml::to_string_pretty(profile).map_err(|e| format!("export profile: {e}"))
    }

    pub fn import_toml(&mut self, text: &str) -> Result<String, String> {
        let profile: LayoutProfile =
            toml::from_str(text).map_err(|e| format!("import profile: {e}"))?;
        if profile.name.trim().is_empty() {
            return Err("imported profile has empty name".to_string());
        }
        let name = profile.name.clone();
        self.profiles.insert(name.clone(), profile);
        Ok(name)
    }
}

/// Multi-signal fullscreen TUI detector.
///
/// Alternate screen alone is not treated as the sole signal; we combine
/// alternate screen, mouse tracking modes, full-height scroll margins, and
/// optional foreground process hints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TuiSignals {
    pub alternate_screen: bool,
    pub mouse_reporting: bool,
    pub bracketed_paste: bool,
    pub full_scroll_margins: bool,
    pub known_tui_foreground: bool,
}

impl TuiSignals {
    pub fn is_fullscreen_tui(self) -> bool {
        if self.alternate_screen {
            return true;
        }
        // Fallback heuristic for TUI apps that draw on main screen with mouse reporting
        // or known TUI process name while using full scroll margins.
        self.known_tui_foreground && (self.mouse_reporting || self.full_scroll_margins)
    }
}

/// Resolve effective placement mode for a widget given TUI state and terminal capability.
pub fn resolve_effective_mode(
    configured: PlacementMode,
    tui_behavior: TuiBehavior,
    in_tui: bool,
    inline_anchor_visible: bool,
) -> PlacementMode {
    if configured == PlacementMode::Hidden {
        return PlacementMode::Hidden;
    }
    if in_tui {
        return match tui_behavior {
            TuiBehavior::Overlay => PlacementMode::Overlay,
            TuiBehavior::Behind => PlacementMode::Behind,
            TuiBehavior::Hidden => PlacementMode::Hidden,
        };
    }
    if configured == PlacementMode::Inline && !inline_anchor_visible {
        // Safe fallback when OSC 133 / inline anchor is not visible in viewport.
        return PlacementMode::Overlay;
    }
    configured
}

/// Compute viewport insets reserved by `Dock` widgets (never active in TUI mode),
/// clamped so excessive Dock areas can never crush the terminal grid.
pub fn compute_dock_insets(
    configs: &[&WidgetPlacementConfig],
    in_tui: bool,
    scale: f32,
) -> ViewportInsets {
    compute_dock_insets_bounded(configs, in_tui, scale, None)
}

pub fn compute_dock_insets_bounded(
    configs: &[&WidgetPlacementConfig],
    in_tui: bool,
    scale: f32,
    win_size_px: Option<(f32, f32)>,
) -> ViewportInsets {
    let mut insets = ViewportInsets::default();
    if in_tui {
        return insets;
    }
    for cfg in configs {
        if cfg.mode != PlacementMode::Dock {
            continue;
        }
        let w = (cfg.width * cfg.scale + cfg.margin * 2.0) * scale;
        let h = (cfg.height * cfg.scale + cfg.margin * 2.0) * scale;
        match cfg.dock_edge {
            DockEdge::Top => insets.top += h,
            DockEdge::Bottom => insets.bottom += h,
            DockEdge::Left => insets.left += w,
            DockEdge::Right => insets.right += w,
        }
    }
    if let Some((win_w, win_h)) = win_size_px {
        let max_x = (win_w * 0.45).max(0.0);
        let max_y = (win_h * 0.40).max(0.0);
        let total_x = insets.left + insets.right;
        if total_x > max_x && total_x > 0.0 {
            let f = max_x / total_x;
            insets.left *= f;
            insets.right *= f;
        }
        let total_y = insets.top + insets.bottom;
        if total_y > max_y && total_y > 0.0 {
            let f = max_y / total_y;
            insets.top *= f;
            insets.bottom *= f;
        }
    }
    insets
}

/// Resolve non-overlapping placement for a candidate rect against already-placed
/// higher-priority rects when `avoid_overlap` is enabled.
pub fn resolve_non_overlapping_position(
    mut rect: ResolvedWidgetRect,
    occupied: &[(f32, f32, f32, f32)],
    win_w: f32,
    win_h: f32,
    gap_px: f32,
) -> ResolvedWidgetRect {
    if !rect.visible || occupied.is_empty() {
        return rect;
    }
    let overlaps_any = |x: f32, y: f32, w: f32, h: f32| {
        occupied
            .iter()
            .any(|&(ox, oy, ow, oh)| x < ox + ow && x + w > ox && y < oy + oh && y + h > oy)
    };
    if !overlaps_any(rect.x, rect.y, rect.w, rect.h) {
        return rect;
    }
    // Try shifting down, up, left, right relative to each colliding rect.
    for &(ox, oy, ow, oh) in occupied {
        let candidates = [
            (rect.x, oy + oh + gap_px),
            (rect.x, oy - rect.h - gap_px),
            (ox - rect.w - gap_px, rect.y),
            (ox + ow + gap_px, rect.y),
        ];
        for (cx, cy) in candidates {
            if cx >= 0.0
                && cy >= 0.0
                && cx + rect.w <= win_w
                && cy + rect.h <= win_h
                && !overlaps_any(cx, cy, rect.w, rect.h)
            {
                rect.x = cx;
                rect.y = cy;
                return rect;
            }
        }
    }
    rect
}

/// Resolve a widget's physical-pixel rectangle inside `(win_w, win_h)`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_widget_rect(
    cfg: &WidgetPlacementConfig,
    win_w: f32,
    win_h: f32,
    scale: f32,
    cell_w: f32,
    cell_h: f32,
    in_tui: bool,
    hovered: bool,
    inline_y_px: Option<f32>,
) -> ResolvedWidgetRect {
    resolve_widget_rect_with_cursor(
        cfg,
        win_w,
        win_h,
        scale,
        cell_w,
        cell_h,
        in_tui,
        hovered,
        inline_y_px,
        None,
    )
}

/// Resolve a widget's physical-pixel rectangle inside `(win_w, win_h)`, with
/// optional `(cursor_y_px, row_height)` avoidance for `Float` mode.
#[allow(clippy::too_many_arguments)]
pub fn resolve_widget_rect_with_cursor(
    cfg: &WidgetPlacementConfig,
    win_w: f32,
    win_h: f32,
    scale: f32,
    cell_w: f32,
    cell_h: f32,
    in_tui: bool,
    hovered: bool,
    inline_y_px: Option<f32>,
    cursor_row_px: Option<(f32, f32)>,
) -> ResolvedWidgetRect {
    let logical_w = win_w / scale.max(0.25);
    let breakpoint = ViewportBreakpoint::from_logical_width(logical_w);
    let effective_mode =
        resolve_effective_mode(cfg.mode, cfg.tui_behavior, in_tui, inline_y_px.is_some());
    if effective_mode == PlacementMode::Hidden
        || (breakpoint.is_compact() && cfg.narrow_behavior == NarrowBehavior::Hide)
    {
        return ResolvedWidgetRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            effective_mode: PlacementMode::Hidden,
            breakpoint,
            compact: breakpoint.is_compact(),
            visible: false,
            interactive: false,
        };
    }

    let compact = breakpoint.is_compact() && cfg.narrow_behavior == NarrowBehavior::Compact;
    let margin = cfg.margin * scale;
    let max_avail_w = (win_w - margin * 2.0).max(24.0);
    let max_avail_h = (win_h - margin * 2.0).max(20.0);

    let min_w = (cfg.min_width * scale).min(max_avail_w);
    let max_w = (cfg.max_width * scale).min(max_avail_w).max(min_w);
    let min_h = (cfg.min_height * scale).min(max_avail_h);
    let max_h = (cfg.max_height * scale).min(max_avail_h).max(min_h);

    let base_w = if compact {
        (cfg.width * 0.78).max(cfg.min_width)
    } else {
        cfg.width
    };
    let base_h = if compact {
        // In compact mode, adapt cleanly to the widget's compact minimum height
        // instead of arbitrarily scaling normal height down and squishing children.
        cfg.min_height.max(44.0).min(cfg.height)
    } else {
        cfg.height
    };
    let w = (base_w * cfg.scale * scale).clamp(min_w, max_w);
    let h = (base_h * cfg.scale * scale).clamp(min_h, max_h);

    let (mut x, mut y) = match effective_mode {
        PlacementMode::Dock => match cfg.dock_edge {
            DockEdge::Top => ((win_w - w) * 0.5, margin),
            DockEdge::Bottom => ((win_w - w) * 0.5, (win_h - h - margin).max(0.0)),
            DockEdge::Left => (margin, (win_h - h) * 0.5),
            DockEdge::Right => ((win_w - w - margin).max(0.0), (win_h - h) * 0.5),
        },
        PlacementMode::Inline => {
            let iy = inline_y_px.unwrap_or(margin);
            (
                (win_w - w - margin).max(margin),
                iy.clamp(0.0, (win_h - h).max(0.0)),
            )
        }
        _ => {
            let (ax, ay) = anchor_origin(cfg.anchor, win_w, win_h, w, h, margin);
            (ax + cfg.x * scale, ay + cfg.y * scale)
        }
    };

    if cfg.snap_to_grid && cell_w > 0.0 && cell_h > 0.0 {
        x = (x / cell_w).round() * cell_w;
        y = (y / cell_h).round() * cell_h;
    }

    if effective_mode == PlacementMode::SmartOverlay
        && let Some((cy, ch)) = cursor_row_px
        && y < cy + ch
        && y + h > cy
    {
        // SmartOverlay: shift vertically above or below the active cursor row
        // without altering PTY/VT coordinates.
        let above = cy - h - margin;
        let below = cy + ch + margin;
        if above >= 0.0 {
            y = above;
        } else if below + h <= win_h {
            y = below;
        }
    }

    x = x.clamp(0.0, (win_w - w).max(0.0));
    y = y.clamp(0.0, (win_h - h).max(0.0));

    let visible = !cfg.show_on_hover || hovered;
    let interactive = visible && !cfg.click_through && effective_mode != PlacementMode::Behind;

    ResolvedWidgetRect {
        x,
        y,
        w,
        h,
        effective_mode,
        breakpoint,
        compact,
        visible,
        interactive,
    }
}

fn anchor_origin(
    anchor: Anchor,
    win_w: f32,
    win_h: f32,
    w: f32,
    h: f32,
    margin: f32,
) -> (f32, f32) {
    match anchor {
        Anchor::TopLeft => (margin, margin),
        Anchor::TopCenter => ((win_w - w) * 0.5, margin),
        Anchor::TopRight => (win_w - w - margin, margin),
        Anchor::CenterLeft => (margin, (win_h - h) * 0.5),
        Anchor::Center => ((win_w - w) * 0.5, (win_h - h) * 0.5),
        Anchor::CenterRight => (win_w - w - margin, (win_h - h) * 0.5),
        Anchor::BottomLeft => (margin, win_h - h - margin),
        Anchor::BottomCenter => ((win_w - w) * 0.5, win_h - h - margin),
        Anchor::BottomRight => (win_w - w - margin, win_h - h - margin),
    }
}

/// Alignment guide line emitted during interactive Drag & Drop snapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapGuide {
    pub vertical: bool,
    pub position_px: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorTab {
    #[default]
    Catalog,
    Properties,
    Layers,
    Profiles,
    Background,
    XRay,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StudioLeftTab {
    #[default]
    Catalog,
    Layers,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NarrowStudioPane {
    #[default]
    Preview,
    Catalog,
    Inspector,
}

/// Interactive visual layout editor (Beyrix Studio) state: Drag & Drop, resize,
/// snapping, Undo/Redo, alignment guides, focused numeric inputs, and panes.
#[derive(Clone, Debug, Default)]
pub struct VisualLayoutEditor {
    pub active: bool,
    pub active_tab: EditorTab,
    pub left_tab: StudioLeftTab,
    pub narrow_pane: NarrowStudioPane,
    pub selected_widget: Option<String>,
    pub focused_input: Option<String>,
    pub input_buffer: String,
    pub inspector_scroll: f32,
    pub drag_mode: Option<DragOperation>,
    pub drag_start_pointer: (f32, f32),
    pub drag_start_rect: (f32, f32, f32, f32),
    pub snap_threshold_px: f32,
    pub active_guides: Vec<SnapGuide>,
    pub undo_stack: Vec<BTreeMap<String, WidgetPlacementConfig>>,
    pub redo_stack: Vec<BTreeMap<String, WidgetPlacementConfig>>,
    pub confirm_reset_profile: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOperation {
    Move,
    ResizeBottomRight,
    ResizeRight,
    ResizeBottom,
    ResizeLeft,
    ResizeTop,
}

impl VisualLayoutEditor {
    pub fn new() -> Self {
        Self {
            active: false,
            active_tab: EditorTab::Catalog,
            left_tab: StudioLeftTab::Catalog,
            narrow_pane: NarrowStudioPane::Preview,
            selected_widget: None,
            focused_input: None,
            input_buffer: String::new(),
            inspector_scroll: 0.0,
            drag_mode: None,
            drag_start_pointer: (0.0, 0.0),
            drag_start_rect: (0.0, 0.0, 0.0, 0.0),
            snap_threshold_px: 12.0,
            active_guides: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            confirm_reset_profile: false,
        }
    }

    pub fn push_undo(&mut self, snapshot: BTreeMap<String, WidgetPlacementConfig>) {
        if self.undo_stack.len() >= 64 {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(snapshot);
        self.redo_stack.clear();
    }

    pub fn undo(&mut self, current: &mut BTreeMap<String, WidgetPlacementConfig>) -> bool {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(current.clone());
            *current = prev;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self, current: &mut BTreeMap<String, WidgetPlacementConfig>) -> bool {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(current.clone());
            *current = next;
            true
        } else {
            false
        }
    }

    pub fn toggle(&mut self) {
        self.active = !self.active;
        if !self.active {
            self.drag_mode = None;
            self.active_guides.clear();
            self.confirm_reset_profile = false;
            self.focused_input = None;
            self.input_buffer.clear();
        }
    }

    pub fn begin_drag(
        &mut self,
        widget_id: String,
        op: DragOperation,
        pointer_px: (f32, f32),
        current_rect: (f32, f32, f32, f32),
    ) {
        self.selected_widget = Some(widget_id);
        self.drag_mode = Some(op);
        self.drag_start_pointer = pointer_px;
        self.drag_start_rect = current_rect;
        self.active_guides.clear();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_drag(
        &mut self,
        cfg: &mut WidgetPlacementConfig,
        pointer_px: (f32, f32),
        win_w: f32,
        win_h: f32,
        scale: f32,
        cell_w: f32,
        cell_h: f32,
    ) {
        self.update_drag_with_neighbors(cfg, pointer_px, win_w, win_h, scale, cell_w, cell_h, &[]);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_drag_with_neighbors(
        &mut self,
        cfg: &mut WidgetPlacementConfig,
        pointer_px: (f32, f32),
        win_w: f32,
        win_h: f32,
        scale: f32,
        cell_w: f32,
        cell_h: f32,
        neighbors: &[(f32, f32, f32, f32)],
    ) {
        if cfg.locked {
            return;
        }
        let Some(op) = self.drag_mode else {
            return;
        };
        self.active_guides.clear();
        let dx = pointer_px.0 - self.drag_start_pointer.0;
        let dy = pointer_px.1 - self.drag_start_pointer.1;
        let (sx, sy, sw, sh) = self.drag_start_rect;
        let snap = self.snap_threshold_px * scale;

        match op {
            DragOperation::Move => {
                let mut nx = (sx + dx).clamp(0.0, (win_w - sw).max(0.0));
                let mut ny = (sy + dy).clamp(0.0, (win_h - sh).max(0.0));
                // Snap to window edges & center
                if nx < snap {
                    nx = 0.0;
                    self.active_guides.push(SnapGuide {
                        vertical: true,
                        position_px: 0.0,
                    });
                } else if (win_w - (nx + sw)).abs() < snap {
                    nx = win_w - sw;
                    self.active_guides.push(SnapGuide {
                        vertical: true,
                        position_px: win_w,
                    });
                } else if ((nx + sw * 0.5) - win_w * 0.5).abs() < snap {
                    nx = (win_w - sw) * 0.5;
                    self.active_guides.push(SnapGuide {
                        vertical: true,
                        position_px: win_w * 0.5,
                    });
                }
                if ny < snap {
                    ny = 0.0;
                    self.active_guides.push(SnapGuide {
                        vertical: false,
                        position_px: 0.0,
                    });
                } else if (win_h - (ny + sh)).abs() < snap {
                    ny = win_h - sh;
                    self.active_guides.push(SnapGuide {
                        vertical: false,
                        position_px: win_h,
                    });
                } else if ((ny + sh * 0.5) - win_h * 0.5).abs() < snap {
                    ny = (win_h - sh) * 0.5;
                    self.active_guides.push(SnapGuide {
                        vertical: false,
                        position_px: win_h * 0.5,
                    });
                }
                // Snap to neighbor widget edges
                for &(ox, oy, ow, oh) in neighbors {
                    if (nx - ox).abs() < snap {
                        nx = ox;
                        self.active_guides.push(SnapGuide {
                            vertical: true,
                            position_px: ox,
                        });
                    } else if (nx - (ox + ow)).abs() < snap {
                        nx = ox + ow;
                        self.active_guides.push(SnapGuide {
                            vertical: true,
                            position_px: ox + ow,
                        });
                    }
                    if (ny - oy).abs() < snap {
                        ny = oy;
                        self.active_guides.push(SnapGuide {
                            vertical: false,
                            position_px: oy,
                        });
                    } else if (ny - (oy + oh)).abs() < snap {
                        ny = oy + oh;
                        self.active_guides.push(SnapGuide {
                            vertical: false,
                            position_px: oy + oh,
                        });
                    }
                }
                if cfg.snap_to_grid && cell_w > 0.0 && cell_h > 0.0 {
                    nx = (nx / cell_w).round() * cell_w;
                    ny = (ny / cell_h).round() * cell_h;
                }
                cfg.anchor = Anchor::TopLeft;
                cfg.margin = 0.0;
                cfg.x = nx / scale;
                cfg.y = ny / scale;
            }
            DragOperation::ResizeBottomRight => {
                cfg.width = ((sw + dx) / (cfg.scale * scale)).clamp(cfg.min_width, win_w / scale);
                cfg.height = ((sh + dy) / (cfg.scale * scale)).clamp(cfg.min_height, win_h / scale);
            }
            DragOperation::ResizeRight => {
                cfg.width = ((sw + dx) / (cfg.scale * scale)).clamp(cfg.min_width, win_w / scale);
            }
            DragOperation::ResizeBottom => {
                cfg.height = ((sh + dy) / (cfg.scale * scale)).clamp(cfg.min_height, win_h / scale);
            }
            DragOperation::ResizeLeft => {
                let new_w = ((sw - dx) / (cfg.scale * scale)).clamp(cfg.min_width, win_w / scale);
                let actual_dx = sw - new_w * cfg.scale * scale;
                cfg.anchor = Anchor::TopLeft;
                cfg.margin = 0.0;
                cfg.x = (sx + actual_dx).max(0.0) / scale;
                cfg.width = new_w;
            }
            DragOperation::ResizeTop => {
                let new_h = ((sh - dy) / (cfg.scale * scale)).clamp(cfg.min_height, win_h / scale);
                let actual_dy = sh - new_h * cfg.scale * scale;
                cfg.anchor = Anchor::TopLeft;
                cfg.margin = 0.0;
                cfg.y = (sy + actual_dy).max(0.0) / scale;
                cfg.height = new_h;
            }
        }
    }

    pub fn end_drag(&mut self) {
        self.drag_mode = None;
        self.active_guides.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_mode_never_uses_dock_or_float() {
        assert_eq!(
            resolve_effective_mode(PlacementMode::Dock, TuiBehavior::Overlay, true, true),
            PlacementMode::Overlay
        );
        assert_eq!(
            resolve_effective_mode(PlacementMode::Float, TuiBehavior::Behind, true, true),
            PlacementMode::Behind
        );
        assert_eq!(
            resolve_effective_mode(PlacementMode::Overlay, TuiBehavior::Hidden, true, true),
            PlacementMode::Hidden
        );
        // Restores on TUI exit
        assert_eq!(
            resolve_effective_mode(PlacementMode::Dock, TuiBehavior::Overlay, false, true),
            PlacementMode::Dock
        );
    }

    #[test]
    fn dock_insets_zero_in_tui() {
        let cfg = WidgetPlacementConfig {
            mode: PlacementMode::Dock,
            dock_edge: DockEdge::Top,
            height: 40.0,
            margin: 4.0,
            ..Default::default()
        };
        let insets_normal = compute_dock_insets(&[&cfg], false, 1.25);
        assert!(insets_normal.top > 0.0);
        let insets_tui = compute_dock_insets(&[&cfg], true, 1.25);
        assert_eq!(insets_tui.top, 0.0);
    }

    #[test]
    fn click_through_and_hover_reveal_work() {
        let mut cfg = WidgetPlacementConfig {
            click_through: true,
            show_on_hover: true,
            ..Default::default()
        };
        let hidden = resolve_widget_rect(&cfg, 1280.0, 720.0, 1.25, 10.0, 18.0, false, false, None);
        assert!(!hidden.visible);
        assert!(!hidden.interactive);

        let shown = resolve_widget_rect(&cfg, 1280.0, 720.0, 1.25, 10.0, 18.0, false, true, None);
        assert!(shown.visible);
        assert!(!shown.interactive);

        cfg.click_through = false;
        let interactive =
            resolve_widget_rect(&cfg, 1280.0, 720.0, 1.25, 10.0, 18.0, false, true, None);
        assert!(interactive.interactive);
    }

    #[test]
    fn visual_editor_drag_and_snap_to_edges() {
        let mut ed = VisualLayoutEditor::new();
        ed.toggle();
        let mut cfg = WidgetPlacementConfig::default();
        ed.begin_drag(
            "w1".into(),
            DragOperation::Move,
            (500.0, 500.0),
            (500.0, 500.0, 200.0, 60.0),
        );
        // Drag near top-left within snap threshold
        ed.update_drag(&mut cfg, (5.0, 4.0), 1280.0, 720.0, 1.0, 10.0, 18.0);
        assert_eq!(cfg.x, 0.0);
        assert_eq!(cfg.y, 0.0);
        ed.end_drag();
    }

    #[test]
    fn smart_overlay_avoids_overlapping_active_cursor_row() {
        let cfg = WidgetPlacementConfig {
            mode: PlacementMode::SmartOverlay,
            anchor: Anchor::TopLeft,
            x: 10.0,
            y: 100.0,
            width: 200.0,
            height: 40.0,
            margin: 8.0,
            ..Default::default()
        };
        let resolved = resolve_widget_rect_with_cursor(
            &cfg,
            800.0,
            600.0,
            1.0,
            10.0,
            18.0,
            false,
            false,
            None,
            Some((110.0, 18.0)),
        );
        // Must shift so it does not overlap [110.0, 128.0)
        assert!(resolved.y + resolved.h <= 110.0 || resolved.y >= 128.0);
    }

    #[test]
    fn adaptive_breakpoints_overlap_avoidance_and_bounded_dock() {
        assert_eq!(
            ViewportBreakpoint::from_logical_width(380.0),
            ViewportBreakpoint::Micro
        );
        assert_eq!(
            ViewportBreakpoint::from_logical_width(560.0),
            ViewportBreakpoint::Narrow
        );
        assert_eq!(
            ViewportBreakpoint::from_logical_width(960.0),
            ViewportBreakpoint::Medium
        );
        assert_eq!(
            ViewportBreakpoint::from_logical_width(1440.0),
            ViewportBreakpoint::Wide
        );

        let cfg = WidgetPlacementConfig {
            width: 300.0,
            height: 80.0,
            min_width: 100.0,
            min_height: 32.0,
            anchor: Anchor::BottomRight,
            ..Default::default()
        };
        let narrow = resolve_widget_rect(&cfg, 500.0, 600.0, 1.0, 10.0, 18.0, false, false, None);
        assert!(narrow.compact);
        assert!(narrow.w < 300.0);

        // Two widgets at the same corner must not overlap after avoidance.
        let occupied = vec![(narrow.x, narrow.y, narrow.w, narrow.h)];
        let shifted = resolve_non_overlapping_position(narrow, &occupied, 500.0, 600.0, 8.0);
        let overlaps = shifted.x < narrow.x + narrow.w
            && shifted.x + shifted.w > narrow.x
            && shifted.y < narrow.y + narrow.h
            && shifted.y + shifted.h > narrow.y;
        assert!(!overlaps, "widget should shift out of collision");

        // Excessive dock areas are capped so terminal grid never collapses.
        let huge_dock = WidgetPlacementConfig {
            mode: PlacementMode::Dock,
            dock_edge: DockEdge::Top,
            height: 400.0,
            ..Default::default()
        };
        let bounded = compute_dock_insets_bounded(&[&huge_dock], false, 1.0, Some((800.0, 500.0)));
        assert!(bounded.top <= 500.0 * 0.40 + 0.01);
    }

    #[test]
    fn profile_store_crud_export_import_and_editor_undo_redo() {
        let mut store = ProfileStore::default();
        assert!(store.profiles.contains_key("Minimal"));
        assert!(store.profiles.contains_key("Development"));
        assert!(store.profiles.contains_key("Music"));
        assert!(store.profiles.contains_key("Diagnostics"));

        assert!(store.duplicate("Music", "MyMusic"));
        assert!(store.rename("MyMusic", "FocusMusic"));
        assert!(store.switch("FocusMusic").is_some());
        let exported = store.export_toml("FocusMusic").unwrap();
        assert!(store.delete("FocusMusic"));
        let imported = store.import_toml(&exported).unwrap();
        assert_eq!(imported, "FocusMusic");

        let mut ed = VisualLayoutEditor::new();
        let mut placements = BTreeMap::new();
        placements.insert("w1".into(), WidgetPlacementConfig::default());
        ed.push_undo(placements.clone());
        placements.get_mut("w1").unwrap().x = 123.0;
        assert!(ed.undo(&mut placements));
        assert_ne!(placements.get("w1").unwrap().x, 123.0);
        assert!(ed.redo(&mut placements));
        assert_eq!(placements.get("w1").unwrap().x, 123.0);
    }
}
