// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::terminal::{Screen, resolve_color};
use fontdue::{Font, FontSettings, Metrics};
use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::process::Command;
use unicode_width::UnicodeWidthChar;

fn cell_advance(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

/// A solid rectangle in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SceneRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

/// A one-to-one copy from a glyph bitmap into the physical framebuffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneGlyph {
    pub ch: char,
    pub px_size: f32,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub source: (usize, usize),
    pub color: [f32; 4],
}

/// A rounded rectangle with optional border in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SceneRoundedRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    pub color: [f32; 4],
    pub border_width: f32,
    pub border_color: [f32; 4],
}

/// An RGBA bitmap image quad in physical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneImage {
    pub id: u64,
    pub src_w: u32,
    pub src_h: u32,
    pub rgba: std::sync::Arc<Vec<u8>>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub alpha: f32,
    pub radius: f32,
}

/// Drawing instructions shared by the OpenGL and software backends.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub behind_rects: Vec<SceneRoundedRect>,
    pub behind_images: Vec<SceneImage>,
    pub behind_glyphs: Vec<SceneGlyph>,
    pub rects: Vec<SceneRect>,
    pub glyphs: Vec<SceneGlyph>,
    pub overlays: Vec<SceneRect>,
    pub overlay_rounded: Vec<SceneRoundedRect>,
    pub overlay_images: Vec<SceneImage>,
    pub overlay_glyphs: Vec<SceneGlyph>,
    pub background: [f32; 4],
}

