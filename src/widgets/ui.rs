// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Declarative Native UI tree, Beyrix Design System tokens, constraint layout
//! solver, hit-testing, and Scene emission for BeyrixWidgets and native panels.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::render::{
    FontKind, Renderer, Scene, SceneGlyph, SceneImage, SceneRoundedRect,
};
use crate::theme::Theme;

use super::icons;

/// Luminance-aware Beyrix Design System palette derived from the active terminal theme.
#[derive(Clone, Copy, Debug)]
pub struct DesignPalette {
    pub is_light: bool,
    pub panel_bg: [f32; 4],
    pub card_bg: [f32; 4],
    pub elevated_bg: [f32; 4],
    pub control_bg: [f32; 4],
    pub control_hover: [f32; 4],
    pub control_pressed: [f32; 4],
    pub border: [f32; 4],
    pub border_subtle: [f32; 4],
    pub border_strong: [f32; 4],
    pub fg: [f32; 4],
    pub fg_secondary: [f32; 4],
    pub accent: [f32; 4],
    pub accent_soft: [f32; 4],
    pub shadow: [f32; 4],
}

impl DesignPalette {
    pub fn from_theme(theme: &Theme) -> Self {
        let bg = [
            theme.background[0] as f32 / 255.0,
            theme.background[1] as f32 / 255.0,
            theme.background[2] as f32 / 255.0,
        ];
        let fg = [
            theme.foreground[0] as f32 / 255.0,
            theme.foreground[1] as f32 / 255.0,
            theme.foreground[2] as f32 / 255.0,
        ];
        let lum = 0.2126 * bg[0] + 0.7152 * bg[1] + 0.0722 * bg[2];
        let is_light = lum > 0.52;
        let mix = |a: [f32; 3], b: [f32; 3], t: f32, alpha: f32| -> [f32; 4] {
            [
                (a[0] * (1.0 - t) + b[0] * t).clamp(0.0, 1.0),
                (a[1] * (1.0 - t) + b[1] * t).clamp(0.0, 1.0),
                (a[2] * (1.0 - t) + b[2] * t).clamp(0.0, 1.0),
                alpha,
            ]
        };
        let accent = to_f4(theme.cursor, 1.0);
        let lift = if is_light { [1.0, 1.0, 1.0] } else { [0.92, 0.95, 1.0] };
        let deepen = if is_light { [0.0, 0.0, 0.0] } else { [0.02, 0.03, 0.05] };
        Self {
            is_light,
            // Opaque primary surfaces prevent terminal text bleed-through in Settings/Studio/X-Ray.
            panel_bg: mix(bg, lift, if is_light { 0.35 } else { 0.055 }, 0.99),
            card_bg: mix(bg, lift, if is_light { 0.55 } else { 0.09 }, 0.98),
            elevated_bg: mix(bg, lift, if is_light { 0.48 } else { 0.08 }, 0.96),
            control_bg: mix(bg, lift, if is_light { 0.14 } else { 0.13 }, 0.98),
            control_hover: mix(bg, lift, if is_light { 0.22 } else { 0.19 }, 1.0),
            control_pressed: mix(bg, deepen, 0.18, 1.0),
            border: [fg[0], fg[1], fg[2], if is_light { 0.18 } else { 0.14 }],
            border_subtle: [fg[0], fg[1], fg[2], if is_light { 0.10 } else { 0.08 }],
            border_strong: [fg[0], fg[1], fg[2], if is_light { 0.30 } else { 0.26 }],
            fg: [fg[0], fg[1], fg[2], 1.0],
            fg_secondary: mix(fg, bg, 0.36, 1.0),
            accent,
            accent_soft: [accent[0], accent[1], accent[2], if is_light { 0.16 } else { 0.20 }],
            shadow: [0.0, 0.0, 0.0, if is_light { 0.18 } else { 0.42 }],
        }
    }
}

/// Color specification in UI nodes: either a theme token or explicit RGBA.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiColor {
    Token(String),
    Rgba([f32; 4]),
}

impl UiColor {
    pub fn resolve(&self, theme: &Theme, default: [f32; 4]) -> [f32; 4] {
        let p = DesignPalette::from_theme(theme);
        match self {
            Self::Rgba(c) => *c,
            Self::Token(name) => match name.as_str() {
                "background" | "bg" => to_f4(theme.background, 1.0),
                "foreground" | "fg" => p.fg,
                "cursor" | "accent" => p.accent,
                "selection" => to_f4(theme.selection, 1.0),
                "selection_fg" => to_f4(theme.selection_foreground, 1.0),
                "muted" | "fg-secondary" => p.fg_secondary,
                "surface-panel" => p.panel_bg,
                "card" | "surface" | "surface-card" => p.card_bg,
                "surface-elevated" => p.elevated_bg,
                "surface-control" => p.control_bg,
                "surface-hover" => p.control_hover,
                "surface-pressed" => p.control_pressed,
                "border" => p.border,
                "border-subtle" => p.border_subtle,
                "border-strong" => p.border_strong,
                "accent-soft" => p.accent_soft,
                "red" | "error" => to_f4(theme.ansi[1], 1.0),
                "green" | "success" => to_f4(theme.ansi[2], 1.0),
                "yellow" | "warning" => to_f4(theme.ansi[3], 1.0),
                "blue" | "info" => to_f4(theme.ansi[4], 1.0),
                "magenta" => to_f4(theme.ansi[5], 1.0),
                "cyan" => to_f4(theme.ansi[6], 1.0),
                hex if hex.starts_with('#') => parse_hex_color(hex).unwrap_or(default),
                _ => default,
            },
        }
    }
}

fn to_f4(c: [u8; 3], a: f32) -> [f32; 4] {
    [
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
        a,
    ]
}

pub fn parse_hex_color(hex: &str) -> Option<[f32; 4]> {
    let s = hex.strip_prefix('#')?;
    if s.len() == 6 {
        let v = u32::from_str_radix(s, 16).ok()?;
        Some([
            ((v >> 16) & 0xff) as f32 / 255.0,
            ((v >> 8) & 0xff) as f32 / 255.0,
            (v & 0xff) as f32 / 255.0,
            1.0,
        ])
    } else if s.len() == 8 {
        let v = u32::from_str_radix(s, 16).ok()?;
        Some([
            ((v >> 24) & 0xff) as f32 / 255.0,
            ((v >> 16) & 0xff) as f32 / 255.0,
            ((v >> 8) & 0xff) as f32 / 255.0,
            (v & 0xff) as f32 / 255.0,
        ])
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CanvasCommand {
    Clear {
        color: UiColor,
    },
    FillRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: UiColor,
        #[serde(default)]
        radius: f32,
    },
    FillCircle {
        cx: f32,
        cy: f32,
        r: f32,
        color: UiColor,
    },
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
        color: UiColor,
    },
}

