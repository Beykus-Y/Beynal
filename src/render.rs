use crate::terminal::{Screen, resolve_color};
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;
use std::io;
use std::process::Command;

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
        let block = cursor_shown && focused && cursor_shape == crate::theme::CursorShape::Block;
        let block_lead = if screen
            .get(cursor_col, screen.cursor_y)
            .is_some_and(|cell| cell.continuation)
        {
            cursor_col.saturating_sub(1)
        } else {
            cursor_col
        };
        let block_span = if screen
            .get(block_lead, screen.cursor_y)
            .is_some_and(|cell| cell.wide)
        {
            2
        } else {
            1
        };
        let block_covers = |x, y| {
            block && y == screen.cursor_y && (block_lead..block_lead + block_span).contains(&x)
        };
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
        let cursor_y = padding + screen.cursor_y * cell_height;
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
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;
    const BG: [u8; 4] = [32, 24, 18, 255];
    const CURSOR: [u8; 4] = [191, 174, 155, 255];
    const PADDING: usize = 12;
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
