// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal OpenGL ES 2.0 backend on top of EGL/Wayland.
//!
//! The pipeline is deliberately small and terminal-specific:
//! * one bounded alpha glyph atlas with ordered batches on overflow;
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

use crate::render::{FontKind, Renderer, Scene, SceneGlyph, SceneImage, SceneRect, SceneRoundedRect};

const ATLAS_SIZE: u32 = 1024;
const ATLAS_ENTRIES: usize = 8192;
const GLYPHS_PER_BATCH: usize = 4096;
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

#[derive(Default)]
struct GlyphAtlas {
    map: HashMap<(char, u32, FontKind), Slot>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
}

impl GlyphAtlas {
    fn clear(&mut self) {
        self.map.clear();
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_h = 0;
    }

    fn insert(&mut self, key: (char, u32, FontKind), w: u32, h: u32) -> Option<Slot> {
        if !(1..=ATLAS_SIZE).contains(&w) || !(1..=ATLAS_SIZE).contains(&h) {
            return None;
        }
        let (x, y, shelf_h) = if self.shelf_x + w > ATLAS_SIZE {
            (0, self.shelf_y + self.shelf_h, 0)
        } else {
            (self.shelf_x, self.shelf_y, self.shelf_h)
        };
        if y + h > ATLAS_SIZE || self.map.len() >= ATLAS_ENTRIES {
            return None;
        }
        let slot = Slot { x, y, w, h };
        self.shelf_x = x + w;
        self.shelf_y = y;
        self.shelf_h = shelf_h.max(h);
        self.map.insert(key, slot);
        Some(slot)
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
    rounded_program: glow::Program,
    glyph_program: glow::Program,
    image_program: glow::Program,
    rect_vbo: glow::Buffer,
    rounded_vbo: glow::Buffer,
    glyph_vbo: glow::Buffer,
    image_vbo: glow::Buffer,
    atlas_texture: glow::Texture,
    image_textures: HashMap<u64, (u32, u32, glow::Texture)>,
    rect_pos: u32,
    rect_color: u32,
    glyph_pos: u32,
    glyph_uv: u32,
    glyph_color: u32,
    rect_size: Option<glow::UniformLocation>,
    rounded_size: Option<glow::UniformLocation>,
    glyph_size: Option<glow::UniformLocation>,
    image_size: Option<glow::UniformLocation>,
    image_params: Option<glow::UniformLocation>,
    image_clip: Option<glow::UniformLocation>,
    atlas: GlyphAtlas,
    width: i32,
    height: i32,
    swap_with_damage: Option<SwapBuffersWithDamage>,
    preserved: bool,
    buffer_age_supported: Option<bool>,
    rect_vertices: Vec<f32>,
    rounded_vertices: Vec<f32>,
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
        let (rounded_program, rounded_size) =
            compile_rounded_program(&gl).map_err(|error| format!("rounded shader: {error}"))?;
        let (glyph_program, glyph_pos, glyph_uv, glyph_color, glyph_size) =
            compile_glyph_program(&gl).map_err(|error| format!("glyph shader: {error}"))?;
        let (image_program, image_size, image_params, image_clip) =
            compile_image_program(&gl).map_err(|error| format!("image shader: {error}"))?;

        let atlas_texture =
            unsafe { gl.create_texture() }.map_err(|error| format!("glGenTextures: {error}"))?;
        let atlas = GlyphAtlas::default();
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
                glow::PixelUnpackData::Slice(None),
            );
            gl.enable(glow::BLEND);
            // Keep the EGL buffer opaque when compositing grayscale masks
            // over the opaque background. Color still uses straight alpha.
            gl.blend_func_separate(
                glow::SRC_ALPHA,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
            );
            gl.disable(glow::DEPTH_TEST);
            gl.viewport(0, 0, width, height);
        }

        let rect_vbo =
            unsafe { gl.create_buffer() }.map_err(|error| format!("glGenBuffers: {error}"))?;
        let rounded_vbo =
            unsafe { gl.create_buffer() }.map_err(|error| format!("glGenBuffers: {error}"))?;
        let glyph_vbo =
            unsafe { gl.create_buffer() }.map_err(|error| format!("glGenBuffers: {error}"))?;
        let image_vbo =
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
            rounded_program,
            glyph_program,
            image_program,
            rect_vbo,
            rounded_vbo,
            glyph_vbo,
            image_vbo,
            atlas_texture,
            image_textures: HashMap::new(),
            rect_pos,
            rect_color,
            glyph_pos,
            glyph_uv,
            glyph_color,
            rect_size,
            rounded_size,
            glyph_size,
            image_size,
            image_params,
            image_clip,
            atlas,
            width,
            height,
            swap_with_damage,
            preserved,
            buffer_age_supported,
            rect_vertices: Vec::new(),
            rounded_vertices: Vec::new(),
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
        self.window = std::ptr::null_mut();
        let window = unsafe { (self.window_api.create)(self.wayland_surface, width, height) };
        if window.is_null() {
            return Err("wl_egl_window_create failed during resize".to_string());
        }
        let surface = match unsafe {
            self.egl
                .create_window_surface(self.display, self.config, window, Some(&[egl::NONE]))
        } {
            Ok(surface) => surface,
            Err(error) => {
                unsafe { (self.window_api.destroy)(window) };
                return Err(format!("eglCreateWindowSurface during resize: {error}"));
            }
        };
        if let Err(error) = self.egl.make_current(
            self.display,
            Some(surface),
            Some(surface),
            Some(self.context),
        ) {
            let _ = self.egl.destroy_surface(self.display, surface);
            unsafe { (self.window_api.destroy)(window) };
            return Err(format!("eglMakeCurrent during resize: {error}"));
        }
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
    pub fn draw(
        &mut self,
        renderer: &mut Renderer,
        scene: &Scene,
        damage: &[PixelRect],
        full: bool,
    ) -> Result<(), String> {
        self.egl
            .make_current(
                self.display,
                Some(self.surface),
                Some(self.surface),
                Some(self.context),
            )
            .map_err(|error| format!("eglMakeCurrent: {error}"))?;

        unsafe {
            self.gl.viewport(0, 0, self.width, self.height);
        }

        unsafe {
            if full {
                self.gl.disable(glow::SCISSOR_TEST);
            } else {
                let rect = damage[0];
                self.gl.enable(glow::SCISSOR_TEST);
                self.gl
                    .scissor(rect.x, self.height - rect.y - rect.h, rect.w, rect.h);
            }
            let [r, g, b, a] = scene.background;
            self.gl.clear_color(r, g, b, a);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }

        if !scene.behind_rects.is_empty() {
            self.draw_rounded_rects(&scene.behind_rects);
        }
        if !scene.behind_images.is_empty() {
            self.draw_images(&scene.behind_images);
        }
        if !scene.behind_glyphs.is_empty() {
            let _ = self.draw_glyph_slice(renderer, &scene.behind_glyphs);
        }
        self.draw_rects(&scene.rects);
        let mut result = self.draw_glyphs(renderer, scene);
        if result.is_ok() {
            self.draw_rects(&scene.overlays);
            if !scene.overlay_rounded.is_empty() {
                self.draw_rounded_rects(&scene.overlay_rounded);
            }
            if !scene.overlay_images.is_empty() {
                self.draw_images(&scene.overlay_images);
            }
            if !scene.overlay_glyphs.is_empty() {
                result = self.draw_glyph_slice(renderer, &scene.overlay_glyphs);
            }
            if result.is_ok() {
                if !scene.panel_rounded.is_empty() {
                    self.draw_rounded_rects(&scene.panel_rounded);
                }
                if !scene.panel_images.is_empty() {
                    self.draw_images(&scene.panel_images);
                }
                if !scene.panel_glyphs.is_empty() {
                    result = self.draw_glyph_slice(renderer, &scene.panel_glyphs);
                }
            }
        }
        unsafe { self.gl.disable(glow::SCISSOR_TEST) };
        result?;
        let error = unsafe { self.gl.get_error() };
        if error != glow::NO_ERROR {
            return Err(format!("OpenGL drawing error 0x{error:x}"));
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

    fn draw_rects(&mut self, rects: &[SceneRect]) {
        if rects.is_empty() {
            return;
        }
        self.rect_vertices.clear();
        self.rect_vertices.reserve(rects.len() * 6 * 6);
        for rect in rects {
            let x0 = rect.x;
            let y0 = rect.y;
            let x1 = rect.x + rect.w;
            let y1 = rect.y + rect.h;
            let [r, g, b, a] = rect.color;
            for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y0), (x1, y1), (x0, y1)] {
                self.rect_vertices.extend_from_slice(&[x, y, r, g, b, a]);
            }
        }
        unsafe {
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
            self.gl.disable_vertex_attrib_array(self.glyph_color);
            self.gl.enable_vertex_attrib_array(self.rect_pos);
            self.gl
                .vertex_attrib_pointer_f32(self.rect_pos, 2, glow::FLOAT, false, 6 * 4, 0);
            self.gl.enable_vertex_attrib_array(self.rect_color);
            self.gl
                .vertex_attrib_pointer_f32(self.rect_color, 4, glow::FLOAT, false, 6 * 4, 2 * 4);
            self.gl
                .draw_arrays(glow::TRIANGLES, 0, (self.rect_vertices.len() / 6) as i32);
        }
    }

    fn draw_rounded_rects(&mut self, rects: &[SceneRoundedRect]) {
        if rects.is_empty() {
            return;
        }
        // Each vertex has 22 floats:
        // pos(2) + local(2) + half(2) + params(4) + fill(4) + border(4) + clip(4)
        const FLOATS_PER_VERTEX: usize = 22;
        self.rounded_vertices.clear();
        self.rounded_vertices
            .reserve(rects.len() * 12 * FLOATS_PER_VERTEX);

        let push_quad = |verts: &mut Vec<f32>,
                         qx0: f32,
                         qy0: f32,
                         qx1: f32,
                         qy1: f32,
                         cx: f32,
                         cy: f32,
                         hw: f32,
                         hh: f32,
                         params: [f32; 4],
                         fill: [f32; 4],
                         border: [f32; 4],
                         clip: [f32; 4]| {
            for (px, py) in [
                (qx0, qy0),
                (qx1, qy0),
                (qx1, qy1),
                (qx0, qy0),
                (qx1, qy1),
                (qx0, qy1),
            ] {
                verts.extend_from_slice(&[
                    px,
                    py,
                    px - cx,
                    py - cy,
                    hw,
                    hh,
                    params[0],
                    params[1],
                    params[2],
                    params[3],
                    fill[0],
                    fill[1],
                    fill[2],
                    fill[3],
                    border[0],
                    border[1],
                    border[2],
                    border[3],
                    clip[0],
                    clip[1],
                    clip[2],
                    clip[3],
                ]);
            }
        };

        for r in rects {
            if r.w <= 0.0 || r.h <= 0.0 {
                continue;
            }
            let hw = r.w * 0.5;
            let hh = r.h * 0.5;
            let cx = r.x + hw;
            let cy = r.y + hh;
            let rad = r.radius.min(hw).min(hh).max(0.0);
            let clip = match r.clip {
                Some([x, y, w, h]) => [x, y, x + w, y + h],
                None => [-1e6, -1e6, 1e6, 1e6],
            };

            // Optional soft drop shadow emitted directly behind the rect.
            if r.shadow_blur > 0.5 && r.shadow_color[3] > 0.002 {
                let b = r.shadow_blur;
                push_quad(
                    &mut self.rounded_vertices,
                    r.x - b,
                    r.y + r.shadow_offset_y - b,
                    r.x + r.w + b,
                    r.y + r.h + r.shadow_offset_y + b,
                    cx,
                    cy + r.shadow_offset_y,
                    hw,
                    hh,
                    [rad, 0.0, b, 0.0],
                    r.shadow_color,
                    [0.0; 4],
                    clip,
                );
            }

            if r.color[3] > 0.0 || (r.border_width > 0.0 && r.border_color[3] > 0.0) {
                push_quad(
                    &mut self.rounded_vertices,
                    r.x - 1.0,
                    r.y - 1.0,
                    r.x + r.w + 1.0,
                    r.y + r.h + 1.0,
                    cx,
                    cy,
                    hw,
                    hh,
                    [rad, r.border_width.max(0.0), 0.0, 0.0],
                    r.color,
                    r.border_color,
                    clip,
                );
            }
        }

        if self.rounded_vertices.is_empty() {
            return;
        }
        let stride = (FLOATS_PER_VERTEX * 4) as i32;
        unsafe {
            self.gl.use_program(Some(self.rounded_program));
            if let Some(location) = &self.rounded_size {
                self.gl
                    .uniform_2_f32(Some(location), self.width as f32, self.height as f32);
            }
            self.gl
                .bind_buffer(glow::ARRAY_BUFFER, Some(self.rounded_vbo));
            self.gl.buffer_data_u8_slice(
                glow::ARRAY_BUFFER,
                as_bytes(&self.rounded_vertices),
                glow::STREAM_DRAW,
            );
            for loc in 0..7 {
                self.gl.enable_vertex_attrib_array(loc);
            }
            self.gl
                .vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            self.gl
                .vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 2 * 4);
            self.gl
                .vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, 4 * 4);
            self.gl
                .vertex_attrib_pointer_f32(3, 4, glow::FLOAT, false, stride, 6 * 4);
            self.gl
                .vertex_attrib_pointer_f32(4, 4, glow::FLOAT, false, stride, 10 * 4);
            self.gl
                .vertex_attrib_pointer_f32(5, 4, glow::FLOAT, false, stride, 14 * 4);
            self.gl
                .vertex_attrib_pointer_f32(6, 4, glow::FLOAT, false, stride, 18 * 4);
            self.gl.draw_arrays(
                glow::TRIANGLES,
                0,
                (self.rounded_vertices.len() / FLOATS_PER_VERTEX) as i32,
            );
            for loc in 2..7 {
                self.gl.disable_vertex_attrib_array(loc);
            }
        }
    }

    fn draw_images(&mut self, images: &[SceneImage]) {
        if images.is_empty() {
            return;
        }
        unsafe {
            self.gl.use_program(Some(self.image_program));
            if let Some(location) = &self.image_size {
                self.gl
                    .uniform_2_f32(Some(location), self.width as f32, self.height as f32);
            }
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_buffer(glow::ARRAY_BUFFER, Some(self.image_vbo));
            self.gl.enable_vertex_attrib_array(0);
            self.gl.enable_vertex_attrib_array(1);
            self.gl.disable_vertex_attrib_array(2);
            self.gl
                .vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 4 * 4, 0);
            self.gl
                .vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 4 * 4, 2 * 4);
        }
        if self.image_textures.len() > 256 {
            for (_, (_, _, tex)) in self.image_textures.drain() {
                unsafe { self.gl.delete_texture(tex) };
            }
        }
        for img in images {
            if img.src_w == 0 || img.src_h == 0 || img.w <= 0.0 || img.h <= 0.0 {
                continue;
            }
            let expected_len = (img.src_w as usize) * (img.src_h as usize) * 4;
            if img.rgba.len() < expected_len {
                continue;
            }
            let tex = match self.image_textures.get(&img.id) {
                Some(&(w, h, tex)) if w == img.src_w && h == img.src_h => tex,
                _ => {
                    if let Some((_, _, old)) = self.image_textures.remove(&img.id) {
                        unsafe { self.gl.delete_texture(old) };
                    }
                    let Ok(tex) = (unsafe { self.gl.create_texture() }) else {
                        continue;
                    };
                    unsafe {
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_MIN_FILTER,
                            glow::LINEAR as i32,
                        );
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_MAG_FILTER,
                            glow::LINEAR as i32,
                        );
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_WRAP_S,
                            glow::CLAMP_TO_EDGE as i32,
                        );
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_WRAP_T,
                            glow::CLAMP_TO_EDGE as i32,
                        );
                        self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                        self.gl.tex_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            glow::RGBA as i32,
                            img.src_w as i32,
                            img.src_h as i32,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(&img.rgba[..expected_len])),
                        );
                    }
                    self.image_textures.insert(img.id, (img.src_w, img.src_h, tex));
                    tex
                }
            };
            let x0 = img.x;
            let y0 = img.y;
            let x1 = img.x + img.w;
            let y1 = img.y + img.h;
            let verts: [f32; 24] = [
                x0, y0, 0.0, 0.0,
                x1, y0, 1.0, 0.0,
                x1, y1, 1.0, 1.0,
                x0, y0, 0.0, 0.0,
                x1, y1, 1.0, 1.0,
                x0, y1, 0.0, 1.0,
            ];
            let clip = match img.clip {
                Some([x, y, w, h]) => [x, y, x + w, y + h],
                None => [-1e6, -1e6, 1e6, 1e6],
            };
            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                if let Some(loc) = &self.image_params {
                    self.gl.uniform_4_f32(
                        Some(loc),
                        img.w,
                        img.h,
                        img.radius.min(img.w * 0.5).min(img.h * 0.5).max(0.0),
                        img.alpha.clamp(0.0, 1.0),
                    );
                }
                if let Some(loc) = &self.image_clip {
                    self.gl
                        .uniform_4_f32(Some(loc), clip[0], clip[1], clip[2], clip[3]);
                }
                self.gl.buffer_data_u8_slice(
                    glow::ARRAY_BUFFER,
                    as_bytes(&verts),
                    glow::STREAM_DRAW,
                );
                self.gl.draw_arrays(glow::TRIANGLES, 0, 6);
            }
        }
    }

    fn draw_glyphs(&mut self, renderer: &mut Renderer, scene: &Scene) -> Result<(), String> {
        self.draw_glyph_slice(renderer, &scene.glyphs)
    }

    fn draw_glyph_slice(
        &mut self,
        renderer: &mut Renderer,
        glyphs: &[SceneGlyph],
    ) -> Result<(), String> {
        if glyphs.is_empty() {
            return Ok(());
        }
        unsafe {
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
            self.gl.enable_vertex_attrib_array(self.glyph_pos);
            self.gl
                .vertex_attrib_pointer_f32(self.glyph_pos, 2, glow::FLOAT, false, 8 * 4, 0);
            self.gl.enable_vertex_attrib_array(self.glyph_uv);
            self.gl
                .vertex_attrib_pointer_f32(self.glyph_uv, 2, glow::FLOAT, false, 8 * 4, 2 * 4);
            self.gl.enable_vertex_attrib_array(self.glyph_color);
            self.gl.vertex_attrib_pointer_f32(
                self.glyph_color,
                4,
                glow::FLOAT,
                false,
                8 * 4,
                4 * 4,
            );
        }
        self.glyph_vertices.clear();
        self.glyph_vertices
            .reserve(glyphs.len().min(GLYPHS_PER_BATCH) * 6 * 8);
        for glyph in glyphs {
            let key = (glyph.ch, glyph.px_size.to_bits(), glyph.font_kind);
            let slot = match self.atlas.map.get(&key) {
                Some(&slot) => slot,
                None => {
                    let (metrics, bitmap) =
                        renderer.glyph_bitmap_kind(glyph.ch, glyph.px_size, glyph.font_kind);
                    let (w, h) = (metrics.width as u32, metrics.height as u32);
                    let slot = if let Some(slot) = self.atlas.insert(key, w, h) {
                        slot
                    } else {
                        // GL executes draws and texture updates in command order.
                        // Submit the vertices that refer to this page before reusing it.
                        self.submit_glyphs();
                        self.atlas.clear();
                        self.atlas
                            .insert(key, w, h)
                            .ok_or("glyph exceeds atlas dimensions")?
                    };
                    unsafe {
                        self.gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            slot.x as i32,
                            slot.y as i32,
                            slot.w as i32,
                            slot.h as i32,
                            glow::ALPHA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(bitmap)),
                        );
                    }
                    slot
                }
            };
            append_glyph(&mut self.glyph_vertices, glyph, slot);
            if self.glyph_vertices.len() == GLYPHS_PER_BATCH * 6 * 8 {
                self.submit_glyphs();
            }
        }
        self.submit_glyphs();
        Ok(())
    }

    fn submit_glyphs(&mut self) {
        if self.glyph_vertices.is_empty() {
            return;
        }
        unsafe {
            self.gl.buffer_data_u8_slice(
                glow::ARRAY_BUFFER,
                as_bytes(&self.glyph_vertices),
                glow::STREAM_DRAW,
            );
            self.gl
                .draw_arrays(glow::TRIANGLES, 0, (self.glyph_vertices.len() / 8) as i32);
        }
        self.glyph_vertices.clear();
    }
}