/// Declarative UI node emitted by BeyrixWidgets or native panels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum UiNode {
    Text {
        text: String,
        #[serde(default)]
        size: Option<f32>,
        #[serde(default)]
        color: Option<UiColor>,
        #[serde(default)]
        align: Align,
    },
    Button {
        id: String,
        label: String,
        #[serde(default)]
        bg: Option<UiColor>,
        #[serde(default)]
        color: Option<UiColor>,
        #[serde(default)]
        width: Option<f32>,
        #[serde(default)]
        height: Option<f32>,
        #[serde(default)]
        radius: f32,
    },
    Icon {
        name: String,
        #[serde(default)]
        size: Option<f32>,
        #[serde(default)]
        color: Option<UiColor>,
    },
    Image {
        id: u64,
        width: f32,
        height: f32,
        #[serde(default)]
        radius: f32,
        #[serde(default)]
        alpha: f32,
        #[serde(skip)]
        rgba: Option<(u32, u32, Arc<Vec<u8>>)>,
    },
    Container {
        #[serde(default)]
        padding: f32,
        #[serde(default)]
        bg: Option<UiColor>,
        #[serde(default)]
        border: Option<UiColor>,
        #[serde(default)]
        radius: f32,
        #[serde(default)]
        width: Option<f32>,
        #[serde(default)]
        height: Option<f32>,
        child: Box<UiNode>,
    },
    Row {
        #[serde(default)]
        gap: f32,
        #[serde(default)]
        align: Align,
        children: Vec<UiNode>,
    },
    Column {
        #[serde(default)]
        gap: f32,
        #[serde(default)]
        align: Align,
        children: Vec<UiNode>,
    },
    Grid {
        columns: usize,
        #[serde(default)]
        gap: f32,
        children: Vec<UiNode>,
    },
    Stack {
        children: Vec<UiNode>,
    },
    Progress {
        value: f32,
        #[serde(default)]
        width: Option<f32>,
        #[serde(default)]
        height: Option<f32>,
        #[serde(default)]
        color: Option<UiColor>,
        #[serde(default)]
        track: Option<UiColor>,
    },
    Slider {
        id: String,
        value: f32,
        #[serde(default)]
        width: Option<f32>,
        #[serde(default)]
        height: Option<f32>,
        #[serde(default)]
        color: Option<UiColor>,
        #[serde(default)]
        track: Option<UiColor>,
    },
    TextInput {
        id: String,
        value: String,
        #[serde(default)]
        placeholder: String,
        #[serde(default)]
        focused: bool,
        #[serde(default)]
        width: Option<f32>,
        #[serde(default)]
        disabled: bool,
    },
    Canvas {
        width: f32,
        height: f32,
        commands: Vec<CanvasCommand>,
    },
    ScrollView {
        #[serde(default)]
        id: String,
        #[serde(default)]
        scroll_y: f32,
        height: f32,
        children: Vec<UiNode>,
    },
    Toggle {
        id: String,
        label: String,
        checked: bool,
        #[serde(default)]
        disabled: bool,
    },
    Checkbox {
        id: String,
        label: String,
        checked: bool,
        #[serde(default)]
        disabled: bool,
    },
    Dropdown {
        id: String,
        options: Vec<String>,
        selected: usize,
        #[serde(default)]
        disabled: bool,
    },
    Divider {
        #[serde(default)]
        vertical: bool,
        #[serde(default)]
        color: Option<UiColor>,
    },
    Badge {
        text: String,
        #[serde(default)]
        color: Option<UiColor>,
        #[serde(default)]
        bg: Option<UiColor>,
    },
    Tabs {
        id: String,
        tabs: Vec<String>,
        active: usize,
    },
    Sparkline {
        values: Vec<f32>,
        width: f32,
        height: f32,
        #[serde(default)]
        color: Option<UiColor>,
    },
}

