// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Declarative Native UI tree, layout solver, hit-testing, and Scene emission
//! for BeyrixWidgets and native overlays (such as X-Ray).

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::render::{Renderer, Scene, SceneGlyph, SceneImage, SceneRect, SceneRoundedRect};
use crate::theme::Theme;

/// Color specification in UI nodes: either a theme token or explicit RGBA.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiColor {
    Token(String),
    Rgba([f32; 4]),
}

impl UiColor {
    pub fn resolve(&self, theme: &Theme, default: [f32; 4]) -> [f32; 4] {
        match self {
            Self::Rgba(c) => *c,
            Self::Token(name) => match name.as_str() {
                "background" | "bg" => to_f4(theme.background, 1.0),
                "foreground" | "fg" => to_f4(theme.foreground, 1.0),
                "cursor" | "accent" => to_f4(theme.cursor, 1.0),
                "selection" => to_f4(theme.selection, 1.0),
                "selection_fg" => to_f4(theme.selection_foreground, 1.0),
                "muted" | "fg-secondary" => {
                    let bg = theme.background;
                    let fg = theme.foreground;
                    [
                        (bg[0] as f32 * 0.38 + fg[0] as f32 * 0.62) / 255.0,
                        (bg[1] as f32 * 0.38 + fg[1] as f32 * 0.62) / 255.0,
                        (bg[2] as f32 * 0.38 + fg[2] as f32 * 0.62) / 255.0,
                        1.0,
                    ]
                }
                "card" | "surface" => {
                    let bg = theme.background;
                    let fg = theme.foreground;
                    [
                        (bg[0] as f32 * 0.90 + fg[0] as f32 * 0.10) / 255.0,
                        (bg[1] as f32 * 0.90 + fg[1] as f32 * 0.10) / 255.0,
                        (bg[2] as f32 * 0.90 + fg[2] as f32 * 0.10) / 255.0,
                        0.95,
                    ]
                }
                "surface-elevated" => {
                    let bg = theme.background;
                    let fg = theme.foreground;
                    [
                        (bg[0] as f32 * 0.84 + fg[0] as f32 * 0.16) / 255.0,
                        (bg[1] as f32 * 0.84 + fg[1] as f32 * 0.16) / 255.0,
                        (bg[2] as f32 * 0.84 + fg[2] as f32 * 0.16) / 255.0,
                        0.97,
                    ]
                }
                "surface-hover" => {
                    let bg = theme.background;
                    let fg = theme.foreground;
                    [
                        (bg[0] as f32 * 0.78 + fg[0] as f32 * 0.22) / 255.0,
                        (bg[1] as f32 * 0.78 + fg[1] as f32 * 0.22) / 255.0,
                        (bg[2] as f32 * 0.78 + fg[2] as f32 * 0.22) / 255.0,
                        0.96,
                    ]
                }
                "border" => {
                    let bg = theme.background;
                    let fg = theme.foreground;
                    [
                        (bg[0] as f32 * 0.68 + fg[0] as f32 * 0.32) / 255.0,
                        (bg[1] as f32 * 0.68 + fg[1] as f32 * 0.32) / 255.0,
                        (bg[2] as f32 * 0.68 + fg[2] as f32 * 0.32) / 255.0,
                        0.55,
                    ]
                }
                "border-subtle" => {
                    let fg = theme.foreground;
                    [
                        fg[0] as f32 / 255.0,
                        fg[1] as f32 / 255.0,
                        fg[2] as f32 / 255.0,
                        0.16,
                    ]
                }
                "accent-soft" => to_f4(theme.cursor, 0.22),
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
        scale,
        opacity: opacity.clamp(0.0, 1.0),
        layer,
        pointer_px,
        theme,
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
        match self.layer {
            UiLayer::Behind => scene.behind_rects.push(r),
            UiLayer::Overlay => scene.overlay_rounded.push(r),
        }
    }

    fn push_glyph(&self, scene: &mut Scene, mut glyph: SceneGlyph) {
        glyph.color[3] *= self.opacity;
        match self.layer {
            UiLayer::Behind => scene.behind_glyphs.push(glyph),
            UiLayer::Overlay => scene.overlay_glyphs.push(glyph),
        }
    }

    fn push_image(&self, scene: &mut Scene, mut img: SceneImage) {
        img.alpha *= self.opacity;
        match self.layer {
            UiLayer::Behind => scene.behind_images.push(img),
            UiLayer::Overlay => scene.overlay_images.push(img),
        }
    }
}

fn measure_node(node: &UiNode, max_w: f32, scale: f32) -> (f32, f32) {
    match node {
        UiNode::Text { text, size, .. } => {
            let px = size.unwrap_or(13.0) * scale;
            let char_w = px * 0.6;
            let w = (text.chars().count() as f32 * char_w).min(max_w);
            let h = px * 1.35;
            (w, h)
        }
        UiNode::Icon { size, .. } => {
            let px = size.unwrap_or(16.0) * scale;
            (px * 1.2, px * 1.2)
        }
        UiNode::Button {
            label,
            width,
            height,
            ..
        } => {
            let w = width
                .map(|v| v * scale)
                .unwrap_or_else(|| (label.chars().count() as f32 * 8.5 + 20.0) * scale);
            let h = height.map(|v| v * scale).unwrap_or(28.0 * scale);
            (w.min(max_w), h)
        }
        UiNode::Image { width, height, .. } => (width * scale, height * scale),
        UiNode::Container {
            padding,
            width,
            height,
            child,
            ..
        } => {
            let pad = padding * scale;
            let (cw, ch) = measure_node(child, (max_w - 2.0 * pad).max(0.0), scale);
            (
                width.map(|v| v * scale).unwrap_or(cw + 2.0 * pad),
                height.map(|v| v * scale).unwrap_or(ch + 2.0 * pad),
            )
        }
        UiNode::Row { gap, children, .. } => {
            let g = gap * scale;
            let mut total_w = 0.0_f32;
            let mut max_h = 0.0_f32;
            for (i, c) in children.iter().enumerate() {
                let (cw, ch) = measure_node(c, max_w, scale);
                total_w += cw + if i > 0 { g } else { 0.0 };
                max_h = max_h.max(ch);
            }
            (total_w.min(max_w), max_h)
        }
        UiNode::Column { gap, children, .. } => {
            let g = gap * scale;
            let mut max_cw = 0.0_f32;
            let mut total_h = 0.0_f32;
            for (i, c) in children.iter().enumerate() {
                let (cw, ch) = measure_node(c, max_w, scale);
                max_cw = max_cw.max(cw);
                total_h += ch + if i > 0 { g } else { 0.0 };
            }
            (max_cw.min(max_w), total_h)
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
                        let (_, ch) = measure_node(child, cell_w, scale);
                        row_h = row_h.max(ch);
                    }
                }
                total_h += row_h + if r > 0 { g } else { 0.0 };
            }
            (max_w, total_h)
        }
        UiNode::Stack { children } => {
            let mut w = 0.0_f32;
            let mut h = 0.0_f32;
            for c in children {
                let (cw, ch) = measure_node(c, max_w, scale);
                w = w.max(cw);
                h = h.max(ch);
            }
            (w.min(max_w), h)
        }
        UiNode::Progress { width, height, .. } | UiNode::Slider { width, height, .. } => (
            width.map(|v| v * scale).unwrap_or(max_w),
            height.map(|v| v * scale).unwrap_or(10.0 * scale),
        ),
        UiNode::Canvas { width, height, .. } => (width * scale, height * scale),
        UiNode::ScrollView { height, .. } => (max_w, height * scale),
        UiNode::Toggle { label, .. } | UiNode::Checkbox { label, .. } => (
            ((label.chars().count() as f32 * 7.8 + 44.0) * scale).min(max_w),
            24.0 * scale,
        ),
        UiNode::Dropdown {
            options, selected, ..
        } => {
            let cur = options.get(*selected).map_or("Select", String::as_str);
            (
                ((cur.chars().count() as f32 * 8.0 + 34.0) * scale).min(max_w),
                26.0 * scale,
            )
        }
        UiNode::Divider { vertical, .. } => {
            if *vertical {
                (1.0 * scale, 18.0 * scale)
            } else {
                (max_w, 1.0 * scale)
            }
        }
        UiNode::Badge { text, .. } => (
            ((text.chars().count() as f32 * 7.0 + 14.0) * scale).min(max_w),
            20.0 * scale,
        ),
        UiNode::Tabs { .. } => (max_w, 26.0 * scale),
        UiNode::Sparkline { width, height, .. } => ((width * scale).min(max_w), height * scale),
    }
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
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let fg_default = to_f4(ctx.theme.foreground, 1.0);
    let accent_default = to_f4(ctx.theme.cursor, 1.0);
    let card_default = UiColor::Token("card".into()).resolve(ctx.theme, [0.12, 0.14, 0.18, 0.92]);

    match node {
        UiNode::Text {
            text,
            size,
            color,
            align,
        } => {
            let px = size.unwrap_or(13.0) * ctx.scale;
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
            let px = size.unwrap_or(16.0) * ctx.scale;
            let col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            let glyph_str = icon_glyph(name);
            emit_text_line(
                ctx,
                glyph_str,
                px,
                (x, y, w, h),
                clip,
                Align::Center,
                col,
                renderer,
                scene,
            );
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
            let hover_default =
                UiColor::Token("surface-hover".into()).resolve(ctx.theme, [0.22, 0.25, 0.30, 0.96]);
            let mut bg_col = bg
                .as_ref()
                .map_or(card_default, |c| c.resolve(ctx.theme, card_default));
            if hovered {
                bg_col = [
                    (bg_col[0] * 0.7 + hover_default[0] * 0.3).clamp(0.0, 1.0),
                    (bg_col[1] * 0.7 + hover_default[1] * 0.3).clamp(0.0, 1.0),
                    (bg_col[2] * 0.7 + hover_default[2] * 0.3).clamp(0.0, 1.0),
                    bg_col[3].max(0.95),
                ];
            }
            let fg_col = color
                .as_ref()
                .map_or(fg_default, |c| c.resolve(ctx.theme, fg_default));
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: *radius * ctx.scale,
                    color: bg_col,
                    border_width: 1.0 * ctx.scale,
                    border_color: if hovered {
                        [accent_default[0], accent_default[1], accent_default[2], 0.7]
                    } else {
                        [fg_col[0], fg_col[1], fg_col[2], 0.22]
                    },
                },
            );
            emit_text_line(
                ctx,
                label,
                12.5 * ctx.scale,
                (x, y, w, h),
                clip,
                Align::Center,
                fg_col,
                renderer,
                scene,
            );
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
                    .map_or([0.0; 4], |c| c.resolve(ctx.theme, card_default));
                let border_col = border
                    .as_ref()
                    .map_or([0.0; 4], |c| c.resolve(ctx.theme, fg_default));
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
            let g = gap * ctx.scale;
            let mut cx = x;
            for child in children {
                let (cw, ch) = measure_node(child, (x + w - cx).max(0.0), ctx.scale);
                let cy = match align {
                    Align::Center => y + (h - ch).max(0.0) * 0.5,
                    Align::End => y + (h - ch).max(0.0),
                    _ => y,
                };
                let child_h = if *align == Align::Stretch {
                    h
                } else {
                    ch.min(h)
                };
                let child_w = cw.min((x + w - cx).max(0.0));
                emit_node(
                    ctx,
                    child,
                    (cx, cy, child_w, child_h),
                    clip,
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
            let g = gap * ctx.scale;
            let mut cy = y;
            for child in children {
                let (cw, ch) = measure_node(child, w, ctx.scale);
                let cx = match align {
                    Align::Center => x + (w - cw).max(0.0) * 0.5,
                    Align::End => x + (w - cw).max(0.0),
                    _ => x,
                };
                let child_w = if *align == Align::Stretch {
                    w
                } else {
                    cw.min(w)
                };
                let child_h = ch.min((y + h - cy).max(0.0));
                emit_node(
                    ctx,
                    child,
                    (cx, cy, child_w, child_h),
                    clip,
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
                for c in row {
                    let (_, ch) = measure_node(c, cell_w, ctx.scale);
                    row_h = row_h.max(ch);
                }
                for (col_idx, child) in row.iter().enumerate() {
                    let cx = x + col_idx as f32 * (cell_w + g);
                    emit_node(
                        ctx,
                        child,
                        (cx, cy, cell_w, row_h.min((y + h - cy).max(0.0))),
                        clip,
                        renderer,
                        scene,
                        hits,
                    );
                }
                cy += row_h + g;
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
            let bar_h = h.min(10.0 * ctx.scale).max(4.0);
            let bar_y = y + (h - bar_h) * 0.5;
            let track_col = track.as_ref().map_or([1.0, 1.0, 1.0, 0.14], |c| {
                c.resolve(ctx.theme, [1.0, 1.0, 1.0, 0.14])
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
                },
            );
            if v > 0.0 {
                ctx.push_rect(
                    scene,
                    SceneRoundedRect {
                        x,
                        y: bar_y,
                        w: (w * v).max(bar_h),
                        h: bar_h,
                        radius: bar_h * 0.5,
                        color: fill_col,
                        border_width: 0.0,
                        border_color: [0.0; 4],
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
            let v = value.clamp(0.0, 1.0);
            let bar_h = (h * 0.4).clamp(4.0, 8.0 * ctx.scale);
            let bar_y = y + (h - bar_h) * 0.5;
            let track_col = track.as_ref().map_or([1.0, 1.0, 1.0, 0.16], |c| {
                c.resolve(ctx.theme, [1.0, 1.0, 1.0, 0.16])
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
                },
            );
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y: bar_y,
                    w: (w * v).max(bar_h),
                    h: bar_h,
                    radius: bar_h * 0.5,
                    color: fill_col,
                    border_width: 0.0,
                    border_color: [0.0; 4],
                },
            );
            let knob = (bar_h * 1.9).max(10.0 * ctx.scale);
            let kx = (x + w * v - knob * 0.5).clamp(x, x + w - knob);
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
                    border_width: 0.0,
                    border_color: [0.0; 4],
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
                let (_, ch) = measure_node(child, w, ctx.scale);
                if cy + ch >= y && cy <= y + h {
                    emit_node(ctx, child, (x, cy, w, ch), view_clip, renderer, scene, hits);
                }
                cy += ch + 4.0 * ctx.scale;
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
                    radius: 4.0 * ctx.scale,
                    color: if *checked {
                        accent_default
                    } else {
                        [fg_default[0], fg_default[1], fg_default[2], 0.12]
                    },
                    border_width: 1.0 * ctx.scale,
                    border_color: if *checked {
                        accent_default
                    } else {
                        [fg_default[0], fg_default[1], fg_default[2], 0.35]
                    },
                },
            );
            if *checked {
                emit_text_line(
                    ctx,
                    "✓",
                    11.0 * ctx.scale,
                    (x, box_y, box_sz, box_sz),
                    clip,
                    Align::Center,
                    to_f4(ctx.theme.background, 1.0),
                    renderer,
                    scene,
                );
            }
            let text_x = x + box_sz + 8.0 * ctx.scale;
            let text_col = if *disabled {
                UiColor::Token("muted".into()).resolve(ctx.theme, fg_default)
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
            let label = format!("{} ▾", options.get(*selected).map_or("—", String::as_str));
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: 6.0 * ctx.scale,
                    color: card_default,
                    border_width: 1.0 * ctx.scale,
                    border_color: if hovered && !*disabled {
                        [
                            accent_default[0],
                            accent_default[1],
                            accent_default[2],
                            0.75,
                        ]
                    } else {
                        [fg_default[0], fg_default[1], fg_default[2], 0.25]
                    },
                },
            );
            let col = if *disabled {
                UiColor::Token("muted".into()).resolve(ctx.theme, fg_default)
            } else {
                fg_default
            };
            emit_text_line(
                ctx,
                &label,
                12.0 * ctx.scale,
                (x + 8.0 * ctx.scale, y, (w - 16.0 * ctx.scale).max(0.0), h),
                clip,
                Align::Start,
                col,
                renderer,
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
            let pill_w = (32.0 * ctx.scale).min(w * 0.45);
            let pill_h = (16.0 * ctx.scale).min(h);
            let pill_y = y + (h - pill_h) * 0.5;
            let track_col = if *checked {
                accent_default
            } else {
                [fg_default[0], fg_default[1], fg_default[2], 0.22]
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
                    border_width: 0.0,
                    border_color: [0.0; 4],
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
                    color: fg_default,
                    border_width: 0.0,
                    border_color: [0.0; 4],
                },
            );
            let text_x = x + pill_w + 8.0 * ctx.scale;
            let text_col = if *disabled {
                UiColor::Token("muted".into()).resolve(ctx.theme, fg_default)
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
            let col = color.as_ref().map_or(
                UiColor::Token("border-subtle".into()).resolve(ctx.theme, [1.0, 1.0, 1.0, 0.16]),
                |c| c.resolve(ctx.theme, [1.0, 1.0, 1.0, 0.16]),
            );
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
                },
            );
        }
        UiNode::Badge { text, color, bg } => {
            let fg_col = color
                .as_ref()
                .map_or(accent_default, |c| c.resolve(ctx.theme, accent_default));
            let bg_col = bg
                .as_ref()
                .map_or([fg_col[0], fg_col[1], fg_col[2], 0.18], |c| {
                    c.resolve(ctx.theme, [fg_col[0], fg_col[1], fg_col[2], 0.18])
                });
            ctx.push_rect(
                scene,
                SceneRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius: h * 0.45,
                    color: bg_col,
                    border_width: 1.0 * ctx.scale,
                    border_color: [fg_col[0], fg_col[1], fg_col[2], 0.42],
                },
            );
            emit_text_line(
                ctx,
                text,
                11.0 * ctx.scale,
                (x + 4.0 * ctx.scale, y, (w - 8.0 * ctx.scale).max(0.0), h),
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
            let gap = 4.0 * ctx.scale;
            let tab_w = ((w - gap * (tabs.len().saturating_sub(1)) as f32) / tabs.len() as f32)
                .max(24.0 * ctx.scale);
            for (idx, label) in tabs.iter().enumerate() {
                let tx = x + idx as f32 * (tab_w + gap);
                if tx >= x + w {
                    break;
                }
                let tw = tab_w.min(x + w - tx);
                let is_active = idx == *active;
                let hovered = ctx.is_hovered((tx, y, tw, h));
                let bg_col = if is_active {
                    [
                        accent_default[0],
                        accent_default[1],
                        accent_default[2],
                        0.25,
                    ]
                } else if hovered {
                    UiColor::Token("surface-hover".into())
                        .resolve(ctx.theme, [0.22, 0.25, 0.30, 0.8])
                } else {
                    [0.0, 0.0, 0.0, 0.0]
                };
                ctx.push_rect(
                    scene,
                    SceneRoundedRect {
                        x: tx,
                        y,
                        w: tw,
                        h,
                        radius: 6.0 * ctx.scale,
                        color: bg_col,
                        border_width: if is_active { 1.0 * ctx.scale } else { 0.0 },
                        border_color: accent_default,
                    },
                );
                emit_text_line(
                    ctx,
                    label,
                    11.5 * ctx.scale,
                    (tx + 4.0 * ctx.scale, y, (tw - 8.0 * ctx.scale).max(0.0), h),
                    clip,
                    Align::Center,
                    if is_active {
                        fg_default
                    } else {
                        UiColor::Token("muted".into()).resolve(ctx.theme, fg_default)
                    },
                    renderer,
                    scene,
                );
                hits.push(UiHitTarget {
                    widget_id: ctx.widget_id.to_string(),
                    node_id: id.clone(),
                    kind: HitKind::Tab(idx),
                    x: tx,
                    y,
                    w: tw,
                    h,
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
                    radius: 4.0 * ctx.scale,
                    color: [fg_default[0], fg_default[1], fg_default[2], 0.06],
                    border_width: 0.0,
                    border_color: [0.0; 4],
                },
            );
            if values.is_empty() {
                return;
            }
            let max_v = values.iter().copied().fold(0.001_f32, f32::max);
            let n = values.len();
            let bar_w = (w / n as f32).max(1.5);
            for (i, &v) in values.iter().enumerate() {
                let norm = (v / max_v).clamp(0.0, 1.0);
                let bh = (h * norm).max(1.5);
                let bx = x + i as f32 * bar_w;
                let bw = (bar_w - 1.0).max(1.0).min((x + w - bx).max(0.0));
                if bw > 0.0 {
                    ctx.push_rect(
                        scene,
                        SceneRoundedRect {
                            x: bx,
                            y: y + h - bh,
                            w: bw,
                            h: bh,
                            radius: 1.0,
                            color: [line_col[0], line_col[1], line_col[2], 0.78],
                            border_width: 0.0,
                            border_color: [0.0; 4],
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
    if clip.2 <= 0.0 || clip.3 <= 0.0 {
        return;
    }
    let max_text_w = clip.2.min(rw).max(0.0);
    let char_adv = px_size * 0.58;
    let total_chars = text.chars().count();
    let max_chars = ((max_text_w / char_adv.max(1.0)).floor() as usize).max(1);
    let display_text: std::borrow::Cow<'_, str> = if total_chars > max_chars && max_chars >= 2 {
        let mut s: String = text.chars().take(max_chars - 1).collect();
        s.push('…');
        std::borrow::Cow::Owned(s)
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let approx_w = (display_text.chars().count() as f32 * char_adv).min(rw);
    let start_x = match align {
        Align::Center => rx + (rw - approx_w).max(0.0) * 0.5,
        Align::End => rx + (rw - approx_w).max(0.0),
        _ => rx,
    };
    let baseline = ry + (rh + px_size * 0.72) * 0.5;
    let mut pen_x = start_x;
    for ch in display_text.chars() {
        if pen_x > clip.0 + clip.2 {
            break;
        }
        if ch != ' ' {
            let (metrics, _) = renderer.glyph_bitmap(ch, px_size);
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
            pen_x += metrics.advance_width.max(px_size * 0.5);
        } else {
            pen_x += px_size * 0.45;
        }
    }
}

fn icon_glyph(name: &str) -> &'static str {
    match name {
        "play" => "▶",
        "pause" => "⏸",
        "next" => "⏭",
        "prev" | "previous" => "⏮",
        "volume" | "audio" => "♫",
        "keyboard" => "⌨",
        "xray" | "activity" => "◈",
        "lock" => "🔒",
        "warning" => "▲",
        "check" => "✓",
        "close" => "✕",
        _ => "●",
    }
}

/// Emit visual editor handles (selection border, corner resize handle) into `Scene`.
pub fn emit_editor_chrome(
    scene: &mut Scene,
    rect: (f32, f32, f32, f32),
    selected: bool,
    scale: f32,
) {
    let (x, y, w, h) = rect;
    let border = if selected {
        [0.35, 0.85, 0.96, 0.95]
    } else {
        [1.0, 1.0, 1.0, 0.35]
    };
    scene.overlay_rounded.push(SceneRoundedRect {
        x,
        y,
        w,
        h,
        radius: 4.0 * scale,
        color: [0.0, 0.0, 0.0, 0.0],
        border_width: 2.0 * scale,
        border_color: border,
    });
    if selected {
        let handle = 10.0 * scale;
        scene.overlays.push(SceneRect {
            x: (x + w - handle).max(x),
            y: (y + h - handle).max(y),
            w: handle,
            h: handle,
            color: [0.35, 0.85, 0.96, 0.95],
        });
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
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node_id, "play");
    }
}
