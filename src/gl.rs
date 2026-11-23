// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal OpenGL ES 2.0 backend on top of EGL/Wayland.
//!
//! The pipeline is deliberately small and terminal-specific:
//! * one alpha glyph atlas, grown only for newly used glyphs;
//! * batched background/selection/cursor rectangles and batched glyph quads;
//! * scissor-based partial redraw of damaged rows;
//! * presentation through `eglSwapBuffers`/`EGL_KHR_swap_buffers_with_damage`.
//!
//! Everything that can fail returns a `Result` so the caller can fall back to
//! the software backend without ever presenting a black window.

use std::collections::HashMap;
use std::ffi::{c_int, c_void};

use glow::HasContext as _;
use khronos_egl as egl;

use crate::render::{Renderer, Scene, SceneGlyph};

const ATLAS_SIZE: u32 = 1024;
const EGL_BUFFER_AGE_EXT: egl::Int = 0x313D;
const EGL_SWAP_BEHAVIOR: egl::Int = 0x3093;
const EGL_BUFFER_PRESERVED: egl::Int = 0x3094;

/// Damage rectangle in physical pixels, top-left origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

struct GlyphAtlas {
    data: Vec<u8>,
    map: HashMap<(char, u32), Slot>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    reset: bool,
    pending: Vec<Slot>,
}

impl GlyphAtlas {
    fn new() -> Self {
        Self {
            data: vec![0; (ATLAS_SIZE * ATLAS_SIZE) as usize],
            map: HashMap::new(),
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            reset: false,
            pending: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.data.fill(0);
        self.map.clear();
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_h = 0;
        self.pending.clear();
        self.reset = true;
    }

    fn slot(&mut self, ch: char, px_size: f32, renderer: &mut Renderer) -> Slot {
        let key = (ch, (px_size * 100.0).round().max(1.0) as u32);
        if let Some(slot) = self.map.get(&key) {
            return *slot;
        }
        let (metrics, bitmap) = renderer.glyph_bitmap(ch, px_size);
        let w = metrics.width as u32;
        let h = metrics.height as u32;
        if w == 0 || h == 0 || w > ATLAS_SIZE || h > ATLAS_SIZE {
            let slot = Slot::default();
            self.map.insert(key, slot);
            return slot;
        }
        if self.shelf_x + w + 1 > ATLAS_SIZE {
            self.shelf_x = 0;
            self.shelf_y += self.shelf_h + 1;
            self.shelf_h = 0;
        }
        if self.shelf_y + h + 1 > ATLAS_SIZE {
            // The atlas is full: recycle it. Vertices are rebuilt every frame,
            // so no stale UVs survive this reset.
            self.clear();
        }
        let slot = Slot {
            x: self.shelf_x,
            y: self.shelf_y,
            w,
            h,
        };
        self.shelf_x += w + 1;
        self.shelf_h = self.shelf_h.max(h);
        for row in 0..h {
            let src = (row * w) as usize;
            let dst = ((slot.y + row) * ATLAS_SIZE + slot.x) as usize;
            self.data[dst..dst + w as usize].copy_from_slice(&bitmap[src..src + w as usize]);
        }
        self.map.insert(key, slot);
        self.pending.push(slot);
        slot
    }
}

struct EglWindowApi {
    _library: libloading::Library,
    create: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> *mut c_void,
    attached: unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int),
    destroy: unsafe extern "C" fn(*mut c_void),
}

impl EglWindowApi {
    fn load() -> Result<Self, String> {
        let library = unsafe {
            libloading::Library::new("libwayland-egl.so.1")
                .or_else(|_| libloading::Library::new("libwayland-egl.so"))
        }
        .map_err(|error| format!("load libwayland-egl: {error}"))?;
        unsafe {
            let create = *library
                .get::<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> *mut c_void>(
                    b"wl_egl_window_create\0",
                )
                .map_err(|error| format!("wl_egl_window_create: {error}"))?;
            let attached = *library
                .get::<unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int)>(
                    b"wl_egl_window_get_attached_size\0",
                )
                .map_err(|error| format!("wl_egl_window_get_attached_size: {error}"))?;
            let destroy = *library
                .get::<unsafe extern "C" fn(*mut c_void)>(b"wl_egl_window_destroy\0")
                .map_err(|error| format!("wl_egl_window_destroy: {error}"))?;
            Ok(Self {
                _library: library,
                create,
                attached,
                destroy,
            })
        }
    }
}

