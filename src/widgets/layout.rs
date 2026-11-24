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
    pub margin: f32,
    pub z_index: i32,
    pub opacity: f32,
    pub scale: f32,
    pub border_radius: f32,
    pub background_color: Option<String>,
    pub border_color: Option<String>,
    pub text_color: Option<String>,
    pub click_through: bool,
    pub show_on_hover: bool,
    pub snap_to_grid: bool,
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
            margin: 8.0,
            z_index: 10,
            opacity: 0.92,
            scale: 1.0,
            border_radius: 8.0,
            background_color: None,
            border_color: None,
            text_color: None,
            click_through: false,
            show_on_hover: false,
            snap_to_grid: false,
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

/// Compute viewport insets reserved by `Dock` widgets (never active in TUI mode).
pub fn compute_dock_insets(
    configs: &[&WidgetPlacementConfig],
    in_tui: bool,
    scale: f32,
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
    insets
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
    let effective_mode =
        resolve_effective_mode(cfg.mode, cfg.tui_behavior, in_tui, inline_y_px.is_some());
    if effective_mode == PlacementMode::Hidden {
        return ResolvedWidgetRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            effective_mode,
            visible: false,
            interactive: false,
        };
    }

    let w = (cfg.width * cfg.scale * scale).max(24.0).min(win_w);
    let h = (cfg.height * cfg.scale * scale).max(20.0).min(win_h);
    let margin = cfg.margin * scale;

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

/// Interactive visual layout editor state (Drag & Drop, resize, snapping).
#[derive(Clone, Debug, Default)]
pub struct VisualLayoutEditor {
    pub active: bool,
    pub selected_widget: Option<String>,
    pub drag_mode: Option<DragOperation>,
    pub drag_start_pointer: (f32, f32),
    pub drag_start_rect: (f32, f32, f32, f32),
    pub snap_threshold_px: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOperation {
    Move,
    ResizeBottomRight,
    ResizeRight,
    ResizeBottom,
}

impl VisualLayoutEditor {
    pub fn new() -> Self {
        Self {
            active: false,
            selected_widget: None,
            drag_mode: None,
            drag_start_pointer: (0.0, 0.0),
            drag_start_rect: (0.0, 0.0, 0.0, 0.0),
            snap_threshold_px: 12.0,
        }
    }

    pub fn toggle(&mut self) {
        self.active = !self.active;
        if !self.active {
            self.drag_mode = None;
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
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_drag(
        &self,
        cfg: &mut WidgetPlacementConfig,
        pointer_px: (f32, f32),
        win_w: f32,
        win_h: f32,
        scale: f32,
        cell_w: f32,
        cell_h: f32,
    ) {
        let Some(op) = self.drag_mode else {
            return;
        };
        let dx = pointer_px.0 - self.drag_start_pointer.0;
        let dy = pointer_px.1 - self.drag_start_pointer.1;
        let (sx, sy, sw, sh) = self.drag_start_rect;
        let snap = self.snap_threshold_px * scale;

        match op {
            DragOperation::Move => {
                let mut nx = (sx + dx).clamp(0.0, (win_w - sw).max(0.0));
                let mut ny = (sy + dy).clamp(0.0, (win_h - sh).max(0.0));
                // Snap to edges & center
                if nx < snap {
                    nx = 0.0;
                } else if (win_w - (nx + sw)).abs() < snap {
                    nx = win_w - sw;
                } else if ((nx + sw * 0.5) - win_w * 0.5).abs() < snap {
                    nx = (win_w - sw) * 0.5;
                }
                if ny < snap {
                    ny = 0.0;
                } else if (win_h - (ny + sh)).abs() < snap {
                    ny = win_h - sh;
                } else if ((ny + sh * 0.5) - win_h * 0.5).abs() < snap {
                    ny = (win_h - sh) * 0.5;
                }
                if cfg.snap_to_grid && cell_w > 0.0 && cell_h > 0.0 {
                    nx = (nx / cell_w).round() * cell_w;
                    ny = (ny / cell_h).round() * cell_h;
                }
                // Express relative to TopLeft with 0 margin for predictable visual drag
                cfg.anchor = Anchor::TopLeft;
                cfg.margin = 0.0;
                cfg.x = nx / scale;
                cfg.y = ny / scale;
            }
            DragOperation::ResizeBottomRight => {
                cfg.width = ((sw + dx) / (cfg.scale * scale)).clamp(48.0, win_w / scale);
                cfg.height = ((sh + dy) / (cfg.scale * scale)).clamp(28.0, win_h / scale);
            }
            DragOperation::ResizeRight => {
                cfg.width = ((sw + dx) / (cfg.scale * scale)).clamp(48.0, win_w / scale);
            }
            DragOperation::ResizeBottom => {
                cfg.height = ((sh + dy) / (cfg.scale * scale)).clamp(28.0, win_h / scale);
            }
        }
    }

    pub fn end_drag(&mut self) {
        self.drag_mode = None;
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
}