/// Interactive hit-target produced during UI layout/emission.
#[derive(Clone, Debug, PartialEq)]
pub struct UiHitTarget {
    pub widget_id: String,
    pub node_id: String,
    pub kind: HitKind,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitKind {
    Button,
    Slider,
    TextInput,
    Toggle,
    Checkbox,
    Dropdown,
    ScrollView,
    Tab(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLayer {
    Behind,
    Overlay,
    Panel,
}

/// Intrinsic size & flexibility constraints for a UI node.
#[derive(Clone, Copy, Debug)]
struct NodeMetrics {
    min_w: f32,
    pref_w: f32,
    min_h: f32,
    pref_h: f32,
    flex_x: f32,
}

/// Lay out a `UiNode` tree inside `(x, y, w, h)` in physical pixels and emit
/// primitives into `Scene` plus interactive hit targets.
#[allow(clippy::too_many_arguments)]
pub fn emit_ui_tree(
    widget_id: &str,
    node: &UiNode,
    rect: (f32, f32, f32, f32),
    scale: f32,
    opacity: f32,
    layer: UiLayer,
    theme: &Theme,
    renderer: &mut Renderer,
    scene: &mut Scene,
    hits: &mut Vec<UiHitTarget>,
) {
    emit_ui_tree_interactive(
        widget_id, node, rect, scale, opacity, layer, None, theme, renderer, scene, hits,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn emit_ui_tree_interactive(
    widget_id: &str,
    node: &UiNode,
    rect: (f32, f32, f32, f32),
    scale: f32,
    opacity: f32,
    layer: UiLayer,
    pointer_px: Option<(f32, f32)>,
    theme: &Theme,
    renderer: &mut Renderer,
    scene: &mut Scene,
    hits: &mut Vec<UiHitTarget>,
) {
    let ctx = EmitContext {
        widget_id,
        scale: scale.max(0.5),
        opacity: opacity.clamp(0.0, 1.0),
        layer,
        pointer_px,
        theme,
        palette: DesignPalette::from_theme(theme),
    };
    emit_node(&ctx, node, rect, rect, renderer, scene, hits);
}

struct EmitContext<'a> {
    widget_id: &'a str,
    scale: f32,
    opacity: f32,
    layer: UiLayer,
    pointer_px: Option<(f32, f32)>,
    theme: &'a Theme,
    palette: DesignPalette,
}

impl EmitContext<'_> {
    fn is_hovered(&self, rect: (f32, f32, f32, f32)) -> bool {
        self.pointer_px.is_some_and(|(px, py)| {
            px >= rect.0 && px <= rect.0 + rect.2 && py >= rect.1 && py <= rect.1 + rect.3
        })
    }

    fn push_rect(&self, scene: &mut Scene, rect: SceneRoundedRect) {
        let mut r = rect;
        r.color[3] *= self.opacity;
        r.border_color[3] *= self.opacity;
        r.shadow_color[3] *= self.opacity;
        match self.layer {
            UiLayer::Behind => scene.behind_rects.push(r),
            UiLayer::Overlay => scene.overlay_rounded.push(r),
            UiLayer::Panel => scene.panel_rounded.push(r),
        }
    }

    fn push_glyph(&self, scene: &mut Scene, mut glyph: SceneGlyph) {
        glyph.color[3] *= self.opacity;
        match self.layer {
            UiLayer::Behind => scene.behind_glyphs.push(glyph),
            UiLayer::Overlay => scene.overlay_glyphs.push(glyph),
            UiLayer::Panel => scene.panel_glyphs.push(glyph),
        }
    }

    fn push_image(&self, scene: &mut Scene, mut img: SceneImage) {
        img.alpha *= self.opacity;
        match self.layer {
            UiLayer::Behind => scene.behind_images.push(img),
            UiLayer::Overlay => scene.overlay_images.push(img),
            UiLayer::Panel => scene.panel_images.push(img),
        }
    }
}

fn measure_node(node: &UiNode, max_w: f32, scale: f32, renderer: &Renderer) -> NodeMetrics {
    match node {
        UiNode::Text { text, size, .. } => {
            let px = size.unwrap_or(12.5) * scale;
            let (tw, th) = renderer.measure_ui_text(text, px);
            let min_w = (px * 1.8).min(tw).min(max_w);
            NodeMetrics {
                min_w,
                pref_w: tw.min(max_w),
                min_h: th,
                pref_h: th,
                flex_x: 1.0,
            }
        }
        UiNode::Icon { size, .. } => {
            let px = size.unwrap_or(15.0) * scale;
            let sz = (px * 1.15).ceil();
            NodeMetrics {
                min_w: sz,
                pref_w: sz,
                min_h: sz,
                pref_h: sz,
                flex_x: 0.0,
            }
        }
        UiNode::Button {
            label,
            width,
            height,
            ..
        } => {
            let px = 12.0 * scale;
            let (tw, _) = if button_icon_name(label).is_some() {
                (14.0 * scale, 14.0 * scale)
            } else {
                renderer.measure_ui_text(label, px)
            };
            let pref_w = width
                .map(|v| v * scale)
                .unwrap_or_else(|| (tw + 18.0 * scale).max(26.0 * scale));
            let pref_h = height.map(|v| v * scale).unwrap_or(26.0 * scale);
            let min_w = width
                .map(|v| v * scale)
                .unwrap_or_else(|| (tw + 12.0 * scale).max(24.0 * scale))
                .min(max_w);
            NodeMetrics {
                min_w,
                pref_w: pref_w.min(max_w),
                min_h: pref_h.max(20.0 * scale),
                pref_h: pref_h.max(20.0 * scale),
                flex_x: 0.0,
            }
        }
        UiNode::Image { width, height, .. } => {
            let w = width * scale;
            let h = height * scale;
            NodeMetrics {
                min_w: w,
                pref_w: w,
                min_h: h,
                pref_h: h,
                flex_x: 0.0,
            }
        }
        UiNode::Container {
            padding,
            width,
            height,
            child,
            ..
        } => {
            let pad = padding * scale;
            let inner_max = width
                .map(|v| (v * scale - 2.0 * pad).max(0.0))
                .unwrap_or_else(|| (max_w - 2.0 * pad).max(0.0));
            let cm = measure_node(child, inner_max, scale, renderer);
            let pref_w = width.map(|v| v * scale).unwrap_or(cm.pref_w + 2.0 * pad);
            let min_w = width
                .map(|v| v * scale)
                .unwrap_or(cm.min_w + 2.0 * pad)
                .min(max_w);
            let pref_h = height.map(|v| v * scale).unwrap_or(cm.pref_h + 2.0 * pad);
            let min_h = height.map(|v| v * scale).unwrap_or(cm.min_h + 2.0 * pad);
            NodeMetrics {
                min_w,
                pref_w: pref_w.min(max_w),
                min_h,
                pref_h,
                flex_x: if width.is_some() { 0.0 } else { cm.flex_x },
            }
        }
        UiNode::Row { gap, children, .. } => {
            let g = gap * scale;
            let mut min_w = 0.0_f32;
            let mut pref_w = 0.0_f32;
            let mut min_h = 0.0_f32;
            let mut pref_h = 0.0_f32;
            let mut any_flex = false;
            for (i, c) in children.iter().enumerate() {
                let cm = measure_node(c, max_w, scale, renderer);
                let sep = if i > 0 { g } else { 0.0 };
                min_w += cm.min_w + sep;
                pref_w += cm.pref_w + sep;
                min_h = min_h.max(cm.min_h);
                pref_h = pref_h.max(cm.pref_h);
                if cm.flex_x > 0.0 {
                    any_flex = true;
                }
            }
            NodeMetrics {
                min_w: min_w.min(max_w),
                pref_w: pref_w.min(max_w),
                min_h,
                pref_h,
                flex_x: if any_flex { 1.0 } else { 0.0 },
            }
        }
        UiNode::Column { gap, children, .. } => {
            let g = gap * scale;
            let mut min_w = 0.0_f32;
            let mut pref_w = 0.0_f32;
            let mut min_h = 0.0_f32;
            let mut pref_h = 0.0_f32;
            for (i, c) in children.iter().enumerate() {
                let cm = measure_node(c, max_w, scale, renderer);
                let sep = if i > 0 { g } else { 0.0 };
                min_w = min_w.max(cm.min_w);
                pref_w = pref_w.max(cm.pref_w);
                min_h += cm.min_h + sep;
                pref_h += cm.pref_h + sep;
            }
            NodeMetrics {
                min_w: min_w.min(max_w),
                pref_w: pref_w.min(max_w),
                min_h,
                pref_h,
                flex_x: 1.0,
            }
        }
        UiNode::Grid {
            columns,
            gap,
            children,
        } => {
            let cols = (*columns).max(1);
            let g = gap * scale;
            let cell_w = ((max_w - g * (cols.saturating_sub(1)) as f32) / cols as f32).max(1.0);
            let rows = children.len().div_ceil(cols);
            let mut total_h = 0.0_f32;
            for r in 0..rows {
                let mut row_h = 0.0_f32;
                for c in 0..cols {
                    if let Some(child) = children.get(r * cols + c) {
                        let cm = measure_node(child, cell_w, scale, renderer);
                        row_h = row_h.max(cm.pref_h);
                    }
                }
                total_h += row_h + if r > 0 { g } else { 0.0 };
            }
            NodeMetrics {
                min_w: max_w.min(120.0 * scale),
                pref_w: max_w,
                min_h: total_h,
                pref_h: total_h,
                flex_x: 1.0,
            }
        }
        UiNode::Stack { children } => {
            let mut w = 0.0_f32;
            let mut h = 0.0_f32;
            for c in children {
                let cm = measure_node(c, max_w, scale, renderer);
                w = w.max(cm.pref_w);
                h = h.max(cm.pref_h);
            }
            NodeMetrics {
                min_w: w.min(max_w),
                pref_w: w.min(max_w),
                min_h: h,
                pref_h: h,
                flex_x: 0.0,
            }
        }
        UiNode::Progress { width, height, .. } | UiNode::Slider { width, height, .. } => {
            let h = height.map(|v| v * scale).unwrap_or(14.0 * scale);
            let pref_w = width.map(|v| v * scale).unwrap_or(max_w);
            let min_w = width.map(|v| v * scale).unwrap_or((48.0 * scale).min(max_w));
            NodeMetrics {
                min_w,
                pref_w: pref_w.min(max_w),
                min_h: h,
                pref_h: h,
                flex_x: if width.is_some() { 0.0 } else { 1.0 },
            }
        }
        UiNode::TextInput { width, .. } => {
            let h = 26.0 * scale;
            let pref_w = width.map(|v| v * scale).unwrap_or(max_w);
            let min_w = width.map(|v| v * scale).unwrap_or((56.0 * scale).min(max_w));
            NodeMetrics {
                min_w,
                pref_w: pref_w.min(max_w),
                min_h: h,
                pref_h: h,
                flex_x: if width.is_some() { 0.0 } else { 1.0 },
            }
        }
        UiNode::Canvas { width, height, .. } => {
            let w = width * scale;
            let h = height * scale;
            NodeMetrics {
                min_w: w,
                pref_w: w,
                min_h: h,
                pref_h: h,
                flex_x: 0.0,
            }
        }
        UiNode::ScrollView { height, .. } => {
            let h = height * scale;
            NodeMetrics {
                min_w: max_w.min(80.0 * scale),
                pref_w: max_w,
                min_h: h.min(60.0 * scale),
                pref_h: h,
                flex_x: 1.0,
            }
        }
        UiNode::Toggle { label, .. } | UiNode::Checkbox { label, .. } => {
            let (tw, _) = renderer.measure_ui_text(label, 12.0 * scale);
            let pref_w = (tw + 44.0 * scale).min(max_w);
            let h = 24.0 * scale;
            NodeMetrics {
                min_w: (64.0 * scale).min(max_w),
                pref_w,
                min_h: h,
                pref_h: h,
                flex_x: 1.0,
            }
        }
        UiNode::Dropdown {
            options, selected, ..
        } => {
            let cur = options.get(*selected).map_or("Select", String::as_str);
            let (tw, _) = renderer.measure_ui_text(cur, 12.0 * scale);
            let pref_w = (tw + 32.0 * scale).max(76.0 * scale).min(max_w);
            let h = 26.0 * scale;
            NodeMetrics {
                min_w: (68.0 * scale).min(max_w),
                pref_w,
                min_h: h,
                pref_h: h,
                flex_x: 1.0,
            }
        }
        UiNode::Divider { vertical, .. } => {
            if *vertical {
                NodeMetrics {
                    min_w: 1.0 * scale,
                    pref_w: 1.0 * scale,
                    min_h: 16.0 * scale,
                    pref_h: 16.0 * scale,
                    flex_x: 0.0,
                }
            } else {
                NodeMetrics {
                    min_w: 16.0 * scale,
                    pref_w: max_w,
                    min_h: 1.0 * scale,
                    pref_h: 1.0 * scale,
                    flex_x: 1.0,
                }
            }
        }
        UiNode::Badge { text, .. } => {
            let (tw, _) = renderer.measure_ui_text(text, 10.5 * scale);
            let w = (tw + 14.0 * scale).min(max_w);
            let h = 20.0 * scale;
            NodeMetrics {
                min_w: w,
                pref_w: w,
                min_h: h,
                pref_h: h,
                flex_x: 0.0,
            }
        }
        UiNode::Tabs { .. } => {
            let h = 28.0 * scale;
            NodeMetrics {
                min_w: max_w.min(120.0 * scale),
                pref_w: max_w,
                min_h: h,
                pref_h: h,
                flex_x: 1.0,
            }
        }
        UiNode::Sparkline { width, height, .. } => {
            let w = (width * scale).min(max_w);
            let h = height * scale;
            NodeMetrics {
                min_w: (48.0 * scale).min(w),
                pref_w: w,
                min_h: h,
                pref_h: h,
                flex_x: 1.0,
            }
        }
    }
}

fn button_icon_name(label: &str) -> Option<&'static str> {
    match label.trim() {
        "▶" => Some("play"),
        "⏸" => Some("pause"),
        "⏮" => Some("prev"),
        "⏭" => Some("next"),
        "✕" | "×" => Some("close"),
        "↻" => Some("refresh"),
        "↶" => Some("undo"),
        "↷" => Some("redo"),
        "▲" => Some("chevron-up"),
        "▼" => Some("chevron-down"),
        "✓" => Some("check"),
        _ => None,
    }
}

fn emit_vector_icon(
    ctx: &EmitContext<'_>,
    name: &str,
    rect: (f32, f32, f32, f32),
    clip: (f32, f32, f32, f32),
    size_px: f32,
    color: [f32; 4],
    scene: &mut Scene,
) {
    let sz = size_px.round().clamp(8.0, 64.0) as u32;
    let draw_sz = sz as f32;
    let ix = rect.0 + (rect.2 - draw_sz) * 0.5;
    let iy = rect.1 + (rect.3 - draw_sz) * 0.5;
    let (id, rgba) = icons::rasterize_icon(name, sz, color);
    ctx.push_image(
        scene,
        SceneImage {
            id,
            src_w: sz,
            src_h: sz,
            rgba,
            x: ix,
            y: iy,
            w: draw_sz,
            h: draw_sz,
            alpha: 1.0,
            radius: 0.0,
            clip: Some([clip.0, clip.1, clip.2, clip.3]),
        },
    );
}

fn emit_node(
    ctx: &EmitContext<'_>,
    node: &UiNode,
    rect: (f32, f32, f32, f32),
    clip: (f32, f32, f32, f32),
    renderer: &mut Renderer,
    scene: &mut Scene,
    hits: &mut Vec<UiHitTarget>,
) {
    let (x, y, w, h) = rect;
    if w <= 0.5 || h <= 0.5 {
        return;
    }
    let clip_arr = Some([clip.0, clip.1, clip.2, clip.3]);
    let fg_default = ctx.palette.fg;
    let accent_default = ctx.palette.accent;
    let control_default = ctx.palette.control_bg;

    match node {
        UiNode::Text {
            text,
            size,
            color,
            align,
        } => {
            let px = size.unwrap_or(12.5) * ctx.scale;
            let col = color
                .as_ref()
                .map_or(fg_default, |c| c.resolve(ctx.theme, fg_default));
            emit_text_line(
                ctx,
                text,
                px,
                (x, y, w, h),
                clip,
                *align,
                col,
                renderer,
                scene,
            );
        }
        UiNode::Icon { name, size, color } => {
            let px = size.unwrap_or(15.0) * ctx.scale;
            let col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            emit_vector_icon(ctx, name, (x, y, w, h), clip, px.min(w).min(h), col, scene);
        }
        UiNode::Button {
            id,
            label,
            bg,
            color,
            radius,
            ..
        } => {
            let hovered = ctx.is_hovered(rect);
            let mut bg_col = bg
                .as_ref()
                .map_or(control_default, |c| c.resolve(ctx.theme, control_default));
            if hovered {
                let hov = ctx.palette.control_hover;
                bg_col = [
                    (bg_col[0] * 0.55 + hov[0] * 0.45).clamp(0.0, 1.0),
                    (bg_col[1] * 0.55 + hov[1] * 0.45).clamp(0.0, 1.0),
                    (bg_col[2] * 0.55 + hov[2] * 0.45).clamp(0.0, 1.0),
                    bg_col[3].max(0.96),
                ];
            }
            let fg_col = color
                .as_ref()
                .map_or(fg_default, |c| c.resolve(ctx.theme, fg_default));
            let rad = if *radius > 0.0 {
                *radius * ctx.scale
            } else {
                6.0 * ctx.scale
            };
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: rad,
                    color: bg_col,
                    border_width: 1.0 * ctx.scale,
                    border_color: if hovered {
                        [accent_default[0], accent_default[1], accent_default[2], 0.72]
                    } else {
                        ctx.palette.border
                    },
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            if let Some(icon_name) = button_icon_name(label) {
                let icon_sz = (h * 0.56).clamp(10.0 * ctx.scale, 18.0 * ctx.scale);
                emit_vector_icon(ctx, icon_name, (x, y, w, h), clip, icon_sz, fg_col, scene);
            } else {
                emit_text_line(
                    ctx,
                    label,
                    12.0 * ctx.scale,
                    (x + 6.0 * ctx.scale, y, (w - 12.0 * ctx.scale).max(0.0), h),
                    clip,
                    Align::Center,
                    fg_col,
                    renderer,
                    scene,
                );
            }
            hits.push(UiHitTarget {
                widget_id: ctx.widget_id.to_string(),
                node_id: id.clone(),
                kind: HitKind::Button,
                x,
                y,
                w,
                h,
            });
        }
        UiNode::Image {
            id,
            radius,
            alpha,
            rgba,
            ..
        } => {
            if let Some((src_w, src_h, data)) = rgba {
                ctx.push_image(
                    scene,
                    SceneImage {
                        id: *id,
                        src_w: *src_w,
                        src_h: *src_h,
                        rgba: Arc::clone(data),
                        x,
                        y,
                        w,
                        h,
                        alpha: if *alpha <= 0.0 { 1.0 } else { *alpha },
                        radius: *radius * ctx.scale,
                        clip: clip_arr,
                    },
                );
            }
        }
        UiNode::Container {
            padding,
            bg,
            border,
            radius,
            child,
            ..
        } => {
            if bg.is_some() || border.is_some() {
                let bg_col = bg
                    .as_ref()
                    .map_or([0.0; 4], |c| c.resolve(ctx.theme, ctx.palette.card_bg));
                let border_col = border
                    .as_ref()
                    .map_or([0.0; 4], |c| c.resolve(ctx.theme, ctx.palette.border));
                let is_elevated = matches!(
                    bg,
                    Some(UiColor::Token(t)) if t == "surface-panel" || t == "surface-elevated"
                );
                let (shadow_blur, shadow_offset_y, shadow_color) = if is_elevated {
                    (14.0 * ctx.scale, 4.0 * ctx.scale, ctx.palette.shadow)
                } else {
                    (0.0, 0.0, [0.0; 4])
                };
                ctx.push_rect(
                    scene,
                    SceneRoundedRect {
                        x,
                        y,
                        w,
                        h,
                        radius: *radius * ctx.scale,
                        color: bg_col,
                        border_width: if border.is_some() {
                            1.0 * ctx.scale
                        } else {
                            0.0
                        },
                        border_color: border_col,
                        shadow_blur,
                        shadow_offset_y,
                        shadow_color,
                        clip: if is_elevated { None } else { clip_arr },
                    },
                );
            }
            let pad = padding * ctx.scale;
            let inner = (
                x + pad,
                y + pad,
                (w - 2.0 * pad).max(0.0),
                (h - 2.0 * pad).max(0.0),
            );
            emit_node(
                ctx,
                child,
                inner,
                intersect_clip(clip, inner),
                renderer,
                scene,
                hits,
            );
        }
        UiNode::Row {
            gap,
            align,
            children,
        } => {
            if children.is_empty() {
                return;
            }
            let g = gap * ctx.scale;
            let metrics: Vec<NodeMetrics> = children
                .iter()
                .map(|c| measure_node(c, w, ctx.scale, renderer))
                .collect();
            let total_gaps = g * children.len().saturating_sub(1) as f32;
            let avail_w = (w - total_gaps).max(0.0);

            // Constraint allocation: reserve fixed children's preferred widths first,
            // then distribute remaining width to flexible children (Text, Slider, Progress, etc.).
            let fixed_sum: f32 = metrics
                .iter()
                .filter(|m| m.flex_x <= 0.0)
                .map(|m| m.pref_w)
                .sum();
            let flex_count: f32 = metrics.iter().map(|m| m.flex_x).sum();
            let flex_min_sum: f32 = metrics
                .iter()
                .filter(|m| m.flex_x > 0.0)
                .map(|m| m.min_w)
                .sum();

            let widths: Vec<f32> = if fixed_sum + flex_min_sum <= avail_w && flex_count > 0.0 {
                let remaining_for_flex = (avail_w - fixed_sum).max(0.0);
                let flex_pref_sum: f32 = metrics
                    .iter()
                    .filter(|m| m.flex_x > 0.0)
                    .map(|m| m.pref_w)
                    .sum();
                metrics
                    .iter()
                    .map(|m| {
                        if m.flex_x <= 0.0 {
                            m.pref_w
                        } else if flex_pref_sum <= remaining_for_flex {
                            m.pref_w
                        } else {
                            let share = remaining_for_flex * (m.flex_x / flex_count);
                            share.max(m.min_w).min(m.pref_w.max(share))
                        }
                    })
                    .collect()
            } else {
                // Tight width: keep each child at least its min_w and never draw half-sliced buttons.
                metrics.iter().map(|m| m.pref_w.min(avail_w)).collect()
            };

            let mut cx = x;
            for (child, (m, alloc_w)) in children.iter().zip(metrics.iter().zip(widths)) {
                let rem = (x + w - cx).max(0.0);
                if rem < m.min_w * 0.85 {
                    break;
                }
                let child_w = alloc_w.min(rem);
                // Never crush a child vertically below its min_h!
                let child_h = if *align == Align::Stretch {
                    h.max(m.min_h).min(h)
                } else {
                    m.pref_h.min(h)
                };
                if child_h < m.min_h * 0.72 {
                    continue;
                }
                let cy = match align {
                    Align::Center => y + (h - child_h).max(0.0) * 0.5,
                    Align::End => y + (h - child_h).max(0.0),
                    _ => y,
                };
                emit_node(
                    ctx,
                    child,
                    (cx, cy, child_w, child_h),
                    intersect_clip(clip, (cx, cy, child_w, child_h)),
                    renderer,
                    scene,
                    hits,
                );
                cx += child_w + g;
                if cx >= x + w {
                    break;
                }
            }
        }
        UiNode::Column {
            gap,
            align,
            children,
        } => {
            if children.is_empty() {
                return;
            }
            let g = gap * ctx.scale;
            let mut cy = y;
            for child in children {
                let m = measure_node(child, w, ctx.scale, renderer);
                let rem_h = (y + h - cy).max(0.0);
                // Never vertically squash buttons or text below their readable minimum height!
                if rem_h < m.min_h * 0.85 {
                    break;
                }
                let child_w = if *align == Align::Stretch {
                    w
                } else {
                    m.pref_w.min(w)
                };
                let cx = match align {
                    Align::Center => x + (w - child_w).max(0.0) * 0.5,
                    Align::End => x + (w - child_w).max(0.0),
                    _ => x,
                };
                let child_h = m.pref_h.min(rem_h).max(m.min_h.min(rem_h));
                emit_node(
                    ctx,
                    child,
                    (cx, cy, child_w, child_h),
                    intersect_clip(clip, (cx, cy, child_w, child_h)),
                    renderer,
                    scene,
                    hits,
                );
                cy += child_h + g;
                if cy >= y + h {
                    break;
                }
            }
        }
        UiNode::Grid {
            columns,
            gap,
            children,
        } => {
            let cols = (*columns).max(1);
            let g = gap * ctx.scale;
            let cell_w = ((w - g * (cols.saturating_sub(1)) as f32) / cols as f32).max(1.0);
            let mut cy = y;
            for row in children.chunks(cols) {
                let mut row_h = 0.0_f32;
                let mut row_min_h = 0.0_f32;
                for c in row {
                    let cm = measure_node(c, cell_w, ctx.scale, renderer);
                    row_h = row_h.max(cm.pref_h);
                    row_min_h = row_min_h.max(cm.min_h);
                }
                let rem_h = (y + h - cy).max(0.0);
                if rem_h < row_min_h * 0.85 {
                    break;
                }
                let ch = row_h.min(rem_h);
                for (col_idx, child) in row.iter().enumerate() {
                    let cx = x + col_idx as f32 * (cell_w + g);
                    emit_node(
                        ctx,
                        child,
                        (cx, cy, cell_w, ch),
                        intersect_clip(clip, (cx, cy, cell_w, ch)),
                        renderer,
                        scene,
                        hits,
                    );
                }
                cy += ch + g;
                if cy >= y + h {
                    break;
                }
            }
        }
        UiNode::Stack { children } => {
            for child in children {
                emit_node(ctx, child, rect, clip, renderer, scene, hits);
            }
        }
        UiNode::Progress {
            value,
            color,
            track,
            ..
        } => {
            let v = value.clamp(0.0, 1.0);
            let bar_h = h.min(6.0 * ctx.scale).max(4.0);
            let bar_y = y + (h - bar_h) * 0.5;
            let track_col = track.as_ref().map_or(ctx.palette.border, |c| {
                c.resolve(ctx.theme, ctx.palette.border)
            });
            let fill_col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y: bar_y,
                    w,
                    h: bar_h,
                    radius: bar_h * 0.5,
                    color: track_col,
                    border_width: 0.0,
                    border_color: [0.0; 4],
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            if v > 0.0 {
                ctx.push_rect(
                    scene,
                    SceneRoundedRect {
                        x,
                        y: bar_y,
                        w: (w * v).max(bar_h).min(w),
                        h: bar_h,
                        radius: bar_h * 0.5,
                        color: fill_col,
                        border_width: 0.0,
                        border_color: [0.0; 4],
                        shadow_blur: 0.0,
                        shadow_offset_y: 0.0,
                        shadow_color: [0.0; 4],
                        clip: clip_arr,
                    },
                );
            }
        }
        UiNode::Slider {
            id,
            value,
            color,
            track,
            ..
        } => {
            let hovered = ctx.is_hovered(rect);
            let v = value.clamp(0.0, 1.0);
            let bar_h = (h * 0.34).clamp(4.0, 6.0 * ctx.scale);
            let bar_y = y + (h - bar_h) * 0.5;
            let track_col = track.as_ref().map_or(ctx.palette.border, |c| {
                c.resolve(ctx.theme, ctx.palette.border)
            });
            let fill_col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y: bar_y,
                    w,
                    h: bar_h,
                    radius: bar_h * 0.5,
                    color: track_col,
                    border_width: 0.0,
                    border_color: [0.0; 4],
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y: bar_y,
                    w: (w * v).max(bar_h).min(w),
                    h: bar_h,
                    radius: bar_h * 0.5,
                    color: fill_col,
                    border_width: 0.0,
                    border_color: [0.0; 4],
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            let knob = if hovered {
                (13.0 * ctx.scale).min(h)
            } else {
                (11.0 * ctx.scale).min(h)
            };
            let kx = (x + w * v - knob * 0.5).clamp(x, (x + w - knob).max(x));
            let ky = y + (h - knob) * 0.5;
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x: kx,
                    y: ky,
                    w: knob,
                    h: knob,
                    radius: knob * 0.5,
                    color: fg_default,
                    border_width: 1.5 * ctx.scale,
                    border_color: fill_col,
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            hits.push(UiHitTarget {
                widget_id: ctx.widget_id.to_string(),
                node_id: id.clone(),
                kind: HitKind::Slider,
                x,
                y,
                w,
                h,
            });
        }
        UiNode::TextInput {
            id,
            value,
            placeholder,
            focused,
            disabled,
            ..
        } => {
            let hovered = ctx.is_hovered(rect);
            let border_col = if *focused {
                accent_default
            } else if hovered && !*disabled {
                ctx.palette.border_strong
            } else {
                ctx.palette.border
            };
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: 6.0 * ctx.scale,
                    color: ctx.palette.control_bg,
                    border_width: if *focused { 1.5 } else { 1.0 } * ctx.scale,
                    border_color: border_col,
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            let display = if value.is_empty() {
                placeholder.as_str()
            } else {
                value.as_str()
            };
            let text_col = if value.is_empty() || *disabled {
                ctx.palette.fg_secondary
            } else {
                fg_default
            };
            let shown = if *focused {
                format!("{value}|")
            } else {
                display.to_string()
            };
            emit_text_line(
                ctx,
                &shown,
                12.0 * ctx.scale,
                (x + 8.0 * ctx.scale, y, (w - 16.0 * ctx.scale).max(0.0), h),
                clip,
                Align::Start,
                text_col,
                renderer,
                scene,
            );
            if !*disabled {
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::TextInput,
                    x,
                    y,
                    w,
                    h,
                });
            }
        }
        UiNode::Canvas { commands, .. } => {
            for cmd in commands {
                match cmd {
                    CanvasCommand::Clear { color } => {
                        ctx.push_rect(
                            scene,
                            SceneRoundedRect {
                                x,
                                y,
                                w,
                                h,
                                radius: 0.0,
                                color: color.resolve(ctx.theme, [0.0; 4]),
                                border_width: 0.0,
                                border_color: [0.0; 4],
                                shadow_blur: 0.0,
                                shadow_offset_y: 0.0,
                                shadow_color: [0.0; 4],
                                clip: clip_arr,
                            },
                        );
                    }
                    CanvasCommand::FillRect {
                        x: rx,
                        y: ry,
                        w: rw,
                        h: rh,
                        color,
                        radius,
                    } => {
                        let px = (x + rx * ctx.scale).clamp(x, x + w);
                        let py = (y + ry * ctx.scale).clamp(y, y + h);
                        let pw = (rw * ctx.scale).min(x + w - px).max(0.0);
                        let ph = (rh * ctx.scale).min(y + h - py).max(0.0);
                        if pw > 0.0 && ph > 0.0 {
                            ctx.push_rect(
                                scene,
                                SceneRoundedRect {
                                    x: px,
                                    y: py,
                                    w: pw,
                                    h: ph,
                                    radius: *radius * ctx.scale,
                                    color: color.resolve(ctx.theme, accent_default),
                                    border_width: 0.0,
                                    border_color: [0.0; 4],
                                    shadow_blur: 0.0,
                                    shadow_offset_y: 0.0,
                                    shadow_color: [0.0; 4],
                                    clip: clip_arr,
                                },
                            );
                        }
                    }
                    CanvasCommand::FillCircle { cx, cy, r, color } => {
                        let rad = r * ctx.scale;
                        let px = (x + cx * ctx.scale - rad).clamp(x, x + w);
                        let py = (y + cy * ctx.scale - rad).clamp(y, y + h);
                        let d = (rad * 2.0).min(x + w - px).min(y + h - py).max(0.0);
                        if d > 0.0 {
                            ctx.push_rect(
                                scene,
                                SceneRoundedRect {
                                    x: px,
                                    y: py,
                                    w: d,
                                    h: d,
                                    radius: d * 0.5,
                                    color: color.resolve(ctx.theme, accent_default),
                                    border_width: 0.0,
                                    border_color: [0.0; 4],
                                    shadow_blur: 0.0,
                                    shadow_offset_y: 0.0,
                                    shadow_color: [0.0; 4],
                                    clip: clip_arr,
                                },
                            );
                        }
                    }
                    CanvasCommand::Line {
                        x1,
                        y1,
                        x2,
                        y2,
                        width,
                        color,
                    } => {
                        let min_x = x + x1.min(*x2) * ctx.scale;
                        let min_y = y + y1.min(*y2) * ctx.scale;
                        let lw = ((x2 - x1).abs() * ctx.scale).max(width * ctx.scale);
                        let lh = ((y2 - y1).abs() * ctx.scale).max(width * ctx.scale);
                        ctx.push_rect(
                            scene,
                            SceneRoundedRect {
                                x: min_x.clamp(x, x + w),
                                y: min_y.clamp(y, y + h),
                                w: lw.min(w),
                                h: lh.min(h),
                                radius: 0.0,
                                color: color.resolve(ctx.theme, fg_default),
                                border_width: 0.0,
                                border_color: [0.0; 4],
                                shadow_blur: 0.0,
                                shadow_offset_y: 0.0,
                                shadow_color: [0.0; 4],
                                clip: clip_arr,
                            },
                        );
                    }
                }
            }
        }
        UiNode::ScrollView {
            id,
            scroll_y,
            children,
            ..
        } => {
            let view_clip = intersect_clip(clip, rect);
            let mut cy = y - scroll_y * ctx.scale;
            for child in children {
                let cm = measure_node(child, w, ctx.scale, renderer);
                if cy + cm.pref_h >= y && cy <= y + h {
                    emit_node(
                        ctx,
                        child,
                        (x, cy, w, cm.pref_h),
                        view_clip,
                        renderer,
                        scene,
                        hits,
                    );
                }
                cy += cm.pref_h + 6.0 * ctx.scale;
            }
            if !id.is_empty() {
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::ScrollView,
                    x,
                    y,
                    w,
                    h,
                });
            }
        }
        UiNode::Checkbox {
            id,
            label,
            checked,
            disabled,
        } => {
            let box_sz = (16.0 * ctx.scale).min(h);
            let box_y = y + (h - box_sz) * 0.5;
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y: box_y,
                    w: box_sz,
                    h: box_sz,
                    radius: 4.5 * ctx.scale,
                    color: if *checked {
                        accent_default
                    } else {
                        ctx.palette.control_bg
                    },
                    border_width: 1.0 * ctx.scale,
                    border_color: if *checked {
                        accent_default
                    } else {
                        ctx.palette.border_strong
                    },
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            if *checked {
                emit_vector_icon(
                    ctx,
                    "check",
                    (x, box_y, box_sz, box_sz),
                    clip,
                    box_sz * 0.72,
                    to_f4(ctx.theme.background, 1.0),
                    scene,
                );
            }
            let text_x = x + box_sz + 8.0 * ctx.scale;
            let text_col = if *disabled {
                ctx.palette.fg_secondary
            } else {
                fg_default
            };
            emit_text_line(
                ctx,
                label,
                12.0 * ctx.scale,
                (text_x, y, (x + w - text_x).max(0.0), h),
                clip,
                Align::Start,
                text_col,
                renderer,
                scene,
            );
            if !*disabled {
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::Checkbox,
                    x,
                    y,
                    w,
                    h,
                });
            }
        }
        UiNode::Dropdown {
            id,
            options,
            selected,
            disabled,
        } => {
            let hovered = ctx.is_hovered(rect);
            let label = options.get(*selected).map_or("—", String::as_str);
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: 6.0 * ctx.scale,
                    color: if hovered && !*disabled {
                        ctx.palette.control_hover
                    } else {
                        ctx.palette.control_bg
                    },
                    border_width: 1.0 * ctx.scale,
                    border_color: if hovered && !*disabled {
                        [accent_default[0], accent_default[1], accent_default[2], 0.75]
                    } else {
                        ctx.palette.border
                    },
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            let col = if *disabled {
                ctx.palette.fg_secondary
            } else {
                fg_default
            };
            let chev_w = 18.0 * ctx.scale;
            emit_text_line(
                ctx,
                label,
                12.0 * ctx.scale,
                (x + 8.0 * ctx.scale, y, (w - chev_w - 10.0 * ctx.scale).max(0.0), h),
                clip,
                Align::Start,
                col,
                renderer,
                scene,
            );
            emit_vector_icon(
                ctx,
                "chevron-down",
                (x + w - chev_w - 2.0 * ctx.scale, y, chev_w, h),
                clip,
                11.0 * ctx.scale,
                ctx.palette.fg_secondary,
                scene,
            );
            if !*disabled {
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::Dropdown,
                    x,
                    y,
                    w,
                    h,
                });
            }
        }
        UiNode::Toggle {
            id,
            label,
            checked,
            disabled,
        } => {
            let hovered = ctx.is_hovered(rect);
            let pill_w = (32.0 * ctx.scale).min(w * 0.45);
            let pill_h = (17.0 * ctx.scale).min(h);
            let pill_y = y + (h - pill_h) * 0.5;
            let track_col = if *checked {
                accent_default
            } else if hovered && !*disabled {
                ctx.palette.control_hover
            } else {
                ctx.palette.control_bg
            };
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y: pill_y,
                    w: pill_w,
                    h: pill_h,
                    radius: pill_h * 0.5,
                    color: track_col,
                    border_width: if *checked { 0.0 } else { 1.0 * ctx.scale },
                    border_color: ctx.palette.border_strong,
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            let knob = (pill_h - 4.0 * ctx.scale).max(6.0);
            let kx = if *checked {
                x + pill_w - knob - 2.0 * ctx.scale
            } else {
                x + 2.0 * ctx.scale
            };
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x: kx,
                    y: pill_y + (pill_h - knob) * 0.5,
                    w: knob,
                    h: knob,
                    radius: knob * 0.5,
                    color: if *checked {
                        to_f4(ctx.theme.background, 1.0)
                    } else {
                        fg_default
                    },
                    border_width: 0.0,
                    border_color: [0.0; 4],
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            let text_x = x + pill_w + 8.0 * ctx.scale;
            let text_col = if *disabled {
                ctx.palette.fg_secondary
            } else {
                fg_default
            };
            emit_text_line(
                ctx,
                label,
                12.0 * ctx.scale,
                (text_x, y, (x + w - text_x).max(0.0), h),
                clip,
                Align::Start,
                text_col,
                renderer,
                scene,
            );
            if !*disabled {
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::Toggle,
                    x,
                    y,
                    w,
                    h,
                });
            }
        }
        UiNode::Divider { vertical, color } => {
            let col = color.as_ref().map_or(ctx.palette.border_subtle, |c| {
                c.resolve(ctx.theme, ctx.palette.border_subtle)
            });
            let (dw, dh) = if *vertical {
                (1.0 * ctx.scale, h)
            } else {
                (w, 1.0 * ctx.scale)
            };
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w: dw,
                    h: dh,
                    radius: 0.0,
                    color: col,
                    border_width: 0.0,
                    border_color: [0.0; 4],
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
        }
        UiNode::Badge { text, color, bg } => {
            let fg_col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            let bg_col = bg
                .as_ref()
                .map_or([fg_col[0], fg_col[1], fg_col[2], 0.16], |c| {
                    c.resolve(ctx.theme, [fg_col[0], fg_col[1], fg_col[2], 0.16])
                });
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: h * 0.5,
                    color: bg_col,
                    border_width: 1.0 * ctx.scale,
                    border_color: [fg_col[0], fg_col[1], fg_col[2], 0.36],
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            emit_text_line(
                ctx,
                text,
                10.5 * ctx.scale,
                (x + 5.0 * ctx.scale, y, (w - 10.0 * ctx.scale).max(0.0), h),
                clip,
                Align::Center,
                fg_col,
                renderer,
                scene,
            );
        }
        UiNode::Tabs { id, tabs, active } => {
            if tabs.is_empty() {
                return;
            }
            // Segmented control pill background.
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: 7.0 * ctx.scale,
                    color: ctx.palette.control_bg,
                    border_width: 1.0 * ctx.scale,
                    border_color: ctx.palette.border_subtle,
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            let pad = 2.5 * ctx.scale;
            let gap = 2.0 * ctx.scale;
            let inner_w = (w - pad * 2.0).max(0.0);
            let tab_w =
                ((inner_w - gap * (tabs.len().saturating_sub(1)) as f32) / tabs.len() as f32)
                    .max(24.0 * ctx.scale);
            for (idx, label) in tabs.iter().enumerate() {
                let tx = x + pad + idx as f32 * (tab_w + gap);
                if tx >= x + w - pad {
                    break;
                }
                let tw = tab_w.min(x + w - pad - tx);
                let ty = y + pad;
                let th = (h - pad * 2.0).max(1.0);
                let is_active = idx == *active;
                let hovered = ctx.is_hovered((tx, ty, tw, th));
                let bg_col = if is_active {
                    ctx.palette.accent_soft
                } else if hovered {
                    ctx.palette.control_hover
                } else {
                    [0.0, 0.0, 0.0, 0.0]
                };
                if bg_col[3] > 0.0 || is_active {
                    ctx.push_rect(
                        scene,
                        SceneRoundedRect {
                            x: tx,
                            y: ty,
                            w: tw,
                            h: th,
                            radius: 5.0 * ctx.scale,
                            color: bg_col,
                            border_width: if is_active { 1.0 * ctx.scale } else { 0.0 },
                            border_color: [accent_default[0], accent_default[1], accent_default[2], 0.55],
                            shadow_blur: 0.0,
                            shadow_offset_y: 0.0,
                            shadow_color: [0.0; 4],
                            clip: clip_arr,
                        },
                    );
                }
                emit_text_line(
                    ctx,
                    label,
                    11.5 * ctx.scale,
                    (tx + 4.0 * ctx.scale, ty, (tw - 8.0 * ctx.scale).max(0.0), th),
                    clip,
                    Align::Center,
                    if is_active {
                        fg_default
                    } else {
                        ctx.palette.fg_secondary
                    },
                    renderer,
                    scene,
                );
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::Tab(idx),
                    x: tx,
                    y: ty,
                    w: tw,
                    h: th,
                });
            }
        }
        UiNode::Sparkline { values, color, .. } => {
            let line_col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: 5.0 * ctx.scale,
                    color: ctx.palette.control_bg,
                    border_width: 1.0 * ctx.scale,
                    border_color: ctx.palette.border_subtle,
                    shadow_blur: 0.0,
                    shadow_offset_y: 0.0,
                    shadow_color: [0.0; 4],
                    clip: clip_arr,
                },
            );
            if values.is_empty() {
                return;
            }
            let pad = 3.0 * ctx.scale;
            let iw = (w - pad * 2.0).max(1.0);
            let ih = (h - pad * 2.0).max(1.0);
            let max_v = values.iter().copied().fold(0.001_f32, f32::max);
            let n = values.len();
            let bar_w = (iw / n as f32).max(2.0);
            for (i, &v) in values.iter().enumerate() {
                let norm = (v / max_v).clamp(0.0, 1.0);
                let bh = (ih * norm).max(2.0);
                let bx = x + pad + i as f32 * bar_w;
                let bw = (bar_w - 1.5).max(1.5).min((x + w - pad - bx).max(0.0));
                if bw > 0.0 {
                    ctx.push_rect(
                        scene,
                        SceneRoundedRect {
                            x: bx,
                            y: y + pad + ih - bh,
                            w: bw,
                            h: bh,
                            radius: (bw * 0.4).min(2.0 * ctx.scale),
                            color: [line_col[0], line_col[1], line_col[2], 0.85],
                            border_width: 0.0,
                            border_color: [0.0; 4],
                            shadow_blur: 0.0,
                            shadow_offset_y: 0.0,
                            shadow_color: [0.0; 4],
                            clip: clip_arr,
                        },
                    );
                }
            }
        }
    }
}