type SwapBuffersWithDamage =
    unsafe extern "system" fn(egl::EGLDisplay, egl::EGLSurface, *const egl::Int, egl::Int) -> u32;

pub struct GlBackend {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    surface: egl::Surface,
    context: egl::Context,
    config: egl::Config,
    wayland_surface: *mut c_void,
    window: *mut c_void,
    window_api: EglWindowApi,
    gl: glow::Context,
    rect_program: glow::Program,
    glyph_program: glow::Program,
    rect_vbo: glow::Buffer,
    glyph_vbo: glow::Buffer,
    atlas_texture: glow::Texture,
    rect_pos: u32,
    rect_color: u32,
    glyph_pos: u32,
    glyph_uv: u32,
    glyph_color: u32,
    rect_size: Option<glow::UniformLocation>,
    glyph_size: Option<glow::UniformLocation>,
    atlas: GlyphAtlas,
    width: i32,
    height: i32,
    swap_with_damage: Option<SwapBuffersWithDamage>,
    preserved: bool,
    buffer_age_supported: Option<bool>,
    rect_vertices: Vec<f32>,
    glyph_vertices: Vec<f32>,
    dump_done: bool,
    dump_at: Option<std::time::Instant>,
}

impl GlBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        wayland_display: *mut c_void,
        wayland_surface: *mut c_void,
        width: i32,
        height: i32,
    ) -> Result<Self, String> {
        if wayland_display.is_null() || wayland_surface.is_null() {
            return Err("null Wayland display or surface".to_string());
        }
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(|error| format!("load libEGL: {error}"))?;
        let display = unsafe { egl.get_display(wayland_display) }
            .ok_or_else(|| "eglGetDisplay failed".to_string())?;
        egl.initialize(display)
            .map_err(|error| format!("eglInitialize: {error}"))?;
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|error| format!("eglBindAPI: {error}"))?;
        let attributes = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attributes)
            .map_err(|error| format!("eglChooseConfig: {error}"))?
            .ok_or_else(|| "no EGL window config with OpenGL ES2".to_string())?;
        let context_attributes = [
            egl::CONTEXT_MAJOR_VERSION,
            2,
            egl::CONTEXT_MINOR_VERSION,
            0,
            egl::NONE,
        ];
        let context = egl
            .create_context(display, config, None, &context_attributes)
            .map_err(|error| format!("eglCreateContext: {error}"))?;
        let window_api = EglWindowApi::load()?;
        let window = unsafe { (window_api.create)(wayland_surface, width, height) };
        if window.is_null() {
            let _ = egl.destroy_context(display, context);
            return Err("wl_egl_window_create failed".to_string());
        }
        let surface =
            match unsafe { egl.create_window_surface(display, config, window, Some(&[egl::NONE])) }
            {
                Ok(surface) => surface,
                Err(error) => {
                    unsafe { (window_api.destroy)(window) };
                    let _ = egl.destroy_context(display, context);
                    return Err(format!("eglCreateWindowSurface: {error}"));
                }
            };
        if let Err(error) = egl.make_current(display, Some(surface), Some(surface), Some(context)) {
            let _ = egl.destroy_surface(display, surface);
            unsafe { (window_api.destroy)(window) };
            let _ = egl.destroy_context(display, context);
            return Err(format!("eglMakeCurrent: {error}"));
        }

        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|function| function as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };
        let (rect_program, rect_pos, rect_color, rect_size) =
            compile_rect_program(&gl).map_err(|error| format!("rect shader: {error}"))?;
        let (glyph_program, glyph_pos, glyph_uv, glyph_color, glyph_size) =
            compile_glyph_program(&gl).map_err(|error| format!("glyph shader: {error}"))?;

        let atlas_texture =
            unsafe { gl.create_texture() }.map_err(|error| format!("glGenTextures: {error}"))?;
        let atlas = GlyphAtlas::new();
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas_texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::ALPHA as i32,
                ATLAS_SIZE as i32,
                ATLAS_SIZE as i32,
                0,
                glow::ALPHA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&atlas.data)),
            );
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.disable(glow::DEPTH_TEST);
            gl.viewport(0, 0, width, height);
        }

        let rect_vbo =
            unsafe { gl.create_buffer() }.map_err(|error| format!("glGenBuffers: {error}"))?;
        let glyph_vbo =
            unsafe { gl.create_buffer() }.map_err(|error| format!("glGenBuffers: {error}"))?;

        let swap_with_damage =
            egl.get_proc_address("eglSwapBuffersWithDamageKHR")
                .map(|function| unsafe {
                    std::mem::transmute::<extern "system" fn(), SwapBuffersWithDamage>(function)
                });
        let preserved = egl
            .query_surface(display, surface, EGL_SWAP_BEHAVIOR)
            .map(|value| value == EGL_BUFFER_PRESERVED)
            .unwrap_or(false);
        let buffer_age_supported = egl
            .query_surface(display, surface, EGL_BUFFER_AGE_EXT)
            .ok()
            .map(|_| true);

        Ok(Self {
            egl,
            display,
            surface,
            context,
            config,
            wayland_surface,
            window,
            window_api,
            gl,
            rect_program,
            glyph_program,
            rect_vbo,
            glyph_vbo,
            atlas_texture,
            rect_pos,
            rect_color,
            glyph_pos,
            glyph_uv,
            glyph_color,
            rect_size,
            glyph_size,
            atlas,
            width,
            height,
            swap_with_damage,
            preserved,
            buffer_age_supported,
            rect_vertices: Vec::new(),
            glyph_vertices: Vec::new(),
            dump_done: false,
            dump_at: crate::compat::env("BEYRIX_GL_DUMP")
                .map(|_| std::time::Instant::now() + std::time::Duration::from_millis(1500)),
        })
    }

    pub fn renderer_string(&self) -> String {
        unsafe {
            format!(
                "{} ({})",
                self.gl.get_parameter_string(glow::RENDERER),
                self.gl.get_parameter_string(glow::VERSION)
            )
        }
    }

    pub fn can_partial(&self) -> bool {
        self.swap_with_damage.is_some()
            && (self.preserved || self.buffer_age_supported == Some(true))
    }

    pub fn buffer_age(&self) -> Option<i32> {
        self.egl
            .query_surface(self.display, self.surface, EGL_BUFFER_AGE_EXT)
            .ok()
    }

    pub fn resize(&mut self, width: i32, height: i32) -> Result<(), String> {
        let width = width.max(1);
        let height = height.max(1);
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        // Mesa sometimes keeps the EGL window's original size after
        // `wl_egl_window_resize` when the surface was created before the first
        // configure. Recreating the native window/surface is deterministic and only
        // happens on interactive resize.
        self.egl
            .make_current(self.display, None, None, None)
            .map_err(|error| format!("eglMakeCurrent(None): {error}"))?;
        let _ = self.egl.destroy_surface(self.display, self.surface);
        unsafe { (self.window_api.destroy)(self.window) };
        let window = unsafe { (self.window_api.create)(self.wayland_surface, width, height) };
        if window.is_null() {
            return Err("wl_egl_window_create failed during resize".to_string());
        }
        let surface = unsafe {
            self.egl
                .create_window_surface(self.display, self.config, window, Some(&[egl::NONE]))
        }
        .map_err(|error| format!("eglCreateWindowSurface during resize: {error}"))?;
        self.egl
            .make_current(
                self.display,
                Some(surface),
                Some(surface),
                Some(self.context),
            )
            .map_err(|error| format!("eglMakeCurrent during resize: {error}"))?;
        self.window = window;
        self.surface = surface;
        self.width = width;
        self.height = height;
        unsafe { self.gl.viewport(0, 0, width, height) };
        if crate::compat::env("BEYRIX_DEBUG_INPUT").is_some() {
            let egl_width = self
                .egl
                .query_surface(self.display, self.surface, egl::WIDTH)
                .unwrap_or(-1);
            let egl_height = self
                .egl
                .query_surface(self.display, self.surface, egl::HEIGHT)
                .unwrap_or(-1);
            let mut attached_w = 0;
            let mut attached_h = 0;
            unsafe { (self.window_api.attached)(self.window, &mut attached_w, &mut attached_h) };
            eprintln!(
                "[gl] wl_egl_window_resize {width}x{height}, egl now {egl_width}x{egl_height}, attached {attached_w}x{attached_h}"
            );
        }
        Ok(())
    }

    pub fn invalidate_atlas(&mut self) {
        self.atlas.clear();
    }

    /// Present one damaged row range. `scene` must contain all cells for the
    /// damaged rows; `damage` is in physical pixels and only used as a hint to
    /// the compositor.
    pub fn draw(&mut self, scene: &Scene, damage: &[PixelRect], full: bool) -> Result<(), String> {
        self.egl
            .make_current(
                self.display,
                Some(self.surface),
                Some(self.surface),
                Some(self.context),
            )
            .map_err(|error| format!("eglMakeCurrent: {error}"))?;
        if self.atlas.reset {
            self.upload_atlas_full();
            self.atlas.reset = false;
        }
        self.upload_pending();
        self.build_rects(scene);
        self.build_glyphs(scene);

        unsafe {
            self.gl.viewport(0, 0, self.width, self.height);
        }

        let first = damage.first().copied().unwrap_or(PixelRect {
            x: 0,
            y: 0,
            w: self.width,
            h: self.height,
        });
        if full {
            unsafe {
                self.gl.disable(glow::SCISSOR_TEST);
                let [r, g, b, a] = scene.background;
                self.gl.clear_color(r, g, b, a);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
        } else {
            let rect = first;
            unsafe {
                self.gl.enable(glow::SCISSOR_TEST);
                self.gl
                    .scissor(rect.x, self.height - rect.y - rect.h, rect.w, rect.h);
                // A damaged row must not retain stale selection/cursor/text
                // pixels when cells become blank again.
                let [r, g, b, a] = scene.background;
                self.gl.clear_color(r, g, b, a);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
        }

        unsafe {
            if !self.rect_vertices.is_empty() {
                self.gl.use_program(Some(self.rect_program));
                if let Some(location) = &self.rect_size {
                    self.gl
                        .uniform_2_f32(Some(location), self.width as f32, self.height as f32);
                }
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.rect_vbo));
                self.gl.buffer_data_u8_slice(
                    glow::ARRAY_BUFFER,
                    as_bytes(&self.rect_vertices),
                    glow::STREAM_DRAW,
                );
                self.gl.enable_vertex_attrib_array(self.rect_pos);
                self.gl
                    .vertex_attrib_pointer_f32(self.rect_pos, 2, glow::FLOAT, false, 6 * 4, 0);
                self.gl.enable_vertex_attrib_array(self.rect_color);
                self.gl.vertex_attrib_pointer_f32(
                    self.rect_color,
                    4,
                    glow::FLOAT,
                    false,
                    6 * 4,
                    2 * 4,
                );
                self.gl
                    .draw_arrays(glow::TRIANGLES, 0, (self.rect_vertices.len() / 6) as i32);
            }

            if !self.glyph_vertices.is_empty() {
                self.gl.use_program(Some(self.glyph_program));
                if let Some(location) = &self.glyph_size {
                    self.gl
                        .uniform_2_f32(Some(location), self.width as f32, self.height as f32);
                }
                self.gl.active_texture(glow::TEXTURE0);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(self.atlas_texture));
                self.gl
                    .bind_buffer(glow::ARRAY_BUFFER, Some(self.glyph_vbo));
                self.gl.buffer_data_u8_slice(
                    glow::ARRAY_BUFFER,
                    as_bytes(&self.glyph_vertices),
                    glow::STREAM_DRAW,
                );
                self.gl.enable_vertex_attrib_array(self.glyph_pos);
                self.gl
                    .vertex_attrib_pointer_f32(self.glyph_pos, 2, glow::FLOAT, false, 8 * 4, 0);
                self.gl.enable_vertex_attrib_array(self.glyph_uv);
                self.gl.vertex_attrib_pointer_f32(
                    self.glyph_uv,
                    2,
                    glow::FLOAT,
                    false,
                    8 * 4,
                    2 * 4,
                );
                self.gl.enable_vertex_attrib_array(self.glyph_color);
                self.gl.vertex_attrib_pointer_f32(
                    self.glyph_color,
                    4,
                    glow::FLOAT,
                    false,
                    8 * 4,
                    4 * 4,
                );
                self.gl
                    .draw_arrays(glow::TRIANGLES, 0, (self.glyph_vertices.len() / 8) as i32);
            }
            self.gl.disable(glow::SCISSOR_TEST);
        }
        Ok(())
    }

    /// Present the frame. With `full == false` the damage list is passed to the
    /// compositor through `EGL_KHR_swap_buffers_with_damage`.
    pub fn present(&mut self, damage: &[PixelRect], full: bool) -> Result<(), String> {
        self.dump_frame(full);
        if full || !self.can_partial() || damage.is_empty() {
            return self
                .egl
                .swap_buffers(self.display, self.surface)
                .map_err(|error| format!("eglSwapBuffers: {error}"));
        }
        let Some(swap) = self.swap_with_damage else {
            return self
                .egl
                .swap_buffers(self.display, self.surface)
                .map_err(|error| format!("eglSwapBuffers: {error}"));
        };
        let mut rects = Vec::with_capacity(damage.len() * 4);
        for rect in damage {
            rects.push(rect.x);
            rects.push(self.height - rect.y - rect.h);
            rects.push(rect.w);
            rects.push(rect.h);
        }
        let result = unsafe {
            swap(
                self.display.as_ptr(),
                self.surface.as_ptr(),
                rects.as_ptr(),
                damage.len() as i32,
            )
        };
        if result == 0 {
            Err("eglSwapBuffersWithDamageKHR failed".to_string())
        } else {
            Ok(())
        }
    }

    /// Developer aid: render one full frame to a PPM file. Used to verify the
    /// GPU pipeline on machines without a screenshot tool.
    fn dump_frame(&mut self, full: bool) {
        if self.dump_done || !full {
            return;
        }
        let Some(path) = crate::compat::env("BEYRIX_GL_DUMP") else {
            return;
        };
        if self
            .dump_at
            .is_some_and(|at| std::time::Instant::now() < at)
        {
            return;
        }
        let width = self.width.max(1);
        let height = self.height.max(1);
        if crate::compat::env("BEYRIX_DEBUG_INPUT").is_some() {
            let egl_width = self
                .egl
                .query_surface(self.display, self.surface, egl::WIDTH)
                .unwrap_or(-1);
            let egl_height = self
                .egl
                .query_surface(self.display, self.surface, egl::HEIGHT)
                .unwrap_or(-1);
            let mut viewport = [0i32; 4];
            unsafe {
                self.gl
                    .get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            }
            eprintln!(
                "[gl] dump size={width}x{height} egl={egl_width}x{egl_height} viewport={viewport:?}"
            );
        }
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        unsafe {
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(
                0,
                0,
                width,
                height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        let mut ppm = Vec::with_capacity(pixels.len() + 32);
        ppm.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
        for row in (0..height as usize).rev() {
            for column in 0..width as usize {
                let offset = (row * width as usize + column) * 4;
                ppm.extend_from_slice(&pixels[offset..offset + 3]);
            }
        }
        if let Err(error) = std::fs::write(&path, ppm) {
            eprintln!("beyrix: cannot write {}: {error}", path.to_string_lossy());
        }
        if crate::compat::env("BEYRIX_DEBUG_INPUT").is_some() {
            eprintln!("[gl] dumped {width}x{height} to {}", path.to_string_lossy());
        }
        self.dump_done = true;
    }

    fn upload_atlas_full(&self) {
        unsafe {
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.atlas_texture));
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                ATLAS_SIZE as i32,
                ATLAS_SIZE as i32,
                glow::ALPHA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&self.atlas.data)),
            );
        }
    }

    fn upload_pending(&mut self) {
        if self.atlas.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.atlas.pending);
        unsafe {
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.atlas_texture));
            for slot in pending {
                if slot.w == 0 || slot.h == 0 {
                    continue;
                }
                let mut pixels = Vec::with_capacity((slot.w * slot.h) as usize);
                for row in 0..slot.h {
                    let start = ((slot.y + row) * ATLAS_SIZE + slot.x) as usize;
                    pixels.extend_from_slice(&self.atlas.data[start..start + slot.w as usize]);
                }
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    slot.x as i32,
                    slot.y as i32,
                    slot.w as i32,
                    slot.h as i32,
                    glow::ALPHA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pixels)),
                );
            }
        }
    }

    fn build_rects(&mut self, scene: &Scene) {
        self.rect_vertices.clear();
        self.rect_vertices.reserve(scene.rects.len() * 6 * 6);
        for rect in &scene.rects {
            let x0 = rect.x;
            let y0 = rect.y;
            let x1 = rect.x + rect.w;
            let y1 = rect.y + rect.h;
            let [r, g, b, a] = rect.color;
            for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y0), (x1, y1), (x0, y1)] {
                self.rect_vertices.extend_from_slice(&[x, y, r, g, b, a]);
            }
        }
    }

    fn build_glyphs(&mut self, scene: &Scene) {
        self.glyph_vertices.clear();
        self.glyph_vertices.reserve(scene.glyphs.len() * 6 * 8);
        for glyph in &scene.glyphs {
            let Some((u0, v0, u1, v1)) = atlas_uv(&mut self.atlas, glyph) else {
                continue;
            };
            let x0 = glyph.x;
            let y0 = glyph.y;
            let x1 = glyph.x + glyph.w;
            let y1 = glyph.y + glyph.h;
            let [r, g, b, a] = glyph.color;
            let corners = [
                (x0, y0, u0, v0),
                (x1, y0, u1, v0),
                (x1, y1, u1, v1),
                (x0, y0, u0, v0),
                (x1, y1, u1, v1),
                (x0, y1, u0, v1),
            ];
            for (x, y, u, v) in corners {
                self.glyph_vertices
                    .extend_from_slice(&[x, y, u, v, r, g, b, a]);
            }
        }
    }

    /// Resolve atlas slots for a scene before building vertex data. Kept
    /// separate from `build_glyphs` so the atlas can borrow the renderer.
    pub fn prepare_scene(&mut self, renderer: &mut Renderer, scene: &Scene) {
        for glyph in &scene.glyphs {
            self.atlas.slot(glyph.ch, glyph.px_size, renderer);
        }
        // `build_glyphs` runs after this and only reads the finished slots.
    }
}