impl Drop for GlBackend {
    fn drop(&mut self) {
        unsafe {
            let _ = self.egl.make_current(self.display, None, None, None);
            if !self.window.is_null() {
                let _ = self.egl.destroy_surface(self.display, self.surface);
                (self.window_api.destroy)(self.window);
            }
            // Destroying the context also frees its GL objects.
            let _ = self.egl.destroy_context(self.display, self.context);
            let _ = self.egl.terminate(self.display);
        }
    }
}

fn append_glyph(vertices: &mut Vec<f32>, glyph: &SceneGlyph, slot: Slot) {
    let (sx, sy) = glyph.source;
    debug_assert!(sx + glyph.w <= slot.w as usize && sy + glyph.h <= slot.h as usize);
    let inv = 1.0 / ATLAS_SIZE as f32;
    let u0 = (slot.x as usize + sx) as f32 * inv;
    let v0 = (slot.y as usize + sy) as f32 * inv;
    let u1 = u0 + glyph.w as f32 * inv;
    let v1 = v0 + glyph.h as f32 * inv;
    let x0 = glyph.x as f32;
    let y0 = glyph.y as f32;
    let x1 = (glyph.x + glyph.w) as f32;
    let y1 = (glyph.y + glyph.h) as f32;
    let [r, g, b, a] = glyph.color;
    for (x, y, u, v) in [
        (x0, y0, u0, v0),
        (x1, y0, u1, v0),
        (x1, y1, u1, v1),
        (x0, y0, u0, v0),
        (x1, y1, u1, v1),
        (x0, y1, u0, v1),
    ] {
        vertices.extend_from_slice(&[x, y, u, v, r, g, b, a]);
    }
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
#ifdef GL_FRAGMENT_PRECISION_HIGH
varying highp vec2 v_uv;
#else
varying mediump vec2 v_uv;
#endif
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
#ifdef GL_FRAGMENT_PRECISION_HIGH
varying highp vec2 v_uv;
#else
varying mediump vec2 v_uv;
#endif
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

fn compile_rounded_program(
    gl: &glow::Context,
) -> Result<(glow::Program, Option<glow::UniformLocation>), String> {
    const VERTEX: &str = r#"
attribute vec2 a_pos;
attribute vec2 a_local;
attribute vec2 a_half;
attribute vec4 a_params;
attribute vec4 a_fill;
attribute vec4 a_border;
attribute vec4 a_clip;
uniform vec2 u_size;
varying vec2 v_screen;
varying vec2 v_local;
varying vec2 v_half;
varying vec4 v_params;
varying vec4 v_fill;
varying vec4 v_border;
varying vec4 v_clip;
void main() {
    vec2 p = a_pos / u_size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, 0.0, 1.0);
    v_screen = a_pos;
    v_local = a_local;
    v_half = a_half;
    v_params = a_params;
    v_fill = a_fill;
    v_border = a_border;
    v_clip = a_clip;
}
"#;
    const FRAGMENT: &str = r#"
precision mediump float;
varying vec2 v_screen;
varying vec2 v_local;
varying vec2 v_half;
varying vec4 v_params;
varying vec4 v_fill;
varying vec4 v_border;
varying vec4 v_clip;
float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}
void main() {
    if (v_screen.x < v_clip.x || v_screen.y < v_clip.y || v_screen.x > v_clip.z || v_screen.y > v_clip.w) {
        discard;
    }
    float r = min(v_params.x, min(v_half.x, v_half.y));
    float bw = v_params.y;
    float blur = v_params.z;
    float d = sdRoundedBox(v_local, v_half, r);
    if (blur > 0.5) {
        float a = 1.0 - smoothstep(-blur * 0.25, blur, d);
        gl_FragColor = vec4(v_fill.rgb, v_fill.a * a);
        return;
    }
    float outer_aa = clamp(0.5 - d, 0.0, 1.0);
    if (outer_aa <= 0.002) {
        discard;
    }
    vec4 col = vec4(v_fill.rgb, v_fill.a * outer_aa);
    if (bw > 0.0 && v_border.a > 0.0) {
        float b_mask = clamp(d + bw + 0.5, 0.0, 1.0) * outer_aa;
        float b_alpha = v_border.a * b_mask;
        float out_a = b_alpha + col.a * (1.0 - b_alpha);
        vec3 out_rgb = out_a > 0.001
            ? (v_border.rgb * b_alpha + col.rgb * col.a * (1.0 - b_alpha)) / out_a
            : vec3(0.0);
        col = vec4(out_rgb, out_a);
    }
    gl_FragColor = col;
}
"#;
    let program = compile_program(
        gl,
        VERTEX,
        FRAGMENT,
        &[
            (0, "a_pos"),
            (1, "a_local"),
            (2, "a_half"),
            (3, "a_params"),
            (4, "a_fill"),
            (5, "a_border"),
            (6, "a_clip"),
        ],
    )?;
    let size = unsafe { gl.get_uniform_location(program, "u_size") };
    Ok((program, size))
}

