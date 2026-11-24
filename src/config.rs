// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration structures for Beyrix next-generation subsystems:
//! BeyrixWidgets, Background Engine, Media/Audio, and X-Ray.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Master configuration for all extensible / optional subsystems.
///
/// When a subsystem's `enabled` flag is `false`, Beyrix does not spawn its
/// background threads, does not poll system APIs, and does not load the JS VM.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlatformConfig {
    pub widgets: WidgetsConfig,
    pub background: BackgroundConfig,
    pub media: MediaConfig,
    pub xray: XRayConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WidgetsConfig {
    pub enabled: bool,
    pub extensions_dir: Option<PathBuf>,
    pub active_profile: String,
    pub hot_reload: bool,
    pub memory_limit_mb: usize,
    pub stack_limit_kb: usize,
    pub execution_timeout_ms: u64,
    pub editor_hotkey: String,
}

impl Default for WidgetsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extensions_dir: None,
            active_profile: "default".to_string(),
            hot_reload: true,
            memory_limit_mb: 16,
            stack_limit_kb: 512,
            execution_timeout_ms: 50,
            editor_hotkey: "Ctrl+Shift+E".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundPreset {
    #[default]
    Torus,
    Sphere,
    Terrain,
    Ocean,
    Tunnel,
    Starfield,
    AudioReactive,
}

impl BackgroundPreset {
    pub const ALL: [Self; 7] = [
        Self::Torus,
        Self::Sphere,
        Self::Terrain,
        Self::Ocean,
        Self::Tunnel,
        Self::Starfield,
        Self::AudioReactive,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Torus => "torus",
            Self::Sphere => "sphere",
            Self::Terrain => "terrain",
            Self::Ocean => "ocean",
            Self::Tunnel => "tunnel",
            Self::Starfield => "starfield",
            Self::AudioReactive => "audio-reactive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == s)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundTuiBehavior {
    #[default]
    Pause,
    Hide,
    Keep,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BackgroundConfig {
    pub enabled: bool,
    pub preset: BackgroundPreset,
    pub fps: u32,
    pub resolution_scale: f32,
    pub char_density: f32,
    pub char_size: f32,
    pub charset: String,
    pub palette: String,
    pub speed: f32,
    pub alpha: f32,
    pub effect_intensity: f32,
    pub mouse_reactive: bool,
    pub audio_reactive: bool,
    pub pause_on_unfocus: bool,
    pub pause_on_minimize: bool,
    pub tui_behavior: BackgroundTuiBehavior,
    pub auto_quality: bool,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preset: BackgroundPreset::Torus,
            fps: 30,
            resolution_scale: 1.0,
            char_density: 1.0,
            char_size: 16.0,
            charset: " .:-=+*#%@".to_string(),
            palette: "theme".to_string(),
            speed: 1.0,
            alpha: 0.35,
            effect_intensity: 1.0,
            mouse_reactive: true,
            audio_reactive: false,
            pause_on_unfocus: true,
            pause_on_minimize: true,
            tui_behavior: BackgroundTuiBehavior::Pause,
            auto_quality: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MediaConfig {
    pub enabled: bool,
    pub mpris_enabled: bool,
    pub player_enabled: bool,
    pub audio_capture_enabled: bool,
    pub save_state: bool,
    pub fft_bands: usize,
    pub smoothing: f32,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mpris_enabled: true,
            player_enabled: true,
            audio_capture_enabled: false,
            save_state: false,
            fft_bands: 32,
            smoothing: 0.65,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum XRayLevel {
    #[default]
    Light,
    Inspect,
    Trace,
}

impl XRayLevel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Inspect => "inspect",
            Self::Trace => "trace",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "light" => Some(Self::Light),
            "inspect" => Some(Self::Inspect),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct XRayConfig {
    pub enabled: bool,
    pub level: XRayLevel,
    pub poll_interval_ms: u64,
    pub history_limit: usize,
    pub max_report_bytes: usize,
    pub privacy_mode: bool,
    pub hotkey: String,
    pub compact_indicator: bool,
}

impl Default for XRayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: XRayLevel::Light,
            poll_interval_ms: 250,
            history_limit: 50,
            max_report_bytes: 2 * 1024 * 1024,
            privacy_mode: true,
            hotkey: "Ctrl+Shift+X".to_string(),
            compact_indicator: true,
        }
    }
}

impl PlatformConfig {
    pub fn parse_toml(text: &str) -> Result<Self, String> {
        let cfg: Self = toml::from_str(text).map_err(|e| format!("invalid platform TOML: {e}"))?;
        if cfg.background.fps == 0 || cfg.background.fps > 144 {
            return Err("background.fps must be 1..=144".to_string());
        }
        if !(0.0..=1.0).contains(&cfg.background.alpha) {
            return Err("background.alpha must be 0.0..=1.0".to_string());
        }
        if cfg.xray.poll_interval_ms < 50 {
            return Err("xray.poll_interval_ms must be >= 50".to_string());
        }
        Ok(cfg)
    }

    pub fn default_extensions_dir() -> Option<PathBuf> {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|p| !p.is_empty() && std::path::Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .map(|p| p.join("beyrix/extensions"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_subsystems_are_disabled_for_zero_overhead() {
        let cfg = PlatformConfig::default();
        assert!(!cfg.widgets.enabled);
        assert!(!cfg.background.enabled);
        assert!(!cfg.media.enabled);
        assert!(!cfg.xray.enabled);
        assert!(cfg.xray.privacy_mode);
        assert!(!cfg.media.audio_capture_enabled);
    }

    #[test]
    fn parses_platform_toml_and_validates_bounds() {
        let toml = r#"
[widgets]
enabled = true
memory_limit_mb = 24

[background]
enabled = true
preset = "audio-reactive"
fps = 60
alpha = 0.45

[xray]
enabled = true
level = "inspect"
poll_interval_ms = 200
"#;
        let cfg = PlatformConfig::parse_toml(toml).unwrap();
        assert!(cfg.widgets.enabled);
        assert_eq!(cfg.widgets.memory_limit_mb, 24);
        assert_eq!(cfg.background.preset, BackgroundPreset::AudioReactive);
        assert_eq!(cfg.background.fps, 60);
        assert_eq!(cfg.xray.level, XRayLevel::Inspect);
    }
}
