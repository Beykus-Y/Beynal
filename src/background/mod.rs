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

use bindings::{BindingInputs, BindingTarget, DataBinding, DataSource, FrameBindingCache};
use glam::Vec3;
use presets::build_preset_scene;
use scene3d::{
    CharRasterizer, Material3D, Mesh3D, Object3D, ProjectionMode, Scene3D, grid_mesh,
    load_obj_mesh, sphere_mesh, torus_mesh,
};

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct CustomObjectSpec {
    pub id: String,
    pub primitive: String,
    #[serde(default)]
    pub obj_data: Option<String>,
    #[serde(default)]
    pub color: Option<[f32; 3]>,
    #[serde(default)]
    pub position: Option<[f32; 3]>,
    #[serde(default)]
    pub rotation: Option<[f32; 3]>,
    #[serde(default)]
    pub scale: Option<[f32; 3]>,
}

pub struct BackgroundEngine {
    pub config: BackgroundConfig,
    pub bindings: Vec<DataBinding>,
    pub custom_scene: Option<Scene3D>,
    pub scene_owner: Option<String>,
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
            custom_scene: None,
            scene_owner: None,
            rasterizer: CharRasterizer::new(64, 24),
            start_time: Instant::now(),
            last_frame: Instant::now(),
            smoothed_frame_ms: 0.0,
            effective_fps: fps,
        }
    }

    pub fn set_preset(
        &mut self,
        owner: Option<&str>,
        preset: BackgroundPreset,
    ) -> Result<(), String> {
        if let (Some(req_owner), Some(existing)) = (owner, &self.scene_owner)
            && req_owner != existing
        {
            return Err("background scene is owned by another extension".to_string());
        }
        self.config.preset = preset;
        self.custom_scene = None;
        self.scene_owner = owner.map(str::to_owned);
        Ok(())
    }

    pub fn clear_scene(&mut self, owner: &str) -> Result<(), String> {
        if let Some(existing) = &self.scene_owner
            && existing != owner
        {
            return Err("background scene is owned by another extension".to_string());
        }
        self.scene_owner = Some(owner.to_string());
        self.custom_scene = Some(Scene3D::default());
        Ok(())
    }

    pub fn release_scene(&mut self, owner: &str) {
        if self.scene_owner.as_deref() == Some(owner) {
            self.scene_owner = None;
            self.custom_scene = None;
        }
        self.bindings.retain(|b| b.owner != owner);
    }

    fn ensure_custom_scene_mut(&mut self, owner: &str) -> Option<&mut Scene3D> {
        if let Some(existing) = &self.scene_owner
            && existing != owner
        {
            return None;
        }
        if self.custom_scene.is_none() {
            self.scene_owner = Some(owner.to_string());
            self.custom_scene = Some(Scene3D::default());
        }
        self.custom_scene.as_mut()
    }

    pub fn add_object(&mut self, owner: &str, spec: CustomObjectSpec) -> Result<(), String> {
        let mesh = match spec.primitive.as_str() {
            "torus" => torus_mesh(1.1, 0.4, 24, 12),
            "sphere" => sphere_mesh(1.0, 14, 22),
            "plane" => grid_mesh(12, 12, 4.0, |_, _| 0.0),
            "cube" => box_mesh(1.2),
            "obj" => {
                let data = spec.obj_data.as_deref().ok_or("missing obj_data")?;
                load_obj_mesh(data)?
            }
            other => return Err(format!("unknown primitive {other:?}")),
        };
        let Some(scene) = self.ensure_custom_scene_mut(owner) else {
            return Err("background scene is owned by another extension".to_string());
        };
        scene.objects.retain(|o| o.id != spec.id);
        scene.objects.push(Object3D {
            id: spec.id,
            mesh,
            material: Material3D {
                color: spec.color.unwrap_or([0.45, 0.78, 0.96]),
                ..Default::default()
            },
            position: spec.position.map_or(Vec3::ZERO, Vec3::from_array),
            rotation: spec.rotation.map_or(Vec3::ZERO, Vec3::from_array),
            scale: spec.scale.map_or(Vec3::ONE, Vec3::from_array),
        });
        Ok(())
    }

    pub fn update_object(
        &mut self,
        owner: &str,
        id: &str,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
        scale: Option<[f32; 3]>,
        color: Option<[f32; 3]>,
    ) {
        let Some(scene) = self.ensure_custom_scene_mut(owner) else {
            return;
        };
        if let Some(obj) = scene.objects.iter_mut().find(|o| o.id == id) {
            if let Some(p) = position {
                obj.position = Vec3::from_array(p);
            }
            if let Some(r) = rotation {
                obj.rotation = Vec3::from_array(r);
            }
            if let Some(s) = scale {
                obj.scale = Vec3::from_array(s);
            }
            if let Some(c) = color {
                obj.material.color = c;
            }
        }
    }

    pub fn remove_object(&mut self, owner: &str, id: &str) {
        if let Some(scene) = self.ensure_custom_scene_mut(owner) {
            scene.objects.retain(|o| o.id != id);
        }
    }

    pub fn set_camera(
        &mut self,
        owner: &str,
        position: Option<[f32; 3]>,
        target: Option<[f32; 3]>,
        fov_deg: Option<f32>,
        ortho: Option<bool>,
    ) {
        let Some(scene) = self.ensure_custom_scene_mut(owner) else {
            return;
        };
        if let Some(p) = position {
            scene.camera.position = Vec3::from_array(p);
        }
        if let Some(t) = target {
            scene.camera.target = Vec3::from_array(t);
        }
        if let Some(fov) = fov_deg {
            scene.camera.fov_rad = fov.clamp(15.0, 120.0).to_radians();
        }
        if let Some(o) = ortho {
            scene.camera.projection = if o {
                ProjectionMode::Orthographic
            } else {
                ProjectionMode::Perspective
            };
        }
    }

    pub fn set_light(
        &mut self,
        owner: &str,
        direction: Option<[f32; 3]>,
        color: Option<[f32; 3]>,
        intensity: Option<f32>,
    ) {
        let Some(scene) = self.ensure_custom_scene_mut(owner) else {
            return;
        };
        if let Some(d) = direction {
            scene.light.direction = Vec3::from_array(d).normalize_or_zero();
        }
        if let Some(c) = color {
            scene.light.color = c;
        }
        if let Some(i) = intensity {
            scene.light.intensity = i.clamp(0.0, 5.0);
        }
    }

    pub fn set_charset(&mut self, owner: &str, charset: String) -> Result<(), String> {
        if let Some(existing) = &self.scene_owner
            && existing != owner
        {
            return Err("background scene is owned by another extension".to_string());
        }
        self.config.charset = charset;
        Ok(())
    }

    pub fn add_binding(&mut self, owner: &str, mut binding: DataBinding) -> Result<(), String> {
        if let Some(existing) = self.bindings.iter().find(|b| b.id == binding.id)
            && !existing.owner.is_empty()
            && existing.owner != owner
        {
            return Err("binding is owned by another extension".to_string());
        }
        binding.owner = owner.to_string();
        self.bindings.retain(|b| b.id != binding.id);
        self.bindings.push(binding);
        Ok(())
    }

    pub fn remove_binding(&mut self, owner: &str, id: &str) -> Result<(), String> {
        if let Some(existing) = self.bindings.iter().find(|b| b.id == id)
            && !existing.owner.is_empty()
            && existing.owner != owner
        {
            return Err("cannot remove binding owned by another extension".to_string());
        }
        self.bindings.retain(|b| b.id != id);
        Ok(())
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
        let cache = FrameBindingCache::evaluate_frame(&mut self.bindings, inputs);
        let mods = cache.global;
        let accent = [
            theme.cursor[0] as f32 / 255.0,
            theme.cursor[1] as f32 / 255.0,
            theme.cursor[2] as f32 / 255.0,
        ];
        let mut s3d = if let Some(custom) = &self.custom_scene {
            let mut s = custom.clone();
            for obj in &mut s.objects {
                let obj_mods = cache.for_object(&obj.id);
                obj.scale *= obj_mods.scale;
                obj.rotation.y += elapsed * 0.4 * obj_mods.rotation_speed;
            }
            s.light.intensity *= mods.light_intensity;
            s
        } else {
            build_preset_scene(
                self.config.preset,
                elapsed,
                &mods,
                if self.config.mouse_reactive {
                    mouse_norm
                } else {
                    (0.5, 0.5)
                },
                accent,
            )
        };
        let _ = &mut s3d;
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

fn box_mesh(size: f32) -> Mesh3D {
    let h = size * 0.5;
    let obj = format!(
        "v -{h} -{h} {h}\nv {h} -{h} {h}\nv {h} {h} {h}\nv -{h} {h} {h}\n\
         v -{h} -{h} -{h}\nv {h} -{h} -{h}\nv {h} {h} -{h}\nv -{h} {h} -{h}\n\
         f 1 2 3\nf 1 3 4\nf 6 5 8\nf 6 8 7\nf 4 3 7\nf 4 7 8\n\
         f 5 6 2\nf 5 2 1\nf 2 6 7\nf 2 7 3\nf 5 1 4\nf 5 4 8\n"
    );
    load_obj_mesh(&obj).unwrap()
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

    #[test]
    fn custom_scene_ownership_and_objects_render_natively() {
        let mut renderer = Renderer::new().unwrap();
        let theme = Theme::default();
        let mut engine = BackgroundEngine::new(BackgroundConfig {
            enabled: true,
            ..Default::default()
        });
        engine.clear_scene("ext.a").unwrap();
        engine
            .add_object(
                "ext.a",
                CustomObjectSpec {
                    id: "my-cube".into(),
                    primitive: "cube".into(),
                    obj_data: None,
                    color: Some([0.9, 0.4, 0.2]),
                    position: None,
                    rotation: Some([0.4, 0.6, 0.0]),
                    scale: None,
                },
            )
            .unwrap();
        // Another extension cannot hijack ext.a's scene, preset, or bindings
        assert!(engine.clear_scene("ext.b").is_err());
        assert!(
            engine
                .set_preset(Some("ext.b"), BackgroundPreset::Starfield)
                .is_err()
        );
        assert!(
            engine
                .add_object(
                    "ext.b",
                    CustomObjectSpec {
                        id: "hijack".into(),
                        primitive: "sphere".into(),
                        obj_data: None,
                        color: None,
                        position: None,
                        rotation: None,
                        scale: None,
                    }
                )
                .is_err()
        );
        let mut b = DataBinding::new(
            "cube-scale",
            DataSource::AudioBass,
            BindingTarget::ObjectScale,
            1.0,
            2.0,
        );
        b.object_id = Some("my-cube".into());
        engine.add_binding("ext.a", b).unwrap();
        assert!(engine.remove_binding("ext.b", "cube-scale").is_err());
        assert!(engine.set_charset("ext.b", ".:*#".into()).is_err());
        assert!(engine.set_charset("ext.a", ".:*#".into()).is_ok());

        let mut scene = Scene::default();
        engine.render_to_scene(
            (800.0, 600.0),
            1.0,
            true,
            false,
            (0.5, 0.5),
            &BindingInputs {
                audio_bass: 1.0,
                ..Default::default()
            },
            &theme,
            &mut renderer,
            &mut scene,
        );
        assert!(!scene.behind_glyphs.is_empty());

        // Releasing scene (e.g. on extension unload) frees custom_scene & bindings
        engine.release_scene("ext.a");
        assert!(engine.custom_scene.is_none());
        assert!(engine.scene_owner.is_none());
        assert!(engine.bindings.is_empty());
    }
}
