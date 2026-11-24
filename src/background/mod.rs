// This Source Code Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Beyrix Background Engine: native procedural 3D character-grid backgrounds.
//!
//! Operates independently of the JavaScript Runtime, supports native declarative
//! data bindings, auto-quality throttling under load, and composites behind
//! terminal text in both OpenGL ES and software backends.

#![allow(dead_code)]

pub mod bindings;
pub mod presets;
pub mod scene3d;

use std::time::{Duration, Instant};

use crate::config::{BackgroundConfig, BackgroundPreset, BackgroundTuiBehavior};
use crate::render::{Renderer, Scene, SceneGlyph};
use crate::theme::Theme;

use bindings::{BindingInputs, BindingTarget, DataBinding, DataSource, evaluate_bindings};
use presets::build_preset_scene;
use scene3d::CharRasterizer;

pub struct BackgroundEngine {
    pub config: BackgroundConfig,
    pub bindings: Vec<DataBinding>,
    rasterizer: CharRasterizer,
    start_time: Instant,
    last_frame: Instant,
    smoothed_frame_ms: f32,
    effective_fps: u32,
}

impl BackgroundEngine {
    pub fn new(config: BackgroundConfig) -> Self {
        let mut bindings = Vec::new();
        if config.audio_reactive || config.preset == BackgroundPreset::AudioReactive {
            bindings.push(DataBinding::new(
                "bass-scale",
                DataSource::AudioBass,
                BindingTarget::ObjectScale,
                0.85,
                1.45,
            ));
            bindings.push(DataBinding::new(
                "rms-light",
                DataSource::AudioRms,
                BindingTarget::LightIntensity,
                0.7,
                1.6,
            ));
            bindings.push(DataBinding::new(
                "beat-particles",
                DataSource::AudioBeat,
                BindingTarget::ParticleImpulse,
                0.0,
                1.0,
            ));
        }
        let fps = config.fps.clamp(1, 144);
        Self {
            config,
            bindings,
            rasterizer: CharRasterizer::new(64, 24),
            start_time: Instant::now(),
            last_frame: Instant::now(),
            smoothed_frame_ms: 0.0,
            effective_fps: fps,
        }
    }

    pub fn is_active(&self, focused: bool, in_tui: bool) -> bool {
        if !self.config.enabled {
            return false;
        }
        if self.config.pause_on_unfocus && !focused {
            return false;
        }
        if in_tui && self.config.tui_behavior != BackgroundTuiBehavior::Keep {
            return false;
        }
        true
    }

    pub fn frame_interval(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.effective_fps.max(1) as f32)
    }

    pub fn should_step(&self) -> bool {
        self.last_frame.elapsed() >= self.frame_interval()
    }

    /// Render the 3D scene into `scene.behind_glyphs`.
    #[allow(clippy::too_many_arguments)]
    pub fn render_to_scene(
        &mut self,
        win_size: (f32, f32),
        scale: f32,
        focused: bool,
        in_tui: bool,
        mouse_norm: (f32, f32),
        inputs: &BindingInputs,
        theme: &Theme,
        renderer: &mut Renderer,
        scene: &mut Scene,
    ) {
        if !self.is_active(focused, in_tui) {
            return;
        }
        let t0 = Instant::now();
        let px_size =
            (self.config.char_size * scale * self.config.resolution_scale).clamp(8.0, 48.0);
        let cell_w = (px_size * 0.62 * self.config.char_density.clamp(0.4, 2.0)).max(6.0);
        let cell_h = (px_size * 1.15 * self.config.char_density.clamp(0.4, 2.0)).max(10.0);
        let cols = ((win_size.0 / cell_w).floor() as usize).clamp(8, 180);
        let rows = ((win_size.1 / cell_h).floor() as usize).clamp(4, 90);
        self.rasterizer.resize(cols, rows);

        let elapsed = self.start_time.elapsed().as_secs_f32() * self.config.speed;
        let mods = evaluate_bindings(&mut self.bindings, inputs);
        let accent = [
            theme.cursor[0] as f32 / 255.0,
            theme.cursor[1] as f32 / 255.0,
            theme.cursor[2] as f32 / 255.0,
        ];
        let mut s3d = build_preset_scene(self.config.preset, elapsed, &mods, mouse_norm, accent);
        if !self.config.mouse_reactive {
            s3d = build_preset_scene(self.config.preset, elapsed, &mods, (0.5, 0.5), accent);
        }
        self.rasterizer.render(
            &s3d,
            &self.config.charset,
            mods.brightness * self.config.effect_intensity,
            cell_w / cell_h,
        );

        let alpha = self.config.alpha.clamp(0.0, 1.0);
        for row in 0..rows {
            let py = row as f32 * cell_h + cell_h * 0.8;
            for col in 0..cols {
                let cell = self.rasterizer.cells[row * cols + col];
                if cell.ch == ' ' {
                    continue;
                }
                let px = col as f32 * cell_w;
                let (metrics, _) = renderer.glyph_bitmap(cell.ch, px_size);
                if metrics.width == 0 || metrics.height == 0 {
                    continue;
                }
                let gx = (px + metrics.xmin as f32).round() as isize;
                let gy = (py - metrics.height as f32 - metrics.ymin as f32).round() as isize;
                if gx < 0 || gy < 0 {
                    continue;
                }
                let x = gx as usize;
                let y = gy as usize;
                let w = metrics.width.min((win_size.0 as usize).saturating_sub(x));
                let h = metrics.height.min((win_size.1 as usize).saturating_sub(y));
                if w > 0 && h > 0 {
                    scene.behind_glyphs.push(SceneGlyph {
                        ch: cell.ch,
                        px_size,
                        x,
                        y,
                        w,
                        h,
                        source: (0, 0),
                        color: [cell.color[0], cell.color[1], cell.color[2], alpha],
                    });
                }
            }
        }

        let frame_ms = t0.elapsed().as_secs_f32() * 1000.0;
        self.smoothed_frame_ms = self.smoothed_frame_ms * 0.85 + frame_ms * 0.15;
        if self.config.auto_quality {
            if self.smoothed_frame_ms > 18.0 && self.effective_fps > 15 {
                self.effective_fps = (self.effective_fps / 2).max(15);
            } else if self.smoothed_frame_ms < 6.0 && self.effective_fps < self.config.fps {
                self.effective_fps = self.config.fps;
            }
        }
        self.last_frame = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_render_into_behind_glyphs_and_pause_in_tui() {
        let mut renderer = Renderer::new().unwrap();
        let theme = Theme::default();
        let inputs = BindingInputs {
            audio_bass: 0.7,
            audio_rms: 0.5,
            audio_beat: 1.0,
            ..Default::default()
        };
        for preset in BackgroundPreset::ALL {
            let mut cfg = BackgroundConfig {
                enabled: true,
                preset,
                ..Default::default()
            };
            cfg.audio_reactive = true;
            let mut engine = BackgroundEngine::new(cfg);
            let mut scene = Scene::default();
            engine.render_to_scene(
                (800.0, 600.0),
                1.0,
                true,
                false,
                (0.5, 0.5),
                &inputs,
                &theme,
                &mut renderer,
                &mut scene,
            );
            assert!(
                !scene.behind_glyphs.is_empty(),
                "preset {:?} produced no background glyphs",
                preset
            );

            let mut tui_scene = Scene::default();
            engine.render_to_scene(
                (800.0, 600.0),
                1.0,
                true,
                true,
                (0.5, 0.5),
                &inputs,
                &theme,
                &mut renderer,
                &mut tui_scene,
            );
            assert!(tui_scene.behind_glyphs.is_empty());
        }
    }
}