type ImageProgram = (
    glow::Program,
    Option<glow::UniformLocation>,
    Option<glow::UniformLocation>,
    Option<glow::UniformLocation>,
);

fn compile_image_program(gl: &glow::Context) -> Result<ImageProgram, String> {
    const VERTEX: &str = r#"
attribute vec2 a_pos;
attribute vec2 a_uv;
uniform vec2 u_size;
varying vec2 v_screen;
varying vec2 v_uv;
void main() {
    vec2 p = a_pos / u_size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, 0.0, 1.0);
    v_screen = a_pos;
    v_uv = a_uv;
}
"#;
    const FRAGMENT: &str = r#"
precision mediump float;
uniform sampler2D u_tex;
uniform vec4 u_params;
uniform vec4 u_clip;
varying vec2 v_screen;
varying vec2 v_uv;
float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}
void main() {
    if (v_screen.x < u_clip.x || v_screen.y < u_clip.y || v_screen.x > u_clip.z || v_screen.y > u_clip.w) {
        discard;
    }
    vec4 texel = texture2D(u_tex, v_uv);
    float mask = 1.0;
    if (u_params.z > 0.0) {
        vec2 half_sz = u_params.xy * 0.5;
        vec2 local = (v_uv - 0.5) * u_params.xy;
        float d = sdRoundedBox(local, half_sz, u_params.z);
        mask = clamp(0.5 - d, 0.0, 1.0);
        if (mask <= 0.002) discard;
    }
    gl_FragColor = vec4(texel.rgb, texel.a * u_params.w * mask);
}
"#;
    let program = compile_program(gl, VERTEX, FRAGMENT, &[(0, "a_pos"), (1, "a_uv")])?;
    let size = unsafe { gl.get_uniform_location(program, "u_size") };
    let params = unsafe { gl.get_uniform_location(program, "u_params") };
    let clip = unsafe { gl.get_uniform_location(program, "u_clip") };
    Ok((program, size, params, clip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_glyph_preserves_atlas_slots() {
        let mut atlas = GlyphAtlas::default();
        let key = ('x', 16.0_f32.to_bits(), FontKind::Mono);
        let slot = atlas.insert(key, ATLAS_SIZE, ATLAS_SIZE).unwrap();
        assert!(atlas.insert(('界', key.1, FontKind::Mono), u32::MAX, 2).is_none());
        assert!(atlas.insert(('界', key.1, FontKind::Mono), 1, 1).is_none());
        assert_eq!(atlas.map[&key].x, slot.x);
        assert_eq!(atlas.map[&key].y, slot.y);
    }

    #[test]
    fn unicode_overflow_preserves_slots_until_each_batch_is_consumed() {
        let mut atlas = GlyphAtlas::default();
        let mut batch = Vec::new();
        let mut consumed = Vec::new();
        let mut resets = 0;
        let px_size = 48.0_f32;
        let unique: Vec<_> = (0x4e00..0x4e00 + 6500)
            .map(|n| char::from_u32(n).unwrap())
            .collect();
        let consume =
            |atlas: &GlyphAtlas,
             batch: &mut Vec<((char, u32, FontKind), Slot)>,
             consumed: &mut Vec<char>| {
                let mut region = vec![false; (ATLAS_SIZE * ATLAS_SIZE) as usize];
                for (key, slot) in batch.drain(..) {
                    let actual = atlas.map[&key];
                    assert_eq!(
                        (actual.x, actual.y, actual.w, actual.h),
                        (slot.x, slot.y, slot.w, slot.h)
                    );
                    for y in slot.y..slot.y + slot.h {
                        let start = (y * ATLAS_SIZE + slot.x) as usize;
                        let row = &mut region[start..start + slot.w as usize];
                        assert!(!row.iter().any(|filled| *filled));
                        row.fill(true);
                    }
                    consumed.push(key.0);
                }
            };
        for (i, &ch) in unique.iter().enumerate() {
            let key = (ch, px_size.to_bits(), FontKind::Mono);
            let w = 19 + (i % 23) as u32;
            let h = 24 + (i % 31) as u32;
            let slot = if let Some(slot) = atlas.insert(key, w, h) {
                slot
            } else {
                consume(&atlas, &mut batch, &mut consumed);
                atlas.clear();
                resets += 1;
                atlas.insert(key, w, h).unwrap()
            };
            batch.push((key, slot));
        }
        consume(&atlas, &mut batch, &mut consumed);
        assert!(resets > 5);
        assert_eq!(consumed, unique);
    }

    #[test]
    fn glyph_vertices_address_exactly_the_clipped_bitmap_region() {
        let glyph = SceneGlyph {
            ch: 'Ж',
            px_size: 20.0,
            font_kind: FontKind::Mono,
            x: 27,
            y: 18,
            w: 6,
            h: 11,
            source: (2, 3),
            color: [1.0; 4],
        };
        let slot = Slot {
            x: 740,
            y: 915,
            w: 8,
            h: 15,
        };
        let mut vertices = Vec::new();
        append_glyph(&mut vertices, &glyph, slot);
        let vertex = |index: usize| {
            let p = &vertices[index * 8..index * 8 + 4];
            (
                p[0],
                p[1],
                p[2] * ATLAS_SIZE as f32,
                p[3] * ATLAS_SIZE as f32,
            )
        };
        assert_eq!(vertex(0), (27.0, 18.0, 742.0, 918.0));
        assert_eq!(vertex(1), (33.0, 18.0, 748.0, 918.0));
        assert_eq!(vertex(2), (33.0, 29.0, 748.0, 929.0));
        assert_eq!(vertex(5), (27.0, 29.0, 742.0, 929.0));
    }

    #[test]
    fn offscreen_egl_pbuffer_verifies_rounded_rect_and_image_shaders_when_available() {
        // Attempt an offscreen EGL PBuffer OpenGL ES 2.0 context to verify our
        // SDF rounded-rectangle and image shaders compile, link, and actually
        // round corners on real pixels. Gracefully skips if the test runner has
        // no EGL/OpenGL driver available.
        let Ok(egl) = (unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }) else {
            return;
        };
        let Ok(display) = (unsafe { egl.get_display(egl::DEFAULT_DISPLAY) })
            .ok_or("no default EGL display")
        else {
            return;
        };
        if egl.initialize(display).is_err() || egl.bind_api(egl::OPENGL_ES_API).is_err() {
            return;
        }
        let attrs = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
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
        let Ok(Some(config)) = egl.choose_first_config(display, &attrs) else {
            let _ = egl.terminate(display);
            return;
        };
        let Ok(context) = egl.create_context(
            display,
            config,
            None,
            &[
                egl::CONTEXT_MAJOR_VERSION,
                2,
                egl::CONTEXT_MINOR_VERSION,
                0,
                egl::NONE,
            ],
        ) else {
            let _ = egl.terminate(display);
            return;
        };
        let (w, h) = (128i32, 128i32);
        let Ok(surface) = egl.create_pbuffer_surface(
            display,
            config,
            &[egl::WIDTH, w, egl::HEIGHT, h, egl::NONE],
        ) else {
            let _ = egl.destroy_context(display, context);
            let _ = egl.terminate(display);
            return;
        };
        if egl
            .make_current(display, Some(surface), Some(surface), Some(context))
            .is_err()
        {
            let _ = egl.destroy_surface(display, surface);
            let _ = egl.destroy_context(display, context);
            let _ = egl.terminate(display);
            return;
        }
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|f| f as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };
        let (rounded_prog, rounded_size) =
            compile_rounded_program(&gl).expect("rounded_program must compile and link");
        let _image_prog = compile_image_program(&gl).expect("image_program must compile and link");

        // Draw a blue rounded rect (radius=24) at (16, 16, 96, 96) over green clear color.
        let vbo = unsafe { gl.create_buffer() }.unwrap();
        let (rx, ry, rw, rh, rad) = (16.0_f32, 16.0_f32, 96.0_f32, 96.0_f32, 24.0_f32);
        let (cx, cy, hw, hh) = (rx + rw * 0.5, ry + rh * 0.5, rw * 0.5, rh * 0.5);
        let mut verts = Vec::new();
        for (px, py) in [
            (rx - 1.0, ry - 1.0),
            (rx + rw + 1.0, ry - 1.0),
            (rx + rw + 1.0, ry + rh + 1.0),
            (rx - 1.0, ry - 1.0),
            (rx + rw + 1.0, ry + rh + 1.0),
            (rx - 1.0, ry + rh + 1.0),
        ] {
            verts.extend_from_slice(&[
                px, py, px - cx, py - cy, hw, hh, rad, 0.0, 0.0, 0.0, 0.1, 0.2, 0.95, 1.0, 0.0,
                0.0, 0.0, 0.0, -1e6, -1e6, 1e6, 1e6,
            ]);
        }
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        unsafe {
            gl.viewport(0, 0, w, h);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.clear_color(0.0, 0.9, 0.1, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(rounded_prog));
            if let Some(loc) = &rounded_size {
                gl.uniform_2_f32(Some(loc), w as f32, h as f32);
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, as_bytes(&verts), glow::STREAM_DRAW);
            let stride = 22 * 4;
            for loc in 0..7 {
                gl.enable_vertex_attrib_array(loc);
            }
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 2 * 4);
            gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, 4 * 4);
            gl.vertex_attrib_pointer_f32(3, 4, glow::FLOAT, false, stride, 6 * 4);
            gl.vertex_attrib_pointer_f32(4, 4, glow::FLOAT, false, stride, 10 * 4);
            gl.vertex_attrib_pointer_f32(5, 4, glow::FLOAT, false, stride, 14 * 4);
            gl.vertex_attrib_pointer_f32(6, 4, glow::FLOAT, false, stride, 18 * 4);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            gl.finish();
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.read_pixels(
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        // In GL read_pixels, row 0 is bottom; top-left corner (16, 16) is at gl_y = h - 1 - 16.
        let sample = |x: usize, top_y: usize| -> [u8; 4] {
            let gl_y = h as usize - 1 - top_y;
            let off = (gl_y * w as usize + x) * 4;
            [pixels[off], pixels[off + 1], pixels[off + 2], pixels[off + 3]]
        };
        let corner = sample(16, 16);
        let center = sample(64, 64);
        assert!(
            corner[1] > 180 && corner[2] < 60,
            "GL rounded rect corner (16,16) must be green bg, got RGBA={corner:?}"
        );
        assert!(
            center[2] > 200 && center[1] < 80,
            "GL rounded rect center (64,64) must be blue fill, got RGBA={center:?}"
        );
        let _ = egl.make_current(display, None, None, None);
        let _ = egl.destroy_surface(display, surface);
        let _ = egl.destroy_context(display, context);
        let _ = egl.terminate(display);
    }
}
