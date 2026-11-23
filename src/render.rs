// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::terminal::{Screen, resolve_color};
use fontdue::{Font, FontSettings, Metrics};
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

/// A glyph quad referencing a slot in the glyph atlas. `uv` are fractions of
/// the rasterized glyph bitmap, so clipped glyphs keep correct texture
/// coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneGlyph {
    pub ch: char,
    pub px_size: f32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub uv: [f32; 4],
    pub color: [f32; 4],
}

/// Backend-independent drawing instructions. The OpenGL backend turns this
/// into batched vertex buffers; the software backend does not use it.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub rects: Vec<SceneRect>,
    pub glyphs: Vec<SceneGlyph>,
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

#[derive(Clone, Copy)]
pub struct Geometry {
    pub cell_width: usize,
    pub cell_height: usize,
    pub padding: usize,
    pub baseline: isize,
}
impl Geometry {
    pub fn from_theme(theme: &crate::theme::Theme) -> Self {
        let scale = theme.font_size / 16.0;
        Self {
            cell_width: (10.0 * scale).round().max(1.0) as usize,
            cell_height: (18.0 * scale).round().max(1.0) as usize,
            padding: theme.padding,
            baseline: (14.0 * scale).round() as isize,
        }
    }
    pub fn for_window(mut self, width: usize, height: usize) -> Self {
        self.padding = self
            .padding
            .min(width.saturating_sub(self.cell_width) / 2)
            .min(height.saturating_sub(self.cell_height) / 2);
        self
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
pub struct Renderer {
    fonts: Vec<Font>,
    cache: HashMap<char, (Metrics, Vec<u8>)>,
    scaled_cache: HashMap<(char, u32), (Metrics, Vec<u8>)>,
    theme: crate::theme::Theme,
    pub geometry: Geometry,
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
                "%{file}\\n",
                &format!("{}:spacing=mono:style=Regular", theme.font_family),
            ])
            .output()
            && output.status.success()
        {
            paths.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_owned),
            );
        }
        paths.extend(
            [
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
                "/usr/share/fonts/liberation-mono-fonts/LiberationMono-Regular.ttf",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        let mut fonts = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            if let Ok(data) = std::fs::read(path)
                && let Ok(font) = Font::from_bytes(data, FontSettings::default())
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
            fonts,
            cache: HashMap::new(),
            scaled_cache: HashMap::new(),
            geometry: Geometry::from_theme(&theme),
            theme,
        })
    }
    pub fn retheme(&mut self, theme: crate::theme::Theme) -> io::Result<()> {
        if self.theme.font_family != theme.font_family || self.theme.font_size != theme.font_size {
            *self = Self::with_theme(theme)?;
        } else {
            self.geometry = Geometry::from_theme(&theme);
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
    fn glyph(&mut self, ch: char) -> &(Metrics, Vec<u8>) {
        if self.cache.len() >= 4096 && !self.cache.contains_key(&ch) {
            self.cache.clear();
        }
        self.cache.entry(ch).or_insert_with(|| {
            let font = self
                .fonts
                .iter()
                .find(|font| font.lookup_glyph_index(ch) != 0)
                .unwrap_or(&self.fonts[0]);
            font.rasterize(ch, self.theme.font_size)
        })
    }

    fn glyph_at(&mut self, ch: char, px_size: f32) -> &(Metrics, Vec<u8>) {
        if (px_size - self.theme.font_size).abs() < 0.01 {
            return self.glyph(ch);
        }
        let key = (ch, (px_size * 100.0).round().max(1.0) as u32);
        if self.scaled_cache.len() >= 4096 && !self.scaled_cache.contains_key(&key) {
            self.scaled_cache.clear();
        }
        self.scaled_cache.entry(key).or_insert_with(|| {
            let font = self
                .fonts
                .iter()
                .find(|font| font.lookup_glyph_index(ch) != 0)
                .unwrap_or(&self.fonts[0]);
            font.rasterize(ch, px_size)
        })
    }

    /// Metrics for a glyph rasterized at `px_size`. Used by the GPU scene
    /// builder; the software path keeps using [`Renderer::glyph`].
    pub fn glyph_metrics(&mut self, ch: char, px_size: f32) -> Metrics {
        self.glyph_at(ch, px_size).0
    }

    /// Rasterized glyph bitmap and metrics. The bitmap is cloned because the
    /// atlas keeps it across frames while the renderer keeps its own cache.
    pub fn glyph_bitmap(&mut self, ch: char, px_size: f32) -> (Metrics, Vec<u8>) {
        let (metrics, bitmap) = self.glyph_at(ch, px_size);
        (*metrics, bitmap.clone())
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
        self.render_into(
            screen,
            (width, height),
            selection,
            focused,
            blink_on,
            &mut pixels,
        );
        pixels
    }
    pub fn render_into(
        &mut self,
        screen: &Screen,
        (width, height): (usize, usize),
        selection: &crate::selection::Selection,
        focused: bool,
        blink_on: bool,
        pixels: &mut Vec<u8>,
    ) {
        let Geometry {
            cell_width,
            cell_height,
            padding,
            baseline,
        } = self.geometry.for_window(width, height);
        let bg = [
            self.theme.background[2],
            self.theme.background[1],
            self.theme.background[0],
            255,
        ];
        let cursor = [
            self.theme.cursor[2],
            self.theme.cursor[1],
            self.theme.cursor[0],
            255,
        ];
        let selection_foreground = self.theme.selection_foreground;
        let background = bg;
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
        pixels.resize(width * height * 4, 0);
        for pixel in pixels.as_chunks_mut::<4>().0.iter_mut() {
            pixel.copy_from_slice(&bg);
        }
        for y in 0..screen.rows {
            for x in 0..screen.columns {
                let Some(cell) = screen.get(x, y) else {
                    continue;
                };
                let bg = if block_covers(x, y) {
                    cursor
                } else if selection.contains(screen, y, x) {
                    [
                        self.theme.selection[2],
                        self.theme.selection[1],
                        self.theme.selection[0],
                        255,
                    ]
                } else {
                    let [r, g, b] = resolve_color(cell.bg, cell.bg_origin, &self.theme);
                    [b, g, r, 255]
                };
                if bg == background {
                    continue;
                }
                for py in (padding + y * cell_height).min(height.saturating_sub(padding))
                    ..(padding + (y + 1) * cell_height).min(height.saturating_sub(padding))
                {
                    for px in (padding + x * cell_width).min(width.saturating_sub(padding))
                        ..(padding + (x + 1) * cell_width).min(width.saturating_sub(padding))
                    {
                        pixels[(py * width + px) * 4..(py * width + px) * 4 + 4]
                            .copy_from_slice(&bg);
                    }
                }
            }
        }
        for y in 0..screen.rows {
            for x in 0..screen.columns {
                let Some(cell) = screen.get(x, y) else {
                    continue;
                };
                if cell.continuation {
                    continue;
                }
                let foreground = if block_covers(x, y) {
                    self.theme.background
                } else if selection.contains(screen, y, x) {
                    selection_foreground
                } else {
                    resolve_color(cell.fg, cell.fg_origin, &self.theme)
                };
                for ch in std::iter::once(cell.character).chain(cell.combining.iter().copied()) {
                    if ch == ' ' {
                        continue;
                    }
                    let (metrics, bitmap) = self.glyph(ch);
                    let cell_left = (padding + x * cell_width) as isize;
                    let cell_right =
                        cell_left + (cell_width * if cell.wide { 2 } else { 1 }) as isize;
                    let left = cell_left
                        + ((cell_right - cell_left - metrics.advance_width.round() as isize) / 2)
                            .max(0)
                        + metrics.xmin as isize;
                    let top = (padding + y * cell_height) as isize + baseline
                        - metrics.height as isize
                        - metrics.ymin as isize;
                    for gy in 0..metrics.height {
                        for gx in 0..metrics.width {
                            let px = left + gx as isize;
                            let py = top + gy as isize;
                            if px < cell_left
                                || px >= cell_right
                                || py < (padding + y * cell_height) as isize
                                || py >= (padding + (y + 1) * cell_height) as isize
                                || px < 0
                                || py < 0
                                || px >= width.saturating_sub(padding) as isize
                                || py >= height.saturating_sub(padding) as isize
                            {
                                continue;
                            }
                            let alpha = bitmap[gy * metrics.width + gx] as u32;
                            let offset = (py as usize * width + px as usize) * 4;
                            for c in 0..3 {
                                pixels[offset + c] = ((foreground[2 - c] as u32 * alpha
                                    + pixels[offset + c] as u32 * (255 - alpha))
                                    / 255)
                                    as u8;
                            }
                        }
                    }
                }
            }
        }
        let cursor_x = padding + cursor_col * cell_width;
        let cursor_y = padding + cursor_row * cell_height;
        if cursor_shown && !block {
            for dy in 0..cell_height {
                for dx in 0..cell_width {
                    let draw = if !focused {
                        dx == 0 || dy == 0 || dx + 1 == cell_width || dy + 1 == cell_height
                    } else {
                        match cursor_shape {
                            crate::theme::CursorShape::Block => true,
                            crate::theme::CursorShape::Beam => dx < 2,
                            crate::theme::CursorShape::Underline => {
                                dy >= cell_height.saturating_sub(3)
                                    && dy < cell_height.saturating_sub(1)
                            }
                        }
                    };
                    let x = cursor_x + dx;
                    let y = cursor_y + dy;
                    if draw
                        && x < width.saturating_sub(padding)
                        && y < height.saturating_sub(padding)
                    {
                        pixels[(y * width + x) * 4..(y * width + x) * 4 + 4]
                            .copy_from_slice(&cursor);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        text: &str,
        x: isize,
        top: isize,
        cell_width: usize,
        cell_height: usize,
        baseline: isize,
        clip: (isize, isize),
        foreground: [u8; 3],
        selection: Option<(usize, usize)>,
        selection_foreground: [u8; 3],
        pixels: &mut [u8],
        width: usize,
        height: usize,
        padding: usize,
    ) {
        let mut cell = 0isize;
        let mut last_left = x;
        for ch in text.chars() {
            let advance = cell_advance(ch) as isize;
            let cell_left = x + cell * cell_width as isize;
            let color = if selection
                .is_some_and(|(start, end)| cell >= start as isize && cell < end as isize)
            {
                selection_foreground
            } else {
                foreground
            };
            if advance == 0 {
                self.blit_glyph(
                    ch,
                    last_left,
                    last_left + cell_width as isize,
                    top,
                    cell_height,
                    baseline,
                    clip,
                    color,
                    pixels,
                    width,
                    height,
                    padding,
                );
                continue;
            }
            if cell_left >= clip.1 {
                break;
            }
            if ch != ' ' && !ch.is_control() {
                let right = cell_left + advance * cell_width as isize;
                self.blit_glyph(
                    ch,
                    cell_left,
                    right,
                    top,
                    cell_height,
                    baseline,
                    clip,
                    color,
                    pixels,
                    width,
                    height,
                    padding,
                );
                last_left = cell_left;
            }
            cell += advance;
        }
    }

    /// Paint the editor (gutter, text, selection, cursor and status bar).
    pub fn render_editor_into(
        &mut self,
        view: &crate::editor::EditorView,
        (width, height): (usize, usize),
        focused: bool,
        blink_on: bool,
        pixels: &mut Vec<u8>,
    ) {
        let Geometry {
            cell_width,
            cell_height,
            padding,
            baseline,
        } = self.geometry.for_window(width, height);
        let bg = [
            self.theme.background[2],
            self.theme.background[1],
            self.theme.background[0],
            255,
        ];
        let cursor_color = [
            self.theme.cursor[2],
            self.theme.cursor[1],
            self.theme.cursor[0],
            255,
        ];
        let selected_bg = [
            self.theme.selection[2],
            self.theme.selection[1],
            self.theme.selection[0],
            255,
        ];
        let foreground = self.theme.foreground;
        let dim = self.theme.ansi[8];
        let text_x = padding as isize + (view.gutter_cells * cell_width) as isize;
        let text_right = (text_x + (view.text_columns * cell_width) as isize)
            .min(width.saturating_sub(padding) as isize);
        pixels.resize(width * height * 4, 0);
        for pixel in pixels.as_chunks_mut::<4>().0.iter_mut() {
            pixel.copy_from_slice(&bg);
        }
        for (index, row) in view.rows.iter().enumerate() {
            let top = padding as isize + (index * cell_height) as isize;
            if top >= (height.saturating_sub(padding)) as isize {
                break;
            }
            if let Some((start, end)) = row.selection {
                let left = (text_x + start as isize * cell_width as isize)
                    .clamp(padding as isize, text_right);
                let right = (text_x + end as isize * cell_width as isize)
                    .clamp(padding as isize, text_right);
                for py in top..(top + cell_height as isize).min(height as isize) {
                    for px in left..right {
                        if px >= 0 && py >= 0 {
                            pixels[(py as usize * width + px as usize) * 4
                                ..(py as usize * width + px as usize) * 4 + 4]
                                .copy_from_slice(&selected_bg);
                        }
                    }
                }
            }
            let gutter_fg = if row.current { foreground } else { dim };
            self.draw_text(
                &row.gutter,
                padding as isize,
                top,
                cell_width,
                cell_height,
                baseline,
                (padding as isize, text_x),
                gutter_fg,
                None,
                self.theme.selection_foreground,
                pixels,
                width,
                height,
                padding,
            );
            self.draw_text(
                &row.text,
                text_x,
                top,
                cell_width,
                cell_height,
                baseline,
                (text_x, text_right),
                foreground,
                row.selection,
                self.theme.selection_foreground,
                pixels,
                width,
                height,
                padding,
            );
        }
        let bar_top = padding as isize + (view.status_row * cell_height) as isize;
        if bar_top < (height.saturating_sub(padding)) as isize {
            let bar_bg = if view.status_error {
                [
                    self.theme.ansi[1][2],
                    self.theme.ansi[1][1],
                    self.theme.ansi[1][0],
                    255,
                ]
            } else {
                selected_bg
            };
            for py in bar_top..(bar_top + cell_height as isize).min(height as isize) {
                for px in padding as isize..text_right {
                    if px >= 0 && py >= 0 {
                        pixels[(py as usize * width + px as usize) * 4
                            ..(py as usize * width + px as usize) * 4 + 4]
                            .copy_from_slice(&bar_bg);
                    }
                }
            }
            self.draw_text(
                &view.status,
                padding as isize + cell_width as isize / 2,
                bar_top,
                cell_width,
                cell_height,
                baseline,
                (padding as isize, text_right),
                self.theme.selection_foreground,
                None,
                self.theme.selection_foreground,
                pixels,
                width,
                height,
                padding,
            );
        }
        if let Some((row, column, cells)) = view.cursor
            && focused
            && (!self.theme.cursor_blink || blink_on)
        {
            let left = text_x + column as isize * cell_width as isize;
            let top = padding as isize + row as isize * cell_height as isize;
            let right = (left + cells.max(1) as isize * cell_width as isize).min(text_right);
            for py in top..(top + cell_height as isize).min(height.saturating_sub(padding) as isize)
            {
                for px in left..right {
                    let draw = match self.theme.cursor_shape {
                        crate::theme::CursorShape::Block => true,
                        crate::theme::CursorShape::Beam => px < left + 2,
                        crate::theme::CursorShape::Underline => {
                            py >= top + cell_height as isize - 3
                                && py < top + cell_height as isize - 1
                        }
                    };
                    if draw && px >= 0 && py >= 0 && px < width as isize {
                        pixels[(py as usize * width + px as usize) * 4
                            ..(py as usize * width + px as usize) * 4 + 4]
                            .copy_from_slice(&cursor_color);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blit_glyph(
        &mut self,
        ch: char,
        cell_left: isize,
        cell_right: isize,
        top: isize,
        cell_height: usize,
        baseline: isize,
        clip: (isize, isize),
        foreground: [u8; 3],
        pixels: &mut [u8],
        width: usize,
        height: usize,
        padding: usize,
    ) {
        let (metrics, bitmap) = self.glyph(ch);
        let left = cell_left
            + ((cell_right - cell_left - metrics.advance_width.round() as isize) / 2).max(0)
            + metrics.xmin as isize;
        let glyph_top = top + baseline - metrics.height as isize - metrics.ymin as isize;
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let px = left + gx as isize;
                let py = glyph_top + gy as isize;
                if px < cell_left
                    || px >= cell_right
                    || px < clip.0
                    || px >= clip.1
                    || py < top
                    || py >= top + cell_height as isize
                    || px < 0
                    || py < 0
                    || px >= width.saturating_sub(padding) as isize
                    || py >= height.saturating_sub(padding) as isize
                {
                    continue;
                }
                let alpha = bitmap[gy * metrics.width + gx] as u32;
                let offset = (py as usize * width + px as usize) * 4;
                for c in 0..3 {
                    pixels[offset + c] = ((foreground[2 - c] as u32 * alpha
                        + pixels[offset + c] as u32 * (255 - alpha))
                        / 255) as u8;
                }
            }
        }
    }
}
impl Renderer {
    /// Build a backend-independent scene for the terminal grid.
    ///
    /// `logical` is the Wayland surface size in logical pixels; `scale` is the
    /// fractional buffer scale. Coordinates in the scene are physical pixels.
    /// `rows` limits emission to a damaged row range; `full_clear` must be true
    /// whenever the caller is not preserving the previous frame.
    #[allow(clippy::too_many_arguments)]
    pub fn build_terminal_scene(
        &mut self,
        screen: &Screen,
        logical: (usize, usize),
        selection: &crate::selection::Selection,
        focused: bool,
        blink_on: bool,
        scale: f32,
        rows: Option<(usize, usize)>,
    ) -> Scene {
        let scale = scale.max(0.1);
        let base = self.geometry.for_window(logical.0.max(1), logical.1.max(1));
        let width = (logical.0 as f32 * scale).round().max(1.0);
        let height = (logical.1 as f32 * scale).round().max(1.0);
        let cell_w = base.cell_width as f32 * scale;
        let cell_h = base.cell_height as f32 * scale;
        let padding = base.padding as f32 * scale;
        let baseline = base.baseline as f32 * scale;
        let px_size = (self.theme.font_size * scale).max(1.0);
        let inner_left = padding;
        let inner_right = (width - padding).max(inner_left);
        let inner_top = padding;
        let inner_bottom = (height - padding).max(inner_top);
        let inner_w = (inner_right - inner_left).max(0.0);
        let inner_h = (inner_bottom - inner_top).max(0.0);

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
                let right = (left + cell_w * if cell.wide { 2.0 } else { 1.0 }).min(inner_right);
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
                let cell_right =
                    (cell_left + cell_w * if cell.wide { 2.0 } else { 1.0 }).min(inner_right);
                for ch in std::iter::once(cell.character).chain(cell.combining.iter().copied()) {
                    if ch == ' ' {
                        continue;
                    }
                    self.push_scene_glyph(
                        &mut scene,
                        ch,
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
                    scene.rects.push(SceneRect {
                        x,
                        y,
                        w,
                        h,
                        color: cursor_color,
                    });
                };
                if !focused {
                    let t = scale.max(1.0);
                    push(cursor_x, cursor_y, cursor_right - cursor_x, t);
                    push(cursor_x, cursor_bottom - t, cursor_right - cursor_x, t);
                    push(cursor_x, cursor_y, t, cursor_bottom - cursor_y);
                    push(cursor_right - t, cursor_y, t, cursor_bottom - cursor_y);
                } else {
                    match cursor_shape {
                        crate::theme::CursorShape::Block => {}
                        crate::theme::CursorShape::Beam => {
                            push(
                                cursor_x,
                                cursor_y,
                                (2.0 * scale).min(cursor_right - cursor_x),
                                cursor_bottom - cursor_y,
                            );
                        }
                        crate::theme::CursorShape::Underline => {
                            let thickness = (2.0 * scale).min(cursor_bottom - cursor_y);
                            push(
                                cursor_x,
                                cursor_bottom - thickness - scale,
                                cursor_right - cursor_x,
                                thickness,
                            );
                        }
                    }
                }
            }
        }
        let _ = inner_w;
        let _ = inner_h;
        scene
    }

    #[allow(clippy::too_many_arguments)]
    fn push_scene_glyph(
        &mut self,
        scene: &mut Scene,
        ch: char,
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
        let metrics = self.glyph_metrics(ch, px_size);
        if metrics.width == 0 || metrics.height == 0 {
            return;
        }
        let left = cell_left
            + ((cell_right - cell_left - metrics.advance_width.round()) / 2.0).max(0.0)
            + metrics.xmin as f32;
        let glyph_top = top + baseline - metrics.height as f32 - metrics.ymin as f32;
        let right = left + metrics.width as f32;
        let glyph_bottom = glyph_top + metrics.height as f32;
        let x0 = left.max(cell_left).max(clip_left);
        let x1 = right.min(cell_right).min(clip_right);
        let y0 = glyph_top.max(top);
        let y1 = glyph_bottom.min(bottom);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let inv_w = 1.0 / metrics.width as f32;
        let inv_h = 1.0 / metrics.height as f32;
        scene.glyphs.push(SceneGlyph {
            ch,
            px_size,
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
            uv: [
                (x0 - left) * inv_w,
                (y0 - glyph_top) * inv_h,
                (x1 - left) * inv_w,
                (y1 - glyph_top) * inv_h,
            ],
            color,
        });
    }

    /// Build a scene for the built-in editor. Editor frames are small and
    /// keyboard driven, so the GPU backend repaints the whole window.
    pub fn build_editor_scene(
        &mut self,
        view: &crate::editor::EditorView,
        logical: (usize, usize),
        focused: bool,
        blink_on: bool,
        scale: f32,
    ) -> Scene {
        let scale = scale.max(0.1);
        let base = self.geometry.for_window(logical.0.max(1), logical.1.max(1));
        let width = (logical.0 as f32 * scale).round().max(1.0);
        let height = (logical.1 as f32 * scale).round().max(1.0);
        let cell_w = base.cell_width as f32 * scale;
        let cell_h = base.cell_height as f32 * scale;
        let padding = base.padding as f32 * scale;
        let baseline = base.baseline as f32 * scale;
        let px_size = (self.theme.font_size * scale).max(1.0);
        let text_x = padding + view.gutter_cells as f32 * cell_w;
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
        for (index, row) in view.rows.iter().enumerate() {
            let top = padding + index as f32 * cell_h;
            if top >= (height - padding) {
                break;
            }
            let bottom = (top + cell_h).min(height);
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
                padding,
                top,
                cell_w,
                cell_h,
                baseline,
                padding,
                text_x,
                gutter_fg,
                None,
                selection_foreground,
                px_size,
            );
            self.push_text_scene(
                &mut scene,
                &row.text,
                text_x,
                top,
                cell_w,
                cell_h,
                baseline,
                text_x,
                text_right,
                foreground,
                row.selection,
                selection_foreground,
                px_size,
            );
        }
        let bar_top = padding + view.status_row as f32 * cell_h;
        if bar_top < (height - padding) {
            let bar_bg = if view.status_error {
                rgba(self.theme.ansi[1])
            } else {
                selected_bg
            };
            let bar_bottom = (bar_top + cell_h).min(height);
            scene.rects.push(SceneRect {
                x: padding,
                y: bar_top,
                w: (text_right - padding).max(0.0),
                h: bar_bottom - bar_top,
                color: bar_bg,
            });
            self.push_text_scene(
                &mut scene,
                &view.status,
                padding + cell_w / 2.0,
                bar_top,
                cell_w,
                cell_h,
                baseline,
                padding,
                text_right,
                selection_foreground,
                None,
                selection_foreground,
                px_size,
            );
        }
        if let Some((row, column, cells)) = view.cursor
            && focused
            && (!self.theme.cursor_blink || blink_on)
        {
            let left = text_x + column as f32 * cell_w;
            let top = padding + row as f32 * cell_h;
            let right = (left + cells.max(1) as f32 * cell_w).min(text_right);
            let bottom = (top + cell_h).min(height - padding);
            match self.theme.cursor_shape {
                crate::theme::CursorShape::Block => {
                    if right > left && bottom > top {
                        scene.rects.push(SceneRect {
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
                        scene.rects.push(SceneRect {
                            x: left,
                            y: top,
                            w: (2.0 * scale).min(right - left),
                            h: bottom - top,
                            color: cursor_color,
                        });
                    }
                }
                crate::theme::CursorShape::Underline => {
                    if right > left && bottom > top {
                        let thickness = (2.0 * scale).min(bottom - top);
                        scene.rects.push(SceneRect {
                            x: left,
                            y: (bottom - thickness - scale).max(top),
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
        cell_h: f32,
        baseline: f32,
        clip_left: f32,
        clip_right: f32,
        foreground: [f32; 4],
        selection: Option<(usize, usize)>,
        selection_foreground: [f32; 4],
        px_size: f32,
    ) {
        let mut cell = 0isize;
        let mut last_left = x;
        let bottom = top + cell_h;
        for ch in text.chars() {
            let advance = cell_advance(ch) as isize;
            let cell_left = x + cell as f32 * cell_w;
            let color = if selection
                .is_some_and(|(start, end)| cell >= start as isize && cell < end as isize)
            {
                selection_foreground
            } else {
                foreground
            };
            if advance == 0 {
                self.push_scene_glyph(
                    scene,
                    ch,
                    px_size,
                    last_left,
                    last_left + cell_w,
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
            if ch != ' ' && !ch.is_control() {
                let right = cell_left + advance as f32 * cell_w;
                self.push_scene_glyph(
                    scene, ch, px_size, cell_left, right, top, bottom, baseline, clip_left,
                    clip_right, color,
                );
                last_left = cell_left;
            }
            cell += advance;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;
    const BG: [u8; 4] = [32, 24, 18, 255];
    const CURSOR: [u8; 4] = [191, 174, 155, 255];
    const PADDING: usize = 12;
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
        let pixels = renderer.render(terminal.screen(), 44, 60);
        let background = [32, 24, 18, 255];
        let painted = |top: usize, bottom: usize| {
            (top..bottom).any(|y| {
                (0..44).any(|x| pixels[(y * 44 + x) * 4..(y * 44 + x) * 4 + 4] != background)
            })
        };
        assert!(!painted(12, 30), "the first display row must stay blank");
        assert!(painted(30, 48), "the glyph must be drawn in the last row");

        terminal.process(b"\x1b[?25h");
        let selection = crate::selection::Selection::default();
        let pixels = renderer.render_with_cursor(terminal.screen(), 44, 60, &selection, true, true);
        assert_eq!(&pixels[(30 * 44 + 22) * 4..(30 * 44 + 22) * 4 + 4], &CURSOR);
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
        let width = 104;
        let pixels = renderer.render(t.screen(), width, 42);
        let pixel = |x: usize| &pixels[(12 * width + x) * 4..(12 * width + x) * 4 + 4];
        assert_eq!(pixel(12), [6, 5, 4, 255]);
        assert_eq!(pixel(22), [3, 2, 1, 255]);
        assert_eq!(pixel(32), [10, 9, 8, 255]);
    }

    #[test]
    fn block_cursor_preserves_glyph_and_config_disables_application_blink() {
        let mut r = Renderer::new().unwrap();
        let mut t = Terminal::new(2, 1);
        t.process(b"A\r\x1b[1 q");
        let selection = crate::selection::Selection::default();
        let shown = r.render_with_cursor(t.screen(), 44, 42, &selection, true, false);
        t.process(b"\x1b[?25l");
        assert_ne!(shown, r.render(t.screen(), 44, 42));
        t.process(b"\x1b[?25h\x1b[2K");
        assert_ne!(shown, r.render(t.screen(), 44, 42));
    }

    #[test]
    fn grid_geometry_uses_padding_and_minimum() {
        assert_eq!(
            Geometry::from_theme(&crate::theme::Theme::default()).grid_size(800, 500),
            (77, 26)
        );
        assert_eq!(
            Geometry::from_theme(&crate::theme::Theme::default()).grid_size(34, 42),
            (1, 1)
        );
        assert_eq!(
            Geometry::from_theme(&crate::theme::Theme::default()).grid_size(1, 1),
            (1, 1)
        );
        assert_eq!(
            Geometry::from_theme(&crate::theme::Theme::default()).grid_size(44, 60),
            (2, 2)
        );
    }
    #[test]
    fn geometry_tracks_theme_metrics() {
        let theme = crate::theme::Theme::parse("font_size=32.0\npadding=4").unwrap();
        let g = Geometry::from_theme(&theme);
        assert_eq!((g.cell_width, g.cell_height), (20, 36));
        assert_eq!(g.grid_size(808, 512), (40, 14));
    }
    #[test]
    fn custom_theme_colors_render() {
        let theme =
            crate::theme::Theme::parse("background='#112233'\nforeground='#abcdef'").unwrap();
        let mut renderer = Renderer::with_theme(theme.clone()).unwrap();
        let mut terminal = Terminal::with_theme(2, 1, theme);
        terminal.process(b"A");
        assert_eq!(terminal.screen().get(0, 0).unwrap().fg, [171, 205, 239]);
        let pixels = renderer.render(terminal.screen(), 44, 42);
        assert_eq!(&pixels[..4], &[51, 34, 17, 255]);
        assert_eq!(
            &pixels[(12 * 44 + 22) * 4..(12 * 44 + 22) * 4 + 4],
            &[51, 34, 17, 255]
        );
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
            let shown = r.render_with_cursor(t.screen(), 44, 42, &selection, true, true);
            let hidden = r.render_with_cursor(t.screen(), 44, 42, &selection, true, false);
            assert_ne!(shown, hidden);
            let unfocused = r.render_with_cursor(t.screen(), 44, 42, &selection, false, false);
            assert_ne!(unfocused, hidden);
        }
    }
    #[test]
    fn selection_highlight_does_not_modify_grid() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(2, 1);
        terminal.process(b"A\x1b[?25l");
        let mut selection = crate::selection::Selection::default();
        selection.press(
            terminal.screen(),
            crate::selection::Position { row: 0, col: 0 },
            1,
            12.0,
            12.0,
        );
        selection.release();
        let pixels = renderer.render_selection(terminal.screen(), 44, 42, &selection);
        assert_eq!(
            &pixels[(12 * 44 + 12) * 4..(12 * 44 + 12) * 4 + 4],
            &[192, 192, 192, 255]
        );
        assert_eq!(terminal.screen().get(0, 0).unwrap().character, 'A');
    }
    #[test]
    fn padding_is_background() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(2, 1);
        terminal.process(b"\x1b[41mA");
        let pixels = renderer.render(terminal.screen(), 44, 42);
        for y in 0..42 {
            for x in 0..44 {
                if !(PADDING..44 - PADDING).contains(&x) || !(PADDING..42 - PADDING).contains(&y) {
                    assert_eq!(&pixels[(y * 44 + x) * 4..(y * 44 + x) * 4 + 4], &BG);
                }
            }
        }
    }
    #[test]
    fn renders_background_and_cursor_visibility() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(2, 1);
        terminal.process(b"\x1b[48;2;10;20;30m \x1b[?25l");
        let pixels = renderer.render(terminal.screen(), 44, 42);
        assert_eq!(
            &pixels[(12 * 44 + 12) * 4..(12 * 44 + 12) * 4 + 4],
            &[30, 20, 10, 255]
        );
        let offset = (27 * 44 + 22) * 4;
        assert_ne!(&pixels[offset..offset + 4], &CURSOR);
        terminal.process(b"\x1b[?25h");
        let pixels = renderer.render(terminal.screen(), 44, 42);
        assert_eq!(&pixels[offset..offset + 4], &CURSOR);
    }
    #[test]
    fn wide_glyph_does_not_paint_adjacent_cell() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(4, 1);
        terminal.process("界".as_bytes());
        let pixels = renderer.render(terminal.screen(), 64, 42);
        // The third cell contains the cursor; the fourth remains untouched.
        for y in 12..30 {
            for x in 42..52 {
                assert_eq!(&pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4], &BG);
            }
        }
    }
    #[test]
    fn editor_renders_gutter_text_status_and_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.txt");
        std::fs::write(&path, "hello\n世界").unwrap();
        let mut editor = crate::editor::Editor::open(&path).unwrap();
        editor.insert_char('!');
        let view = editor.view(40, 6);
        let mut renderer = Renderer::new().unwrap();
        let mut pixels = Vec::new();
        renderer.render_editor_into(&view, (400, 180), true, true, &mut pixels);
        assert_eq!(pixels.len(), 400 * 180 * 4);
        let bg = [32, 24, 18, 255];
        assert!(
            pixels.as_chunks::<4>().0.iter().any(|pixel| *pixel != bg),
            "editor must paint text and the status bar"
        );
        assert!(view.status.contains("demo.txt"));
        assert_eq!(view.rows[0].text, "!hello");
    }

    fn to_bgra(color: [f32; 4]) -> [u8; 4] {
        [
            (color[2] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[3] * 255.0).round().clamp(0.0, 255.0) as u8,
        ]
    }

    /// CPU reference rasterizer for the GPU scene. It uses exactly the same
    /// alpha blending rules as the software renderer, so any scene-layout
    /// regression shows up as a pixel mismatch.
    fn rasterize_scene(
        renderer: &mut Renderer,
        scene: &Scene,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        let background = to_bgra(scene.background);
        let mut pixels = vec![0u8; width * height * 4];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            *pixel = background;
        }
        for rect in &scene.rects {
            let color = to_bgra(rect.color);
            let x0 = rect.x.round() as isize;
            let y0 = rect.y.round() as isize;
            let x1 = (rect.x + rect.w).round() as isize;
            let y1 = (rect.y + rect.h).round() as isize;
            for y in y0.max(0)..y1.min(height as isize) {
                for x in x0.max(0)..x1.min(width as isize) {
                    let offset = (y as usize * width + x as usize) * 4;
                    pixels[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
        for glyph in &scene.glyphs {
            let (metrics, bitmap) = renderer.glyph_bitmap(glyph.ch, glyph.px_size);
            let sw = metrics.width as isize;
            let sh = metrics.height as isize;
            if sw == 0 || sh == 0 {
                continue;
            }
            let sx0 = (glyph.uv[0] * sw as f32).round() as isize;
            let sy0 = (glyph.uv[1] * sh as f32).round() as isize;
            let color = to_bgra(glyph.color);
            let x0 = glyph.x.round() as isize;
            let y0 = glyph.y.round() as isize;
            let x1 = (glyph.x + glyph.w).round() as isize;
            let y1 = (glyph.y + glyph.h).round() as isize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let sx = sx0 + (x - x0);
                    let sy = sy0 + (y - y0);
                    if sx < 0 || sy < 0 || sx >= sw || sy >= sh {
                        continue;
                    }
                    if x < 0 || y < 0 || x >= width as isize || y >= height as isize {
                        continue;
                    }
                    let alpha = bitmap[(sy * sw + sx) as usize] as u32;
                    let offset = (y as usize * width + x as usize) * 4;
                    for channel in 0..3 {
                        pixels[offset + channel] = ((color[channel] as u32 * alpha
                            + pixels[offset + channel] as u32 * (255 - alpha))
                            / 255) as u8;
                    }
                }
            }
        }
        pixels
    }

    fn terminal_scene_matches_software(theme: crate::theme::Theme, text: &[u8], width: usize) {
        let height = 120;
        let mut renderer = Renderer::with_theme(theme.clone()).unwrap();
        let mut terminal = Terminal::with_theme(8, 4, theme);
        terminal.process(text);
        let mut selection = crate::selection::Selection::default();
        selection.press(
            terminal.screen(),
            crate::selection::Position { row: 0, col: 1 },
            1,
            0.,
            0.,
        );
        selection.motion(
            terminal.screen(),
            crate::selection::Position { row: 0, col: 4 },
        );
        selection.release();
        let scene = renderer.build_terminal_scene(
            terminal.screen(),
            (width, height),
            &selection,
            true,
            true,
            1.0,
            None,
        );
        let gpu = rasterize_scene(&mut renderer, &scene, width, height);
        let software =
            renderer.render_with_cursor(terminal.screen(), width, height, &selection, true, true);
        assert_eq!(gpu.len(), software.len());
        if gpu != software {
            let index = gpu.iter().zip(&software).position(|(a, b)| a != b).unwrap();
            let pixel = index / 4;
            panic!(
                "GPU scene differs from software at pixel ({}, {}) byte {}: {:?} vs {:?}",
                pixel % width,
                pixel / width,
                index % 4,
                &gpu[index - index % 4..index - index % 4 + 4],
                &software[index - index % 4..index - index % 4 + 4],
            );
        }
    }

    #[test]
    fn gpu_scene_matches_software_for_color_unicode_and_selection() {
        let theme = crate::theme::Theme {
            cursor_shape: crate::theme::CursorShape::Block,
            cursor_blink: false,
            ..Default::default()
        };
        terminal_scene_matches_software(
            theme,
            "A\x1b[41mB\x1b[0m Привет 世界 e\u{301}".as_bytes(),
            220,
        );
    }

    #[test]
    fn gpu_scene_matches_software_for_all_cursor_shapes_and_layout() {
        for shape in [
            crate::theme::CursorShape::Block,
            crate::theme::CursorShape::Beam,
            crate::theme::CursorShape::Underline,
        ] {
            let theme = crate::theme::Theme {
                cursor_shape: shape,
                cursor_blink: false,
                layout: crate::terminal::LayoutMode::BottomInput,
                ..Default::default()
            };
            terminal_scene_matches_software(theme, b"one\r\ntwo\x1b[48;2;1;2;3m three", 240);
        }
    }

    #[test]
    fn gpu_editor_scene_matches_software() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scene.txt");
        std::fs::write(&path, "first line\n世界 second\nthird").unwrap();
        let mut editor = crate::editor::Editor::open(&path).unwrap();
        editor.insert_char('!');
        let mut renderer = Renderer::new().unwrap();
        let view = editor.view(40, 6);
        let scene = renderer.build_editor_scene(&view, (400, 180), true, true, 1.0);
        let gpu = rasterize_scene(&mut renderer, &scene, 400, 180);
        let mut software = Vec::new();
        renderer.render_editor_into(&view, (400, 180), true, true, &mut software);
        if gpu != software {
            let index = gpu.iter().zip(&software).position(|(a, b)| a != b).unwrap();
            let pixel = index / 4;
            panic!(
                "editor scene differs at ({}, {}) byte {}: {:?} vs {:?}",
                pixel % 400,
                pixel / 400,
                index % 4,
                &gpu[index - index % 4..index - index % 4 + 4],
                &software[index - index % 4..index - index % 4 + 4],
            );
        }
    }

    #[test]
    fn renders_cyrillic_and_caches_glyph() {
        let mut renderer = Renderer::new().unwrap();
        let mut terminal = Terminal::new(80, 25);
        let empty = renderer.render(terminal.screen(), 800, 500);
        terminal.process("Привет".as_bytes());
        let filled = renderer.render(terminal.screen(), 800, 500);
        assert_ne!(empty, filled);
        assert!(renderer.cache.contains_key(&'П'));
        assert!(renderer.glyph('П').1.iter().any(|&a| a != 0));
        assert_eq!(filled.len(), 800 * 500 * 4);
    }
}