fn intersect_clip(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

#[allow(clippy::too_many_arguments)]
fn emit_text_line(
    ctx: &EmitContext<'_>,
    text: &str,
    px_size: f32,
    rect: (f32, f32, f32, f32),
    clip: (f32, f32, f32, f32),
    align: Align,
    color: [f32; 4],
    renderer: &mut Renderer,
    scene: &mut Scene,
) {
    let (rx, ry, rw, rh) = rect;
    let clip = intersect_clip(clip, rect);
    if clip.2 <= 0.5 || clip.3 <= 0.5 || text.is_empty() {
        return;
    }
    let max_text_w = clip.2.min(rw).max(0.0);
    let (full_w, _) = renderer.measure_ui_text(text, px_size);
    let display_text: std::borrow::Cow<'_, str> = if full_w > max_text_w + 0.5 {
        let (ell_w, _) = renderer.measure_ui_text("…", px_size);
        let budget = (max_text_w - ell_w).max(0.0);
        let mut acc = 0.0_f32;
        let mut s = String::new();
        for ch in text.chars() {
            let (cw, _) = renderer.measure_ui_text(&ch.to_string(), px_size);
            if acc + cw > budget {
                break;
            }
            acc += cw;
            s.push(ch);
        }
        if !s.is_empty() {
            s.push('…');
            std::borrow::Cow::Owned(s)
        } else {
            std::borrow::Cow::Borrowed("…")
        }
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let (actual_w, _) = renderer.measure_ui_text(&display_text, px_size);
    let start_x = match align {
        Align::Center => rx + (rw - actual_w).max(0.0) * 0.5,
        Align::End => rx + (rw - actual_w).max(0.0),
        _ => rx,
    };
    let baseline = ry + (rh + px_size * 0.70) * 0.5;
    let mut pen_x = start_x;
    for ch in display_text.chars() {
        if pen_x > clip.0 + clip.2 {
            break;
        }
        if ch != ' ' {
            let (metrics, _) = renderer.glyph_bitmap_kind(ch, px_size, FontKind::Ui);
            if metrics.width > 0 && metrics.height > 0 {
                let gx = (pen_x + metrics.xmin as f32).round() as isize;
                let gy = (baseline - metrics.height as f32 - metrics.ymin as f32).round() as isize;
                let clip_x0 = clip.0.floor() as isize;
                let clip_y0 = clip.1.floor() as isize;
                let clip_x1 = (clip.0 + clip.2).ceil() as isize;
                let clip_y1 = (clip.1 + clip.3).ceil() as isize;
                let x0 = gx.max(clip_x0);
                let y0 = gy.max(clip_y0);
                let x1 = (gx + metrics.width as isize).min(clip_x1);
                let y1 = (gy + metrics.height as isize).min(clip_y1);
                if x1 > x0 && y1 > y0 {
                    ctx.push_glyph(
                        scene,
                        SceneGlyph {
                            ch,
                            px_size,
                            font_kind: FontKind::Ui,
                            x: x0 as usize,
                            y: y0 as usize,
                            w: (x1 - x0) as usize,
                            h: (y1 - y0) as usize,
                            source: ((x0 - gx) as usize, (y0 - gy) as usize),
                            color,
                        },
                    );
                }
            }
            pen_x += metrics.advance_width.max(px_size * 0.28);
        } else {
            let (metrics, _) = renderer.glyph_bitmap_kind(' ', px_size, FontKind::Ui);
            pen_x += if metrics.advance_width > 0.5 {
                metrics.advance_width
            } else {
                px_size * 0.32
            };
        }
    }
}

/// Emit visual editor handles (selection border, corner & edge resize handles) into `Scene`.
pub fn emit_editor_chrome(
    scene: &mut Scene,
    rect: (f32, f32, f32, f32),
    selected: bool,
    scale: f32,
) {
    let (x, y, w, h) = rect;
    let accent = [0.38, 0.78, 0.98, 0.95];
    let border = if selected {
        accent
    } else {
        [1.0, 1.0, 1.0, 0.28]
    };
    scene.overlay_rounded.push(SceneRoundedRect {
        x: x - 1.5 * scale,
        y: y - 1.5 * scale,
        w: w + 3.0 * scale,
        h: h + 3.0 * scale,
        radius: 10.0 * scale,
        color: if selected {
            [0.38, 0.78, 0.98, 0.06]
        } else {
            [0.0, 0.0, 0.0, 0.0]
        },
        border_width: if selected { 2.0 } else { 1.0 } * scale,
        border_color: border,
        shadow_blur: 0.0,
        shadow_offset_y: 0.0,
        shadow_color: [0.0; 4],
        clip: None,
    });
    if selected {
        let hs = 8.0 * scale;
        let corners = [
            (x - hs * 0.5, y - hs * 0.5),
            (x + w - hs * 0.5, y - hs * 0.5),
            (x - hs * 0.5, y + h - hs * 0.5),
            (x + w - hs * 0.5, y + h - hs * 0.5),
        ];
        for (hx, hy) in corners {
            scene.overlay_rounded.push(SceneRoundedRect {
                x: hx,
                y: hy,
                w: hs,
                h: hs,
                radius: hs * 0.5,
                color: [1.0, 1.0, 1.0, 1.0],
                border_width: 1.5 * scale,
                border_color: accent,
                shadow_blur: 0.0,
                shadow_offset_y: 0.0,
                shadow_color: [0.0; 4],
                clip: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_tree_emits_rects_glyphs_and_hit_targets() {
        let mut renderer = Renderer::new().unwrap();
        let theme = Theme::default();
        let mut scene = Scene::default();
        let mut hits = Vec::new();
        let tree = UiNode::Container {
            padding: 8.0,
            bg: Some(UiColor::Token("card".into())),
            border: Some(UiColor::Token("accent".into())),
            radius: 8.0,
            width: Some(220.0),
            height: Some(64.0),
            child: Box::new(UiNode::Row {
                gap: 8.0,
                align: Align::Center,
                children: vec![
                    UiNode::Text {
                        text: "EN".into(),
                        size: Some(14.0),
                        color: None,
                        align: Align::Start,
                    },
                    UiNode::Button {
                        id: "play".into(),
                        label: "Play".into(),
                        bg: None,
                        color: None,
                        width: None,
                        height: None,
                        radius: 4.0,
                    },
                ],
            }),
        };
        emit_ui_tree(
            "test-widget",
            &tree,
            (10.0, 10.0, 220.0, 64.0),
            1.25,
            1.0,
            UiLayer::Overlay,
            &theme,
            &mut renderer,
            &mut scene,
            &mut hits,
        );
        assert!(!scene.overlay_rounded.is_empty());
        assert!(!scene.overlay_glyphs.is_empty());
        assert!(
            scene
                .overlay_glyphs
                .iter()
                .all(|g| g.font_kind == FontKind::Ui)
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node_id, "play");
    }

    #[test]
    fn constraint_layout_never_squishes_buttons_vertically() {
        let mut renderer = Renderer::new().unwrap();
        let theme = Theme::default();
        let mut scene = Scene::default();
        let mut hits = Vec::new();
        let tree = UiNode::Container {
            padding: 8.0,
            bg: Some(UiColor::Token("surface-elevated".into())),
            border: Some(UiColor::Token("border".into())),
            radius: 10.0,
            width: None,
            height: None,
            child: Box::new(UiNode::Column {
                gap: 6.0,
                align: Align::Stretch,
                children: vec![
                    UiNode::Text {
                        text: "Track Title".into(),
                        size: Some(13.0),
                        color: None,
                        align: Align::Start,
                    },
                    UiNode::Text {
                        text: "Artist Name".into(),
                        size: Some(11.0),
                        color: None,
                        align: Align::Start,
                    },
                    UiNode::Button {
                        id: "play".into(),
                        label: "▶".into(),
                        bg: None,
                        color: None,
                        width: Some(30.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                ],
            }),
        };
        // Intentionally give a tight height (56px) where the 3rd row cannot fit without squishing.
        emit_ui_tree(
            "tight-media",
            &tree,
            (0.0, 0.0, 200.0, 56.0),
            1.0,
            1.0,
            UiLayer::Overlay,
            &theme,
            &mut renderer,
            &mut scene,
            &mut hits,
        );
        // Any emitted button MUST maintain its minimum height (>= 20px), never squished to 4px.
        for hit in &hits {
            assert!(
                hit.h >= 20.0,
                "button {} was squished to h={}",
                hit.node_id,
                hit.h
            );
        }
    }
}