impl Drop for GlBackend {
    fn drop(&mut self) {
        unsafe {
            let _ = self.egl.make_current(self.display, None, None, None);
            self.gl.delete_program(self.rect_program);
            self.gl.delete_program(self.glyph_program);
            self.gl.delete_buffer(self.rect_vbo);
            self.gl.delete_buffer(self.glyph_vbo);
            self.gl.delete_texture(self.atlas_texture);
            let _ = self.egl.destroy_surface(self.display, self.surface);
            let _ = self.egl.destroy_context(self.display, self.context);
            (self.window_api.destroy)(self.window);
            let _ = self.egl.terminate(self.display);
        }
    }
}

fn atlas_uv(atlas: &mut GlyphAtlas, glyph: &SceneGlyph) -> Option<(f32, f32, f32, f32)> {
    let key = (glyph.ch, (glyph.px_size * 100.0).round().max(1.0) as u32);
    let slot = *atlas.map.get(&key)?;
    if slot.w == 0 || slot.h == 0 {
        return None;
    }
    let [u0, v0, u1, v1] = glyph.uv;
    let inv = 1.0 / ATLAS_SIZE as f32;
    Some((
        (slot.x as f32 + u0 * slot.w as f32) * inv,
        (slot.y as f32 + v0 * slot.h as f32) * inv,
        (slot.x as f32 + u1 * slot.w as f32) * inv,
        (slot.y as f32 + v1 * slot.h as f32) * inv,
    ))
}