fn rgba(color: [u8; 3]) -> [f32; 4] {
    [
        color[0] as f32 / 255.0,
        color[1] as f32 / 255.0,
        color[2] as f32 / 255.0,
        1.0,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub cell_width: usize,
    pub cell_height: usize,
    pub padding: usize,
    pub baseline: usize,
}
impl Geometry {
    /// The primary face defines the grid. Fallback faces use those same cells.
    fn from_font(font: &Font, px_size: f32) -> Self {
        let (mut ascent, mut descent, gap) =
            font.horizontal_line_metrics(px_size)
                .map_or((0, 0, 0), |line| {
                    (
                        line.ascent.ceil() as usize,
                        (-line.descent).ceil() as usize,
                        line.line_gap.round() as usize,
                    )
                });
        let mut advance = 0.0_f32;
        // Include the actual bitmap bounds when the line tables understate
        // the extent of printable ASCII at this physical font size.
        for ch in ' '..='~' {
            let glyph = font.metrics(ch, px_size);
            advance = advance.max(glyph.advance_width);
            if glyph.width != 0 && glyph.height != 0 {
                ascent = ascent.max((glyph.ymin + glyph.height as i32).max(0) as usize);
                descent = descent.max((-glyph.ymin).max(0) as usize);
            }
        }
        Self {
            cell_width: advance.ceil().max(1.0) as usize,
            cell_height: (ascent + descent + gap).max(1),
            baseline: ascent + gap / 2,
            padding: 0,
        }
    }

    pub fn for_window(mut self, width: usize, height: usize) -> Self {
        self.padding = self
            .padding
            .min(width.saturating_sub(self.cell_width) / 2)
            .min(height.saturating_sub(self.cell_height) / 2);
        self
    }
    pub fn cell_at(self, x: f64, y: f64) -> (usize, usize) {
        let pad = self.padding as f64;
        (
            ((x - pad) / self.cell_width as f64).floor() as usize,
            ((y - pad) / self.cell_height as f64).floor() as usize,
        )
    }

    pub fn grid_size(&self, width: usize, height: usize) -> (usize, usize) {
        let padding = self.for_window(width, height).padding;
        (
            width
                .saturating_sub(2 * padding)
                .checked_div(self.cell_width)
                .unwrap_or(0)
                .clamp(1, u16::MAX as usize),
            height
                .saturating_sub(2 * padding)
                .checked_div(self.cell_height)
                .unwrap_or(0)
                .clamp(1, u16::MAX as usize),
        )
    }
}
const GLYPH_CACHE_BYTES: usize = 16 * 1024 * 1024;
const GLYPH_CACHE_ENTRIES: usize = 4096;

pub struct Renderer {
    fonts: Vec<Font>,
    font_geometry: Cell<Option<(u32, Geometry)>>,
    cache: HashMap<(char, u32), (Metrics, Vec<u8>)>,
    cache_bytes: usize,
    theme: crate::theme::Theme,
}
impl Renderer {
    #[cfg(test)]
    pub fn new() -> io::Result<Self> {
        Self::with_theme(crate::theme::Theme::default())
    }
    pub fn with_theme(theme: crate::theme::Theme) -> io::Result<Self> {
        let mut paths = Vec::new();
        if let Ok(output) = Command::new("fc-match")
            .args([
                "-s",
                "-f",
                "%{file}\\t%{index}\\n",
                &format!("{}:spacing=mono:style=Regular", theme.font_family),
            ])
            .output()
            && output.status.success()
        {
            paths.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        let (path, index) = line.rsplit_once('\t')?;
                        Some((path.to_owned(), index.parse::<u32>().ok()? & 0xffff))
                    }),
            );
        }
        paths.extend(
            [
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
                "/usr/share/fonts/liberation-mono-fonts/LiberationMono-Regular.ttf",
            ]
            .iter()
            .map(|s| (s.to_string(), 0)),
        );
        let mut fonts = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (path, collection_index) in paths {
            if !seen.insert((path.clone(), collection_index)) {
                continue;
            }
            if let Ok(data) = std::fs::read(path)
                && let Ok(font) = Font::from_bytes(
                    data,
                    FontSettings {
                        collection_index,
                        ..Default::default()
                    },
                )
            {
                fonts.push(font);
                if fonts.len() == 12 {
                    break;
                }
            }
        }
        if fonts.is_empty() {
            return Err(io::Error::other(
                "no usable system font found (install a monospace font or fontconfig)",
            ));
        }
        Ok(Self {
            font_geometry: Cell::default(),
            fonts,
            cache: HashMap::new(),
            cache_bytes: 0,
            theme,
        })
    }
    pub fn retheme(&mut self, theme: crate::theme::Theme) -> io::Result<()> {
        if self.theme.font_family != theme.font_family || self.theme.font_size != theme.font_size {
            *self = Self::with_theme(theme)?;
        } else {
            self.theme = theme;
        }
        Ok(())
    }
    pub fn cursor_blinks(&self, screen: &Screen) -> bool {
        self.theme.cursor_blink && screen.cursor_style.is_none_or(|style| style.blink)
    }
    pub fn cursor_x(screen: &Screen) -> usize {
        screen.cursor_x.min(screen.columns - 1)
    }
    pub fn geometry(&self, scale: f32) -> Geometry {
        let px_size = self.font_px(scale);
        let key = px_size.to_bits();
        let mut geometry = match self.font_geometry.get() {
            Some((cached, geometry)) if cached == key => geometry,
            _ => {
                let geometry = Geometry::from_font(&self.fonts[0], px_size);
                self.font_geometry.set(Some((key, geometry)));
                geometry
            }
        };
        geometry.padding = (self.theme.padding as f32 * scale).round() as usize;
        geometry
    }

    fn font_px(&self, scale: f32) -> f32 {
        self.theme.font_size * scale
    }

    fn font_for(&self, ch: char) -> Option<&Font> {
        self.fonts
            .iter()
            .find(|font| font.lookup_glyph_index(ch) != 0)
    }

    fn raster_source(&self, ch: char) -> (&Font, char) {
        if let Some(font) = self.font_for(ch) {
            (font, ch)
        } else {
            // Some faces give the missing-glyph slot an empty outline.
            // Use an explicit replacement while keeping the original cell span.
            let replacement = '\u{fffd}';
            self.font_for(replacement)
                .map_or((&self.fonts[0], '?'), |font| (font, replacement))
        }
    }

    pub fn glyph_bitmap(&mut self, ch: char, px_size: f32) -> &(Metrics, Vec<u8>) {
        let key = (ch, px_size.to_bits());
        if !self.cache.contains_key(&key) {
            let (font, raster_char) = self.raster_source(ch);
            let glyph = font.rasterize(raster_char, px_size);
            if self.cache.len() >= GLYPH_CACHE_ENTRIES
                || self.cache_bytes + glyph.1.len() > GLYPH_CACHE_BYTES
            {
                self.cache.clear();
                self.cache_bytes = 0;
            }
            self.cache_bytes += glyph.1.len();
            self.cache.insert(key, glyph);
        }
        &self.cache[&key]
    }
    #[cfg(test)]
    pub fn render(&mut self, screen: &Screen, width: usize, height: usize) -> Vec<u8> {
        self.render_selection(
            screen,
            width,
            height,
            &crate::selection::Selection::default(),
        )
    }
    #[cfg(test)]
    pub fn render_selection(
        &mut self,
        screen: &Screen,
        width: usize,
        height: usize,
        selection: &crate::selection::Selection,
    ) -> Vec<u8> {
        self.render_with_cursor(screen, width, height, selection, true, true)
    }
    #[cfg(test)]
    pub fn render_with_cursor(
        &mut self,
        screen: &Screen,
        width: usize,
        height: usize,
        selection: &crate::selection::Selection,
        focused: bool,
        blink_on: bool,
    ) -> Vec<u8> {
        let mut pixels = Vec::new();
        let scene = self.build_terminal_scene(
            screen,
            (width, height),
            selection,
            focused,
            blink_on,
            1.0,
            None,
        );
        pixels.resize(width * height * 4, 0);
        self.paint_scene(&scene, width, &mut pixels);
        pixels
    }

    /// Build a scene in physical pixels. Partial redraws emit only damaged rows.
    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn build_terminal_scene(
        &mut self,
        screen: &Screen,
        dimensions: (usize, usize),
        selection: &crate::selection::Selection,
        focused: bool,
        blink_on: bool,
        scale: f32,
        rows: Option<(usize, usize)>,
    ) -> Scene {
        self.build_terminal_scene_with_insets(
            screen,
            dimensions,
            selection,
            focused,
            blink_on,
            scale,
            rows,
            (0.0, 0.0, 0.0, 0.0),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_terminal_scene_with_insets(
        &mut self,
        screen: &Screen,
        (width, height): (usize, usize),
        selection: &crate::selection::Selection,
        focused: bool,
        blink_on: bool,
        scale: f32,
        rows: Option<(usize, usize)>,
        insets: (f32, f32, f32, f32),
    ) -> Scene {
        let base = self.geometry(scale).for_window(width, height);
        let (width, height) = (width as f32, height as f32);
        let cell_w = base.cell_width as f32;
        let cell_h = base.cell_height as f32;
        let padding = base.padding as f32;
        let baseline = base.baseline as f32;
        let px_size = self.font_px(scale);
        let inner_left = padding + insets.0;
        let inner_right = (width - padding - insets.2).max(inner_left);
        let inner_top = padding + insets.1;
        let inner_bottom = (height - padding - insets.3).max(inner_top);

        let background = rgba(self.theme.background);
        let cursor_color = rgba(self.theme.cursor);
        let selection_color = rgba(self.theme.selection);
        let selection_foreground = rgba(self.theme.selection_foreground);
        let cursor_shape = screen
            .cursor_style
            .map_or(self.theme.cursor_shape, |style| style.shape);
        let cursor_shown = screen.cursor_visible
            && screen.view_offset() == 0
            && (!self.cursor_blinks(screen) || !focused || blink_on);
        let cursor_col = Self::cursor_x(screen);
        let cursor_row = screen.cursor_display_row();
        let block = cursor_shown && focused && cursor_shape == crate::theme::CursorShape::Block;
        let block_lead = if screen
            .get(cursor_col, cursor_row)
            .is_some_and(|cell| cell.continuation)
        {
            cursor_col.saturating_sub(1)
        } else {
            cursor_col
        };
        let block_span = if screen
            .get(block_lead, cursor_row)
            .is_some_and(|cell| cell.wide)
        {
            2
        } else {
            1
        };
        let block_covers =
            |x, y| block && y == cursor_row && (block_lead..block_lead + block_span).contains(&x);

        let (row_start, row_end) = rows.unwrap_or((0, screen.rows));
        let row_start = row_start.min(screen.rows);
        let row_end = row_end.min(screen.rows).max(row_start);

        let mut scene = Scene {
            background,
            ..Default::default()
        };
        for y in row_start..row_end {
            let top = inner_top + y as f32 * cell_h;
            let bottom = (top + cell_h).min(inner_bottom);
            if bottom <= top {
                continue;
            }
            for x in 0..screen.columns {
                let Some(cell) = screen.get(x, y) else {
                    continue;
                };
                let left = inner_left + x as f32 * cell_w;
                let right = (left + cell_w).min(inner_right);
                if right <= left {
                    continue;
                }
                let bg = if block_covers(x, y) {
                    cursor_color
                } else if selection.contains(screen, y, x) {
                    selection_color
                } else {
                    let [r, g, b] = resolve_color(cell.bg, cell.bg_origin, &self.theme);
                    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
                };
                if bg != background {
                    scene.rects.push(SceneRect {
                        x: left,
                        y: top,
                        w: right - left,
                        h: bottom - top,
                        color: bg,
                    });
                }
            }
        }

        for y in row_start..row_end {
            let top = inner_top + y as f32 * cell_h;
            let bottom = (top + cell_h).min(inner_bottom);
            if bottom <= top {
                continue;
            }
            for x in 0..screen.columns {
                let Some(cell) = screen.get(x, y) else {
                    continue;
                };
                if cell.continuation {
                    continue;
                }
                let foreground = if block_covers(x, y) {
                    background
                } else if selection.contains(screen, y, x) {
                    selection_foreground
                } else {
                    rgba(resolve_color(cell.fg, cell.fg_origin, &self.theme))
                };
                let cell_left = inner_left + x as f32 * cell_w;
                let cell_right = cell_left + cell_w * if cell.wide { 2.0 } else { 1.0 };
                for (index, ch) in std::iter::once(cell.character)
                    .chain(cell.combining.iter().copied())
                    .enumerate()
                {
                    if ch == ' ' {
                        continue;
                    }
                    self.push_scene_glyph(
                        &mut scene,
                        ch,
                        index != 0,
                        px_size,
                        cell_left,
                        cell_right,
                        top,
                        bottom,
                        baseline,
                        inner_left,
                        inner_right,
                        foreground,
                    );
                }
            }
        }

        if cursor_shown && !block && cursor_row >= row_start && cursor_row < row_end {
            let cursor_x = inner_left + cursor_col as f32 * cell_w;
            let cursor_y = inner_top + cursor_row as f32 * cell_h;
            let cursor_right = (cursor_x + cell_w).min(inner_right);
            let cursor_bottom = (cursor_y + cell_h).min(inner_bottom);
            if cursor_right > cursor_x && cursor_bottom > cursor_y {
                let mut push = |x, y, w, h| {
                    scene.overlays.push(SceneRect {
                        x,
                        y,
                        w,
                        h,
                        color: cursor_color,
                    });
                };
                if !focused {
                    let t = scale.round().max(1.0);
                    push(
                        cursor_x,
                        cursor_y,
                        cursor_right - cursor_x,
                        t.min(cursor_bottom - cursor_y),
                    );
                    push(
                        cursor_x,
                        (cursor_bottom - t).max(cursor_y),
                        cursor_right - cursor_x,
                        t.min(cursor_bottom - cursor_y),
                    );
                    push(
                        cursor_x,
                        cursor_y,
                        t.min(cursor_right - cursor_x),
                        cursor_bottom - cursor_y,
                    );
                    push(
                        (cursor_right - t).max(cursor_x),
                        cursor_y,
                        t.min(cursor_right - cursor_x),
                        cursor_bottom - cursor_y,
                    );
                } else {
                    match cursor_shape {
                        crate::theme::CursorShape::Block => {}
                        crate::theme::CursorShape::Beam => {
                            push(
                                cursor_x,
                                cursor_y,
                                (2.0 * scale).round().min(cursor_right - cursor_x),
                                cursor_bottom - cursor_y,
                            );
                        }
                        crate::theme::CursorShape::Underline => {
                            let thickness = (2.0 * scale).round().min(cursor_bottom - cursor_y);
                            push(
                                cursor_x,
                                (cursor_bottom - thickness - scale.round()).max(cursor_y),
                                cursor_right - cursor_x,
                                thickness,
                            );
                        }
                    }
                }
            }
        }
        scene
    }

    #[allow(clippy::too_many_arguments)]
    fn push_scene_glyph(
        &mut self,
        scene: &mut Scene,
        ch: char,
        combining: bool,
        px_size: f32,
        cell_left: f32,
        cell_right: f32,
        top: f32,
        bottom: f32,
        baseline: f32,
        clip_left: f32,
        clip_right: f32,
        color: [f32; 4],
    ) {
        let metrics = if let Some(glyph) = self.cache.get(&(ch, px_size.to_bits())) {
            glyph.0
        } else {
            if combining && self.font_for(ch).is_none() {
                return;
            }
            let (font, raster_char) = self.raster_source(ch);
            font.metrics(raster_char, px_size)
        };
        if metrics.width == 0 || metrics.height == 0 {
            return;
        }
        let bitmap_width = metrics.width as f32;
        let span = cell_right - cell_left;
        let offset = if combining {
            // Combining characters occupy the preceding cell span. Standalone
            // bearings can be relative to a pen after the base character.
            (span - bitmap_width) / 2.0
        } else {
            ((span - metrics.advance_width) / 2.0).max(0.0) + metrics.xmin as f32
        };
        let left = cell_left + offset.round().clamp(0.0, (span - bitmap_width).max(0.0));
        let glyph_top = top + baseline - metrics.height as f32 - metrics.ymin as f32;
        let x0 = left.max(clip_left);
        let x1 = (left + bitmap_width).min(cell_right).min(clip_right);
        let y0 = glyph_top.max(top);
        let y1 = (glyph_top + metrics.height as f32).min(bottom);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        scene.glyphs.push(SceneGlyph {
            ch,
            px_size,
            x: x0 as usize,
            y: y0 as usize,
            w: (x1 - x0) as usize,
            h: (y1 - y0) as usize,
            source: ((x0 - left) as usize, (y0 - glyph_top) as usize),
            color,
        });
    }

    /// Build a scene for the built-in editor. Editor frames are small and
    /// keyboard driven, so the GPU backend repaints the whole window.
    pub fn build_editor_scene(
        &mut self,
        view: &crate::editor::EditorView,
        (width, height): (usize, usize),
        focused: bool,
        blink_on: bool,
        scale: f32,
    ) -> Scene {
        let base = self.geometry(scale).for_window(width, height);
        let (width, height) = (width as f32, height as f32);
        let cell_w = base.cell_width as f32;
        let cell_h = base.cell_height as f32;
        let padding = base.padding as f32;
        let baseline = base.baseline as f32;
        let px_size = self.font_px(scale);
        let (offset_x, offset_y) = view.text_offset;
        let body_left = padding + offset_x as f32 * cell_w;
        let body_top = padding + offset_y as f32 * cell_h;
        let window_right = width - padding;
        let window_bottom = height - padding;
        let text_x = (body_left + view.gutter_cells as f32 * cell_w).min(window_right);
        let bar_right = (padding + view.columns as f32 * cell_w).min(window_right);
        let text_right = (text_x + view.text_columns as f32 * cell_w).min(width - padding);
        let background = rgba(self.theme.background);
        let cursor_color = rgba(self.theme.cursor);
        let selected_bg = rgba(self.theme.selection);
        let foreground = rgba(self.theme.foreground);
        let dim = rgba(self.theme.ansi[8]);
        let selection_foreground = rgba(self.theme.selection_foreground);
        let mut scene = Scene {
            background,
            ..Default::default()
        };
        for item in &view.chrome {
            let left = padding + item.col as f32 * cell_w;
            let top = padding + item.row as f32 * cell_h;
            let right = (left + item.width as f32 * cell_w).min(window_right);
            let bottom = (top + cell_h).min(window_bottom);
            if right <= left || bottom <= top {
                continue;
            }
            let (background, foreground) = chrome_colors(&self.theme, item.tone);
            scene.rects.push(SceneRect {
                x: left,
                y: top,
                w: right - left,
                h: bottom - top,
                color: rgba(background),
            });
            self.push_text_scene(
                &mut scene,
                &item.text,
                left,
                top,
                cell_w,
                bottom,
                baseline,
                left,
                right,
                rgba(foreground),
                None,
                &[],
                rgba(foreground),
                px_size,
            );
        }
        for (index, row) in view.rows.iter().enumerate() {
            let top = body_top + index as f32 * cell_h;
            if top >= window_bottom {
                break;
            }
            let bottom = (top + cell_h).min(window_bottom);
            if let Some((start, end)) = row.selection {
                let left = (text_x + start as f32 * cell_w).clamp(padding, text_right);
                let right = (text_x + end as f32 * cell_w).clamp(padding, text_right);
                if right > left {
                    scene.rects.push(SceneRect {
                        x: left,
                        y: top,
                        w: right - left,
                        h: bottom - top,
                        color: selected_bg,
                    });
                }
            }
            let gutter_fg = if row.current { foreground } else { dim };
            self.push_text_scene(
                &mut scene,
                &row.gutter,
                body_left,
                top,
                cell_w,
                bottom,
                baseline,
                body_left,
                text_x,
                gutter_fg,
                None,
                &[],
                selection_foreground,
                px_size,
            );
            self.push_text_scene(
                &mut scene,
                &row.text,
                text_x,
                top,
                cell_w,
                bottom,
                baseline,
                text_x,
                text_right,
                foreground,
                row.selection,
                &row.styles,
                selection_foreground,
                px_size,
            );
        }
        let bar_top = padding + view.status_row as f32 * cell_h;
        if bar_top < window_bottom {
            let bar_bg = if view.status_error {
                rgba(self.theme.ansi[1])
            } else {
                selected_bg
            };
            let bar_bottom = (bar_top + cell_h).min(window_bottom);
            scene.rects.push(SceneRect {
                x: padding,
                y: bar_top,
                w: (bar_right - padding).max(0.0),
                h: bar_bottom - bar_top,
                color: bar_bg,
            });
            self.push_text_scene(
                &mut scene,
                &view.status,
                padding + (cell_w / 2.0).floor(),
                bar_top,
                cell_w,
                bar_bottom,
                baseline,
                padding,
                bar_right,
                selection_foreground,
                None,
                &[],
                selection_foreground,
                px_size,
            );
        }
        if let Some((row, column, cells)) = view.cursor
            && focused
            && (!self.theme.cursor_blink || blink_on)
        {
            let left = text_x + column as f32 * cell_w;
            let top = body_top + row as f32 * cell_h;
            let right = (left + cells.max(1) as f32 * cell_w).min(text_right);
            let bottom = (top + cell_h).min(window_bottom);
            match self.theme.cursor_shape {
                crate::theme::CursorShape::Block => {
                    if right > left && bottom > top {
                        scene.overlays.push(SceneRect {
                            x: left,
                            y: top,
                            w: right - left,
                            h: bottom - top,
                            color: cursor_color,
                        });
                    }
                }
                crate::theme::CursorShape::Beam => {
                    if bottom > top && right > left {
                        scene.overlays.push(SceneRect {
                            x: left,
                            y: top,
                            w: (2.0 * scale).round().min(right - left),
                            h: bottom - top,
                            color: cursor_color,
                        });
                    }
                }
                crate::theme::CursorShape::Underline => {
                    if right > left && bottom > top {
                        let thickness = (2.0 * scale).round().min(bottom - top);
                        scene.overlays.push(SceneRect {
                            x: left,
                            y: (bottom - thickness - scale.round()).max(top),
                            w: right - left,
                            h: thickness,
                            color: cursor_color,
                        });
                    }
                }
            }
        }
        scene
    }

    #[allow(clippy::too_many_arguments)]
    fn push_text_scene(
        &mut self,
        scene: &mut Scene,
        text: &str,
        x: f32,
        top: f32,
        cell_w: f32,
        bottom: f32,
        baseline: f32,
        clip_left: f32,
        clip_right: f32,
        foreground: [f32; 4],
        selection: Option<(usize, usize)>,
        styles: &[(std::ops::Range<usize>, crate::syntax::Style)],
        selection_foreground: [f32; 4],
        px_size: f32,
    ) {
        let mut cell = 0isize;
        let mut last_cell = (x, x + cell_w);
        let mut last_index = 0;
        let mut styles = styles.iter().peekable();
        for (byte, ch) in text.char_indices() {
            let advance = cell_advance(ch) as isize;
            let cell_left = x + cell as f32 * cell_w;
            while styles.peek().is_some_and(|(range, _)| range.end <= byte) {
                styles.next();
            }
            let selection_cell = if advance == 0 { last_index } else { cell };
            let color = if selection.is_some_and(|(start, end)| {
                selection_cell >= start as isize && selection_cell < end as isize
            }) {
                selection_foreground
            } else {
                styles
                    .peek()
                    .filter(|(range, _)| range.contains(&byte))
                    .map_or(foreground, |(_, style)| {
                        rgba(syntax_color(&self.theme, *style))
                    })
            };
            if advance == 0 {
                self.push_scene_glyph(
                    scene,
                    ch,
                    true,
                    px_size,
                    last_cell.0,
                    last_cell.1,
                    top,
                    bottom,
                    baseline,
                    clip_left,
                    clip_right,
                    color,
                );
                continue;
            }
            if cell_left >= clip_right {
                break;
            }
            let right = cell_left + advance as f32 * cell_w;
            if ch != ' ' && !ch.is_control() {
                self.push_scene_glyph(
                    scene, ch, false, px_size, cell_left, right, top, bottom, baseline, clip_left,
                    clip_right, color,
                );
            }
            last_cell = (cell_left, right);
            last_index = cell;
            cell += advance;
        }
    }

    pub fn paint_scene(&mut self, scene: &Scene, width: usize, pixels: &mut [u8]) {
        let (pixels, _) = pixels.as_chunks_mut::<4>();
        let height = pixels.len() / width;
        pixels.fill(to_bgra(scene.background));
        if !scene.behind_rects.is_empty() {
            paint_rounded_rects(pixels, width, height, &scene.behind_rects);
        }
        if !scene.behind_images.is_empty() {
            paint_images(pixels, width, height, &scene.behind_images);
        }
        if !scene.behind_glyphs.is_empty() {
            self.paint_glyph_list(pixels, width, height, &scene.behind_glyphs);
        }
        paint_rects(pixels, width, height, &scene.rects);
        self.paint_glyph_list(pixels, width, height, &scene.glyphs);
        paint_rects(pixels, width, height, &scene.overlays);
        if !scene.overlay_rounded.is_empty() {
            paint_rounded_rects(pixels, width, height, &scene.overlay_rounded);
        }
        if !scene.overlay_images.is_empty() {
            paint_images(pixels, width, height, &scene.overlay_images);
        }
        if !scene.overlay_glyphs.is_empty() {
            self.paint_glyph_list(pixels, width, height, &scene.overlay_glyphs);
        }
    }

    fn paint_glyph_list(
        &mut self,
        pixels: &mut [[u8; 4]],
        width: usize,
        height: usize,
        glyphs: &[SceneGlyph],
    ) {
        for glyph in glyphs {
            let (metrics, bitmap) = self.glyph_bitmap(glyph.ch, glyph.px_size);
            let color = to_bgra(glyph.color);
            let glyph_alpha = glyph.color[3].clamp(0.0, 1.0);
            let (sx, sy) = glyph.source;
            let right = (glyph.x + glyph.w).min(width);
            let bottom = (glyph.y + glyph.h).min(height);
            if right <= glyph.x || bottom <= glyph.y {
                continue;
            }
            let count = right - glyph.x;
            for y in glyph.y..bottom {
                let start = (sy + y - glyph.y) * metrics.width + sx;
                let dest = y * width + glyph.x;
                for (pixel, &alpha) in pixels[dest..dest + count]
                    .iter_mut()
                    .zip(&bitmap[start..start + count])
                {
                    let alpha = (f32::from(alpha) * glyph_alpha).round() as u32;
                    for channel in 0..3 {
                        pixel[channel] = ((u32::from(color[channel]) * alpha
                            + u32::from(pixel[channel]) * (255 - alpha))
                            / 255) as u8;
                    }
                }
            }
        }
    }
}

fn paint_rounded_rects(
    pixels: &mut [[u8; 4]],
    width: usize,
    height: usize,
    rects: &[SceneRoundedRect],
) {
    for rect in rects {
        let x0 = (rect.x.floor().max(0.0) as usize).min(width);
        let x1 = ((rect.x + rect.w).ceil().max(0.0) as usize).min(width);
        let y0 = (rect.y.floor().max(0.0) as usize).min(height);
        let y1 = ((rect.y + rect.h).ceil().max(0.0) as usize).min(height);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let r = rect.radius.min(rect.w * 0.5).min(rect.h * 0.5).max(0.0);
        let bw = rect.border_width.max(0.0);
        let fill = to_bgra(rect.color);
        let fill_a = rect.color[3].clamp(0.0, 1.0);
        let border = to_bgra(rect.border_color);
        let border_a = rect.border_color[3].clamp(0.0, 1.0);
        for py in y0..y1 {
            let cy = py as f32 + 0.5 - rect.y;
            for px in x0..x1 {
                let cx = px as f32 + 0.5 - rect.x;
                let dx = (r - cx).max(0.0).max(cx - (rect.w - r));
                let dy = (r - cy).max(0.0).max(cy - (rect.h - r));
                let dist = if r > 0.0 && dx > 0.0 && dy > 0.0 {
                    (dx * dx + dy * dy).sqrt() - r
                } else {
                    -cx.min(rect.w - cx).min(cy).min(rect.h - cy)
                };
                if dist > 0.5 {
                    continue;
                }
                let aa = (0.5 - dist).clamp(0.0, 1.0);
                let is_border = bw > 0.0 && dist > -bw && border_a > 0.0;
                let (src, a) = if is_border {
                    (border, border_a * aa)
                } else if fill_a > 0.0 {
                    (fill, fill_a * aa)
                } else {
                    continue;
                };
                let dst = &mut pixels[py * width + px];
                let inv = 1.0 - a;
                for c in 0..3 {
                    dst[c] = (src[c] as f32 * a + dst[c] as f32 * inv).round() as u8;
                }
            }
        }
    }
}

fn paint_images(pixels: &mut [[u8; 4]], width: usize, height: usize, images: &[SceneImage]) {
    for img in images {
        if img.src_w == 0 || img.src_h == 0 || img.w <= 0.0 || img.h <= 0.0 {
            continue;
        }
        let x0 = (img.x.floor().max(0.0) as usize).min(width);
        let x1 = ((img.x + img.w).ceil().max(0.0) as usize).min(width);
        let y0 = (img.y.floor().max(0.0) as usize).min(height);
        let y1 = ((img.y + img.h).ceil().max(0.0) as usize).min(height);
        for py in y0..y1 {
            let v = ((py as f32 - img.y) / img.h).clamp(0.0, 0.9999);
            let sy = (v * img.src_h as f32) as usize;
            for px in x0..x1 {
                let u = ((px as f32 - img.x) / img.w).clamp(0.0, 0.9999);
                let sx = (u * img.src_w as f32) as usize;
                let off = (sy * img.src_w as usize + sx) * 4;
                if off + 3 >= img.rgba.len() {
                    continue;
                }
                let a = (img.rgba[off + 3] as f32 / 255.0) * img.alpha.clamp(0.0, 1.0);
                let inv = 1.0 - a;
                let dst = &mut pixels[py * width + px];
                dst[0] = (img.rgba[off + 2] as f32 * a + dst[0] as f32 * inv).round() as u8;
                dst[1] = (img.rgba[off + 1] as f32 * a + dst[1] as f32 * inv).round() as u8;
                dst[2] = (img.rgba[off] as f32 * a + dst[2] as f32 * inv).round() as u8;
            }
        }
    }
}

fn paint_rects(pixels: &mut [[u8; 4]], width: usize, height: usize, rects: &[SceneRect]) {
    for rect in rects {
        let color = to_bgra(rect.color);
        let x0 = (rect.x as usize).min(width);
        let x1 = ((rect.x + rect.w) as usize).min(width);
        let y0 = rect.y as usize;
        let y1 = ((rect.y + rect.h) as usize).min(height);
        for y in y0..y1 {
            pixels[y * width + x0..y * width + x1].fill(color);
        }
    }
}

fn to_bgra(color: [f32; 4]) -> [u8; 4] {
    [color[2], color[1], color[0], color[3]].map(|c| (c * 255.0).round() as u8)
}

fn syntax_color(theme: &crate::theme::Theme, style: crate::syntax::Style) -> [u8; 3] {
    use crate::syntax::Style;
    theme.ansi[match style {
        Style::Comment => 8,
        Style::Keyword => 12,
        Style::String => 10,
        Style::Number => 11,
    }]
}

fn chrome_colors(theme: &crate::theme::Theme, tone: crate::bedit::Tone) -> ([u8; 3], [u8; 3]) {
    use crate::bedit::Tone;
    let blend = |weight: u16| {
        std::array::from_fn(|i| {
            ((u16::from(theme.background[i]) * (100 - weight)
                + u16::from(theme.foreground[i]) * weight)
                / 100) as u8
        })
    };
    match tone {
        Tone::Panel => (blend(11), theme.foreground),
        Tone::Active => (theme.selection, theme.selection_foreground),
        Tone::Muted => (blend(6), theme.foreground),
        Tone::Accent => (blend(16), theme.ansi[14]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;
    const BG: [u8; 4] = [32, 24, 18, 255];
    const CURSOR: [u8; 4] = [191, 174, 155, 255];
    const SCALES: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];

    fn frame(g: Geometry, columns: usize, rows: usize) -> (usize, usize) {
        (
            2 * g.padding + columns * g.cell_width,
            2 * g.padding + rows * g.cell_height,
        )
    }

    fn pixel(pixels: &[u8], width: usize, x: usize, y: usize) -> &[u8] {
        &pixels[(y * width + x) * 4..(y * width + x + 1) * 4]
    }

    #[test]
    fn grid_geometry_uses_padding_and_minimum() {
        let renderer = Renderer::new().unwrap();
        let g = renderer.geometry(1.0);
        let (width, height) = frame(g, 2, 2);
        assert_eq!(g.grid_size(width, height), (2, 2));
        assert_eq!(g.grid_size(width - 1, height - 1), (1, 1));
        assert_eq!(g.grid_size(1, 1), (1, 1));
    }

    #[test]
    fn geometry_tracks_primary_face_and_font_size() {
        for size in [11.0, 16.0, 23.5, 32.0] {
            let theme = crate::theme::Theme {
                font_size: size,
                padding: 4,
                ..Default::default()
            };
            let renderer = Renderer::with_theme(theme).unwrap();
            for scale in SCALES {
                let px = size * scale;
                let g = renderer.geometry(scale);
                let font = &renderer.fonts[0];
                let line = font.horizontal_line_metrics(px).unwrap();
                assert_eq!(g.padding, (4.0 * scale).round() as usize);
                assert!(g.baseline as f32 >= line.ascent - 0.001);
                assert!((g.cell_height - g.baseline) as f32 >= -line.descent - 0.001);
                let mut advance = 0.0_f32;
                for ch in ' '..='~' {
                    let glyph = font.metrics(ch, px);
                    advance = advance.max(glyph.advance_width);
                    if glyph.width != 0 && glyph.height != 0 {
                        assert!(
                            glyph.ymin + glyph.height as i32 <= g.baseline as i32,
                            "{ch}"
                        );
                        assert!(
                            glyph.ymin >= g.baseline as i32 - g.cell_height as i32,
                            "{ch}"
                        );
                    }
                }
                assert!(g.cell_width as f32 >= advance - 0.001);
                assert!(g.cell_width as f32 <= advance + 1.001);
            }
        }
    }

    #[test]
    fn bottom_input_layout_renders_content_and_cursor_on_last_row() {
        let theme = crate::theme::Theme {
            layout: crate::terminal::LayoutMode::BottomInput,
            cursor_shape: crate::theme::CursorShape::Block,
            cursor_blink: false,
            ..Default::default()
        };
        let mut renderer = Renderer::with_theme(theme.clone()).unwrap();
        let mut terminal = Terminal::with_theme(2, 2, theme);
        terminal.process(b"A\x1b[?25l");
        let g = renderer.geometry(1.0);
        let (width, height) = frame(g, 2, 2);
        let pixels = renderer.render(terminal.screen(), width, height);
        let last_row = g.padding + g.cell_height;
        assert!(
            pixels[..last_row * width * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == BG)
        );
        assert!(
            pixels[last_row * width * 4..]
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| *p != BG)
        );
        terminal.process(b"\x1b[?25h");
        let pixels = renderer.render(terminal.screen(), width, height);
        assert_eq!(
            pixel(&pixels, width, g.padding + g.cell_width, last_row),
            CURSOR
        );
    }

    #[test]
    fn history_colors_follow_theme_and_keep_explicit_rgb() {
        let mut t = Terminal::new(8, 1);
        t.process(b"\x1b[41mA\x1b[48;2;1;2;3mB\x1b[0mC\r\n");
        t.screen_mut().scroll_view(1);
        let mut theme = crate::theme::Theme::default();
        let mut renderer = Renderer::with_theme(theme.clone()).unwrap();
        theme.background = [8, 9, 10];
        theme.ansi[1] = [4, 5, 6];
        t.apply_theme(theme.clone());
        renderer.retheme(theme).unwrap();
        let g = renderer.geometry(1.0);
        let (width, height) = frame(g, 8, 1);
        let pixels = renderer.render(t.screen(), width, height);
        let cell = |col: usize| pixel(&pixels, width, g.padding + col * g.cell_width, g.padding);
        assert_eq!(cell(0), [6, 5, 4, 255]);
        assert_eq!(cell(1), [3, 2, 1, 255]);
        assert_eq!(cell(2), [10, 9, 8, 255]);
    }

    #[test]
    fn cursor_shapes_and_focus_are_rendered() {
        let mut theme = crate::theme::Theme {
            cursor_blink: true,
            ..Default::default()
        };
        let t = Terminal::new(2, 1);
        let selection = crate::selection::Selection::default();
        for shape in [
            crate::theme::CursorShape::Block,
            crate::theme::CursorShape::Beam,
            crate::theme::CursorShape::Underline,
        ] {
            theme.cursor_shape = shape;
            let mut r = Renderer::with_theme(theme.clone()).unwrap();
            let (width, height) = frame(r.geometry(1.0), 2, 1);
            let shown = r.render_with_cursor(t.screen(), width, height, &selection, true, true);
            let hidden = r.render_with_cursor(t.screen(), width, height, &selection, true, false);
            let unfocused =
                r.render_with_cursor(t.screen(), width, height, &selection, false, false);
            assert_ne!(shown, hidden);
            assert_ne!(unfocused, hidden);
        }
    }

    #[test]
    fn block_cursor_preserves_glyph_and_config_disables_application_blink() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(2, 1);
        terminal.process(b"A\r\x1b[1 q");
        let selection = crate::selection::Selection::default();
        let (width, height) = frame(renderer.geometry(1.0), 2, 1);
        let shown =
            renderer.render_with_cursor(terminal.screen(), width, height, &selection, true, false);
        terminal.process(b"\x1b[?25l");
        assert_ne!(shown, renderer.render(terminal.screen(), width, height));
        terminal.process(b"\x1b[?25h\x1b[2K");
        assert_ne!(shown, renderer.render(terminal.screen(), width, height));
    }

    #[test]
    fn selection_preserves_the_grid_and_window_padding() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(2, 1);
        terminal.process(b"A\x1b[?25l");
        let mut selection = crate::selection::Selection::default();
        selection.press(
            terminal.screen(),
            crate::selection::Position { row: 0, col: 0 },
            1,
            0.0,
            0.0,
        );
        selection.release();
        let g = renderer.geometry(1.0);
        let (width, height) = frame(g, 2, 1);
        let pixels = renderer.render_selection(terminal.screen(), width, height, &selection);
        assert_eq!(
            pixel(&pixels, width, g.padding, g.padding),
            [192, 192, 192, 255]
        );
        assert_eq!(terminal.screen().get(0, 0).unwrap().character, 'A');
        for y in 0..height {
            for x in 0..width {
                if !(g.padding..width - g.padding).contains(&x)
                    || !(g.padding..height - g.padding).contains(&y)
                {
                    assert_eq!(pixel(&pixels, width, x, y), BG);
                }
            }
        }
    }

    #[test]
    fn terminal_glyphs_have_fixed_pixel_phase_and_baseline() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(12, 3);
        terminal.process("IIe\u{301}界Я\r\nI\x1b[?25l".as_bytes());
        let selection = crate::selection::Selection::default();
        for scale in SCALES {
            let g = renderer.geometry(scale);
            let dimensions = frame(g, 12, 3);
            let scene = renderer.build_terminal_scene(
                terminal.screen(),
                dimensions,
                &selection,
                true,
                true,
                scale,
                None,
            );
            let repeated: Vec<_> = scene
                .glyphs
                .iter()
                .filter(|glyph| glyph.ch == 'I')
                .collect();
            assert_eq!(repeated.len(), 3);
            let (first, next_column, next_line) = (repeated[0], repeated[1], repeated[2]);
            assert_eq!(
                (next_column.x - first.x, next_column.y),
                (g.cell_width, first.y)
            );
            assert_eq!(
                (next_line.x, next_line.y - first.y),
                (first.x, g.cell_height)
            );
            let bitmap = renderer.glyph_bitmap('I', first.px_size).0;
            assert_eq!(
                first.y as i32 - first.source.1 as i32 + bitmap.height as i32 + bitmap.ymin,
                (g.padding + g.baseline) as i32
            );
            let wide = scene.glyphs.iter().find(|glyph| glyph.ch == '界').unwrap();
            assert!(wide.x >= g.padding + g.cell_width * 3);
            assert!(wide.x + wide.w <= g.padding + g.cell_width * 5);
            if renderer.font_for('\u{301}').is_some() {
                let mark = scene
                    .glyphs
                    .iter()
                    .find(|glyph| glyph.ch == '\u{301}')
                    .unwrap();
                assert!(mark.x >= g.padding + g.cell_width * 2);
                assert!(mark.x + mark.w <= g.padding + g.cell_width * 3);
            }
            for glyph in &scene.glyphs {
                let metrics = renderer.glyph_bitmap(glyph.ch, glyph.px_size).0;
                assert!(glyph.source.0 + glyph.w <= metrics.width);
                assert!(glyph.source.1 + glyph.h <= metrics.height);
                assert!(glyph.x + glyph.w <= dimensions.0 - g.padding);
                assert!(glyph.y + glyph.h <= dimensions.1 - g.padding);
            }
            let (width, height) = dimensions;
            let mut software = vec![0; width * height * 4];
            renderer.paint_scene(&scene, width, &mut software);
            let masks: Vec<Vec<_>> = repeated
                .iter()
                .map(|glyph| {
                    (glyph.y..glyph.y + glyph.h)
                        .flat_map(|y| {
                            let start = (y * width + glyph.x) * 4;
                            software[start..start + glyph.w * 4].iter().copied()
                        })
                        .collect()
                })
                .collect();
            assert!(masks[0].as_chunks::<4>().0.iter().any(|p| *p != BG));
            assert_eq!(masks[0], masks[1]);
            assert_eq!(masks[0], masks[2]);
        }
    }

    #[test]
    fn fallback_face_supplies_the_bitmap_without_changing_the_grid() {
        let mut renderer = Renderer::new().unwrap();
        let before = renderer.geometry(1.25);
        let px = renderer.font_px(1.25);
        let missing = '\u{10ffff}';
        let (font, replacement) = renderer.raster_source(missing);
        assert_ne!(replacement, missing);
        let expected = font.rasterize(replacement, px);
        assert!(expected.1.iter().any(|&alpha| alpha != 0));
        assert_eq!(renderer.glyph_bitmap(missing, px), &expected);
        let primary = &renderer.fonts[0];
        let fallback = renderer.fonts[1..].iter().find_map(|font| {
            font.chars()
                .keys()
                .copied()
                .find(|&ch| font.metrics(ch, px).width > 0 && primary.lookup_glyph_index(ch) == 0)
                .map(|ch| (ch, font.rasterize(ch, px)))
        });
        if let Some((ch, expected)) = fallback {
            assert_eq!(renderer.glyph_bitmap(ch, px), &expected);
        }
        assert_eq!(renderer.geometry(1.25), before);
    }

    #[test]
    fn editor_renders_gutter_text_status_and_cursor_at_each_scale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.txt");
        std::fs::write(&path, "hello\n世界").unwrap();
        let mut editor = crate::editor::Document::open(&path).unwrap();
        editor.insert_char('!');
        let mut renderer = Renderer::new().unwrap();
        let view = editor.view(40, 6);
        for scale in SCALES {
            let g = renderer.geometry(scale);
            let (width, height) = frame(g, 40, 6);
            let scene = renderer.build_editor_scene(&view, (width, height), true, true, scale);
            assert!(scene.glyphs.iter().any(|glyph| glyph.ch == '!'));
            assert!(!scene.overlays.is_empty());
            let mut pixels = vec![0; width * height * 4];
            renderer.paint_scene(&scene, width, &mut pixels);
            let cursor = scene.overlays[0];
            assert_eq!(
                pixel(&pixels, width, cursor.x as usize, cursor.y as usize),
                CURSOR
            );
        }
        assert!(view.status.contains("demo.txt"));
        assert_eq!(view.rows[0].text, "!hello");
    }
}
