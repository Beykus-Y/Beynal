// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native declarative data bindings for Beyrix Background Engine.
//!
//! Allows extensions or presets to bind live data sources (Audio Bass/Treble/RMS/Beat,
//! CPU, RAM, X-Ray activity, Mouse, Time) to 3D scene properties with smoothing,
//! range mapping, clamping, and composition — without per-frame JS execution.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DataSource {
    AudioBass,
    AudioMid,
    AudioTreble,
    AudioRms,
    AudioPeak,
    AudioBeat,
    CpuUsage,
    RamUsage,
    XRayActivity,
    MouseX,
    MouseY,
    Time,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BindingTarget {
    ObjectScale,
    RotationSpeed,
    LightIntensity,
    ParticleImpulse,
    Brightness,
    WaveAmplitude,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CombineMode {
    #[default]
    Replace,
    Add,
    Multiply,
    Max,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataBinding {
    pub id: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub object_id: Option<String>,
    pub source: DataSource,
    pub target: BindingTarget,
    pub in_min: f32,
    pub in_max: f32,
    pub out_min: f32,
    pub out_max: f32,
    pub smoothing: f32,
    pub combine: CombineMode,
    #[serde(skip)]
    pub smoothed_value: f32,
}

impl DataBinding {
    pub fn new(
        id: impl Into<String>,
        source: DataSource,
        target: BindingTarget,
        out_min: f32,
        out_max: f32,
    ) -> Self {
        Self {
            id: id.into(),
            owner: String::new(),
            object_id: None,
            source,
            target,
            in_min: 0.0,
            in_max: 1.0,
            out_min,
            out_max,
            smoothing: 0.35,
            combine: CombineMode::Replace,
            smoothed_value: out_min,
        }
    }

    pub fn evaluate(&mut self, inputs: &BindingInputs) -> f32 {
        let raw = inputs.read(self.source);
        let span = (self.in_max - self.in_min).abs().max(1e-5);
        let t = ((raw - self.in_min) / span).clamp(0.0, 1.0);
        let mapped = self.out_min + t * (self.out_max - self.out_min);
        let s = self.smoothing.clamp(0.0, 0.98);
        self.smoothed_value = self.smoothed_value * s + mapped * (1.0 - s);
        self.smoothed_value
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BindingInputs {
    pub audio_bass: f32,
    pub audio_mid: f32,
    pub audio_treble: f32,
    pub audio_rms: f32,
    pub audio_peak: f32,
    pub audio_beat: f32,
    pub cpu_usage: f32,
    pub ram_usage: f32,
    pub xray_activity: f32,
    pub mouse_x: f32,
    pub mouse_y: f32,
    pub time_secs: f32,
}

impl BindingInputs {
    pub fn read(&self, source: DataSource) -> f32 {
        match source {
            DataSource::AudioBass => self.audio_bass,
            DataSource::AudioMid => self.audio_mid,
            DataSource::AudioTreble => self.audio_treble,
            DataSource::AudioRms => self.audio_rms,
            DataSource::AudioPeak => self.audio_peak,
            DataSource::AudioBeat => self.audio_beat,
            DataSource::CpuUsage => self.cpu_usage,
            DataSource::RamUsage => self.ram_usage,
            DataSource::XRayActivity => self.xray_activity,
            DataSource::MouseX => self.mouse_x,
            DataSource::MouseY => self.mouse_y,
            DataSource::Time => self.time_secs.sin() * 0.5 + 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EvaluatedModifiers {
    pub scale: f32,
    pub rotation_speed: f32,
    pub light_intensity: f32,
    pub particle_impulse: f32,
    pub brightness: f32,
    pub wave_amplitude: f32,
}

impl Default for EvaluatedModifiers {
    fn default() -> Self {
        Self {
            scale: 1.0,
            rotation_speed: 1.0,
            light_intensity: 1.0,
            particle_impulse: 0.0,
            brightness: 1.0,
            wave_amplitude: 1.0,
        }
    }
}

/// Pre-evaluated per-frame binding cache so every binding's EMA smoothing is
/// stepped **exactly once per frame** regardless of how many objects are in the scene.
#[derive(Clone, Debug)]
pub struct FrameBindingCache {
    pub global: EvaluatedModifiers,
    pub per_object: std::collections::HashMap<String, EvaluatedModifiers>,
}

impl FrameBindingCache {
    pub fn evaluate_frame(bindings: &mut [DataBinding], inputs: &BindingInputs) -> Self {
        let mut global = EvaluatedModifiers::default();
        let mut per_object: std::collections::HashMap<String, EvaluatedModifiers> =
            std::collections::HashMap::new();
        for b in bindings {
            let v = b.evaluate(inputs);
            let mods = if let Some(oid) = &b.object_id {
                per_object.entry(oid.clone()).or_default()
            } else {
                &mut global
            };
            let slot = match b.target {
                BindingTarget::ObjectScale => &mut mods.scale,
                BindingTarget::RotationSpeed => &mut mods.rotation_speed,
                BindingTarget::LightIntensity => &mut mods.light_intensity,
                BindingTarget::ParticleImpulse => &mut mods.particle_impulse,
                BindingTarget::Brightness => &mut mods.brightness,
                BindingTarget::WaveAmplitude => &mut mods.wave_amplitude,
            };
            *slot = match b.combine {
                CombineMode::Replace => v,
                CombineMode::Add => *slot + v,
                CombineMode::Multiply => *slot * v,
                CombineMode::Max => slot.max(v),
            };
        }
        Self { global, per_object }
    }

    pub fn for_object(&self, object_id: &str) -> EvaluatedModifiers {
        self.per_object
            .get(object_id)
            .copied()
            .unwrap_or(self.global)
    }
}

pub fn evaluate_bindings(
    bindings: &mut [DataBinding],
    inputs: &BindingInputs,
) -> EvaluatedModifiers {
    FrameBindingCache::evaluate_frame(bindings, inputs).global
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_map_smooth_and_compose_natively() {
        let mut bindings = vec![
            DataBinding::new(
                "bass-scale",
                DataSource::AudioBass,
                BindingTarget::ObjectScale,
                1.0,
                2.0,
            ),
            DataBinding::new(
                "rms-light",
                DataSource::AudioRms,
                BindingTarget::LightIntensity,
                0.5,
                1.8,
            ),
        ];
        bindings[0].smoothing = 0.0;
        bindings[1].smoothing = 0.0;
        let inputs = BindingInputs {
            audio_bass: 0.5,
            audio_rms: 1.0,
            ..Default::default()
        };
        let mods = evaluate_bindings(&mut bindings, &inputs);
        assert!((mods.scale - 1.5).abs() < 1e-4);
        assert!((mods.light_intensity - 1.8).abs() < 1e-4);
    }
}