fn as_bytes(values: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), values.len() * 4) }
}

fn compile_program(
    gl: &glow::Context,
    vertex: &str,
    fragment: &str,
    attributes: &[(u32, &str)],
) -> Result<glow::Program, String> {
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, source) in [
            (glow::VERTEX_SHADER, vertex),
            (glow::FRAGMENT_SHADER, fragment),
        ] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                gl.delete_program(program);
                return Err(format!("compile: {log}"));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        for (index, name) in attributes {
            gl.bind_attrib_location(program, *index, name);
        }
        gl.link_program(program);
        let linked = gl.get_program_link_status(program);
        if !linked {
            let log = gl.get_program_info_log(program);
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.delete_program(program);
            return Err(format!("link: {log}"));
        }
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        Ok(program)
    }
}

type RectProgram = (glow::Program, u32, u32, Option<glow::UniformLocation>);

fn compile_rect_program(gl: &glow::Context) -> Result<RectProgram, String> {
    const VERTEX: &str = r#"
attribute vec2 a_pos;
attribute vec4 a_color;
uniform vec2 u_size;
varying vec4 v_color;
void main() {
    vec2 p = a_pos / u_size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, 0.0, 1.0);
    v_color = a_color;
}
"#;
    const FRAGMENT: &str = r#"
precision mediump float;
varying vec4 v_color;
void main() {
    gl_FragColor = v_color;
}
"#;
    let program = compile_program(gl, VERTEX, FRAGMENT, &[(0, "a_pos"), (1, "a_color")])?;
    let size = unsafe { gl.get_uniform_location(program, "u_size") };
    Ok((program, 0, 1, size))
}

type GlyphProgram = (glow::Program, u32, u32, u32, Option<glow::UniformLocation>);

fn compile_glyph_program(gl: &glow::Context) -> Result<GlyphProgram, String> {
    const VERTEX: &str = r#"
attribute vec2 a_pos;
attribute vec2 a_uv;
attribute vec4 a_color;
uniform vec2 u_size;
varying vec2 v_uv;
varying vec4 v_color;
void main() {
    vec2 p = a_pos / u_size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, 0.0, 1.0);
    v_uv = a_uv;
    v_color = a_color;
}
"#;
    const FRAGMENT: &str = r#"
precision mediump float;
uniform sampler2D u_atlas;
varying vec2 v_uv;
varying vec4 v_color;
void main() {
    float alpha = texture2D(u_atlas, v_uv).a;
    gl_FragColor = vec4(v_color.rgb, v_color.a * alpha);
}
"#;
    let program = compile_program(
        gl,
        VERTEX,
        FRAGMENT,
        &[(0, "a_pos"), (1, "a_uv"), (2, "a_color")],
    )?;
    let size = unsafe { gl.get_uniform_location(program, "u_size") };
    Ok((program, 0, 1, 2, size))
}
