// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native Visual Layout Editor Panel & Unified Beyrix Settings Overlay.
//!
//! Renders completely natively via `UiNode` -> `Scene` (no external browser
//! engine), adapts cleanly from wide fullscreen down to narrow Niri columns,
//! and binds every control directly to `PlatformConfig` / `WidgetManager` with
//! atomic TOML persistence.

use crate::config::{
    BackgroundPreset, BackgroundTuiBehavior, CtrlVPolicy, PlatformConfig, XRayLevel,
};
use crate::render::{Renderer, Scene, SceneRect};
use crate::theme::Theme;

use super::layout::{
    Anchor, EditorTab, NarrowBehavior, PlacementMode, SnapGuide, TuiBehavior, ViewportBreakpoint,
};
use super::ui::{Align, HitKind, UiColor, UiHitTarget, UiLayer, UiNode, emit_ui_tree_interactive};
use super::{ExtensionTrust, WidgetManager};

/// Active section of the Unified Settings Overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsSection {
    #[default]
    Keyboard,
    Widgets,
    Background,
    MediaAudio,
    XRay,
    PrivacyPerf,
}

impl SettingsSection {
    pub const ALL: [Self; 6] = [
        Self::Keyboard,
        Self::Widgets,
        Self::Background,
        Self::MediaAudio,
        Self::XRay,
        Self::PrivacyPerf,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Keyboard => "Keyboard",
            Self::Widgets => "Widgets",
            Self::Background => "Background",
            Self::MediaAudio => "Media & Audio",
            Self::XRay => "X-Ray",
            Self::PrivacyPerf => "Perf & Privacy",
        }
    }
}

/// Native Unified Settings Overlay state (works even when Widgets JS VM is off).
#[derive(Clone, Debug, Default)]
pub struct SettingsOverlay {
    pub open: bool,
    pub section: SettingsSection,
    pub status_message: Option<String>,
    pub hits: Vec<UiHitTarget>,
}

impl SettingsOverlay {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.status_message = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn emit_to_scene(
        &mut self,
        cfg: &PlatformConfig,
        win_size: (f32, f32),
        scale: f32,
        pointer_px: (f32, f32),
        theme: &Theme,
        renderer: &mut Renderer,
        scene: &mut Scene,
    ) {
        self.hits.clear();
        if !self.open {
            return;
        }
        let logical_w = win_size.0 / scale.max(0.25);
        let bp = ViewportBreakpoint::from_logical_width(logical_w);
        let panel_w = if bp.is_compact() {
            (win_size.0 - 20.0 * scale).max(260.0)
        } else {
            (620.0 * scale).min(win_size.0 - 32.0 * scale)
        };
        let panel_h = (440.0 * scale).min(win_size.1 - 32.0 * scale);
        let px = (win_size.0 - panel_w) * 0.5;
        let py = (win_size.1 - panel_h) * 0.5;

        let tab_labels: Vec<String> = SettingsSection::ALL
            .iter()
            .map(|s| s.label().to_string())
            .collect();
        let active_idx = SettingsSection::ALL
            .iter()
            .position(|&s| s == self.section)
            .unwrap_or(0);

        let body_children = match self.section {
            SettingsSection::Keyboard => vec![
                UiNode::Toggle {
                    id: "set.kbd.clear_copy".into(),
                    label: "Clear selection after Ctrl+C copy (next Ctrl+C interrupts)".into(),
                    checked: cfg.keyboard.clear_selection_after_copy,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.kbd.multiline_guard".into(),
                    label: "Multiline paste guard (when bracketed paste is off)".into(),
                    checked: cfg.keyboard.multiline_paste_protection,
                    disabled: false,
                },
                UiNode::Row {
                    gap: 8.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Ctrl+V policy: {:?}", cfg.keyboard.ctrl_v_policy),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Button {
                            id: "set.kbd.cycle_ctrl_v".into(),
                            label: "Cycle Policy".into(),
                            bg: Some(UiColor::Token("surface-elevated".into())),
                            color: Some(UiColor::Token("accent".into())),
                            width: Some(110.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                },
                UiNode::Text {
                    text: format!(
                        "Force Ctrl+C: {}  ·  Literal Ctrl+V: {}  ·  Settings: {}",
                        cfg.keyboard.force_ctrl_c_hotkey,
                        cfg.keyboard.literal_ctrl_v_hotkey,
                        cfg.keyboard.settings_hotkey
                    ),
                    size: Some(11.0),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                },
            ],
            SettingsSection::Widgets => vec![
                UiNode::Toggle {
                    id: "set.widgets.enabled".into(),
                    label: "Enable BeyrixWidgets JS Runtime".into(),
                    checked: cfg.widgets.enabled,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.widgets.hot_reload".into(),
                    label: "Hot reload extensions on file changes".into(),
                    checked: cfg.widgets.hot_reload,
                    disabled: false,
                },
                UiNode::Text {
                    text: format!(
                        "Profile: {}  ·  Memory cap: {} MB  ·  Timeout: {} ms  ·  Editor: {}",
                        cfg.widgets.active_profile,
                        cfg.widgets.memory_limit_mb,
                        cfg.widgets.execution_timeout_ms,
                        cfg.widgets.editor_hotkey
                    ),
                    size: Some(11.5),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                },
            ],
            SettingsSection::Background => vec![
                UiNode::Toggle {
                    id: "set.bg.enabled".into(),
                    label: "Enable 3D Background Engine".into(),
                    checked: cfg.background.enabled,
                    disabled: false,
                },
                UiNode::Row {
                    gap: 8.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Preset: {}", cfg.background.preset.name()),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Button {
                            id: "set.bg.cycle_preset".into(),
                            label: "Next Preset".into(),
                            bg: Some(UiColor::Token("surface-elevated".into())),
                            color: Some(UiColor::Token("accent".into())),
                            width: Some(100.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                        UiNode::Button {
                            id: "set.bg.cycle_fps".into(),
                            label: format!("{} FPS", cfg.background.fps),
                            bg: Some(UiColor::Token("surface-elevated".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(74.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                },
                UiNode::Toggle {
                    id: "set.bg.mouse".into(),
                    label: "Mouse-reactive camera".into(),
                    checked: cfg.background.mouse_reactive,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.bg.audio".into(),
                    label: "Audio-reactive bindings".into(),
                    checked: cfg.background.audio_reactive,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.bg.auto_quality".into(),
                    label: "Auto-quality FPS throttling under load".into(),
                    checked: cfg.background.auto_quality,
                    disabled: false,
                },
            ],
            SettingsSection::MediaAudio => vec![
                UiNode::Toggle {
                    id: "set.media.enabled".into(),
                    label: "Enable Media & Audio Subsystem".into(),
                    checked: cfg.media.enabled,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.media.mpris".into(),
                    label: "Linux MPRIS2 system media control".into(),
                    checked: cfg.media.mpris_enabled,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.media.player".into(),
                    label: "Native local audio player".into(),
                    checked: cfg.media.player_enabled,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.media.capture".into(),
                    label: "System audio capture (PipeWire opt-in for visualizers)".into(),
                    checked: cfg.media.audio_capture_enabled,
                    disabled: false,
                },
            ],
            SettingsSection::XRay => vec![
                UiNode::Toggle {
                    id: "set.xray.enabled".into(),
                    label: "Enable Beyrix X-Ray Diagnostics".into(),
                    checked: cfg.xray.enabled,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.xray.compact".into(),
                    label: "Show compact status indicator".into(),
                    checked: cfg.xray.compact_indicator,
                    disabled: false,
                },
                UiNode::Row {
                    gap: 8.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Level: {}", cfg.xray.level.name()),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Button {
                            id: "set.xray.cycle_level".into(),
                            label: "Switch Light / Inspect".into(),
                            bg: Some(UiColor::Token("surface-elevated".into())),
                            color: Some(UiColor::Token("cyan".into())),
                            width: Some(150.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                },
            ],
            SettingsSection::PrivacyPerf => vec![
                UiNode::Toggle {
                    id: "set.xray.privacy".into(),
                    label: "X-Ray Privacy Mode (redact tokens/secrets in reports)".into(),
                    checked: cfg.xray.privacy_mode,
                    disabled: false,
                },
                UiNode::Toggle {
                    id: "set.bg.pause_unfocus".into(),
                    label: "Pause Background Engine when window unfocused".into(),
                    checked: cfg.background.pause_on_unfocus,
                    disabled: false,
                },
                UiNode::Row {
                    gap: 8.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Background in TUI: {:?}", cfg.background.tui_behavior),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Button {
                            id: "set.bg.cycle_tui".into(),
                            label: "Cycle TUI Mode".into(),
                            bg: Some(UiColor::Token("surface-elevated".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(110.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                },
            ],
        };

        let mut rows = vec![
            UiNode::Row {
                gap: 8.0,
                align: Align::Center,
                children: vec![
                    UiNode::Text {
                        text: "Beyrix Settings".into(),
                        size: Some(15.0),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                    UiNode::Badge {
                        text: "platform.toml".into(),
                        color: Some(UiColor::Token("accent".into())),
                        bg: None,
                    },
                    UiNode::Button {
                        id: "set.save".into(),
                        label: "Save".into(),
                        bg: Some(UiColor::Token("accent-soft".into())),
                        color: Some(UiColor::Token("accent".into())),
                        width: Some(64.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                    UiNode::Button {
                        id: "set.reset".into(),
                        label: "Reset".into(),
                        bg: Some(UiColor::Token("surface-elevated".into())),
                        color: Some(UiColor::Token("warning".into())),
                        width: Some(64.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                    UiNode::Button {
                        id: "set.close".into(),
                        label: "✕".into(),
                        bg: Some(UiColor::Token("surface-elevated".into())),
                        color: Some(UiColor::Token("muted".into())),
                        width: Some(28.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                ],
            },
            UiNode::Tabs {
                id: "set.tabs".into(),
                tabs: tab_labels,
                active: active_idx,
            },
            UiNode::Divider {
                vertical: false,
                color: None,
            },
        ];
        rows.extend(body_children);
        if let Some(msg) = &self.status_message {
            rows.push(UiNode::Divider {
                vertical: false,
                color: None,
            });
            rows.push(UiNode::Text {
                text: msg.clone(),
                size: Some(11.5),
                color: Some(UiColor::Token("success".into())),
                align: Align::Start,
            });
        }

        let tree = UiNode::Container {
            padding: 14.0,
            bg: Some(UiColor::Token("surface-elevated".into())),
            border: Some(UiColor::Token("accent".into())),
            radius: 12.0,
            width: None,
            height: None,
            child: Box::new(UiNode::Column {
                gap: 10.0,
                align: Align::Stretch,
                children: rows,
            }),
        };

        emit_ui_tree_interactive(
            "beyrix-settings",
            &tree,
            (px, py, panel_w, panel_h),
            scale,
            0.98,
            UiLayer::Overlay,
            Some(pointer_px),
            theme,
            renderer,
            scene,
            &mut self.hits,
        );
    }

    /// Handle a click inside the Settings Overlay. Returns `(consumed, config_changed)`.
    pub fn handle_click(
        &mut self,
        pointer_px: (f32, f32),
        cfg: &mut PlatformConfig,
    ) -> (bool, bool) {
        if !self.open {
            return (false, false);
        }
        for hit in self.hits.iter().rev() {
            if pointer_px.0 >= hit.x
                && pointer_px.0 <= hit.x + hit.w
                && pointer_px.1 >= hit.y
                && pointer_px.1 <= hit.y + hit.h
            {
                let mut changed = false;
                match (hit.node_id.as_str(), hit.kind) {
                    ("set.tabs", HitKind::Tab(idx)) => {
                        if let Some(&sec) = SettingsSection::ALL.get(idx) {
                            self.section = sec;
                        }
                    }
                    ("set.close", _) => self.open = false,
                    ("set.save", _) => {
                        if let Some(path) = PlatformConfig::config_path() {
                            match cfg.save_atomic(&path) {
                                Ok(()) => {
                                    self.status_message =
                                        Some(format!("Saved atomically to {}", path.display()))
                                }
                                Err(e) => self.status_message = Some(format!("Save error: {e}")),
                            }
                        }
                    }
                    ("set.reset", _) => {
                        *cfg = PlatformConfig::default();
                        self.status_message =
                            Some("Reset to defaults (click Save to persist)".into());
                        changed = true;
                    }
                    ("set.kbd.clear_copy", _) => {
                        cfg.keyboard.clear_selection_after_copy =
                            !cfg.keyboard.clear_selection_after_copy;
                        changed = true;
                    }
                    ("set.kbd.multiline_guard", _) => {
                        cfg.keyboard.multiline_paste_protection =
                            !cfg.keyboard.multiline_paste_protection;
                        changed = true;
                    }
                    ("set.kbd.cycle_ctrl_v", _) => {
                        cfg.keyboard.ctrl_v_policy = match cfg.keyboard.ctrl_v_policy {
                            CtrlVPolicy::Smart => CtrlVPolicy::AlwaysPaste,
                            CtrlVPolicy::AlwaysPaste => CtrlVPolicy::AlwaysLiteral,
                            CtrlVPolicy::AlwaysLiteral => CtrlVPolicy::Smart,
                        };
                        changed = true;
                    }
                    ("set.widgets.enabled", _) => {
                        cfg.widgets.enabled = !cfg.widgets.enabled;
                        changed = true;
                    }
                    ("set.widgets.hot_reload", _) => {
                        cfg.widgets.hot_reload = !cfg.widgets.hot_reload;
                        changed = true;
                    }
                    ("set.bg.enabled", _) => {
                        cfg.background.enabled = !cfg.background.enabled;
                        changed = true;
                    }
                    ("set.bg.cycle_preset", _) => {
                        let idx = BackgroundPreset::ALL
                            .iter()
                            .position(|&p| p == cfg.background.preset)
                            .unwrap_or(0);
                        cfg.background.preset =
                            BackgroundPreset::ALL[(idx + 1) % BackgroundPreset::ALL.len()];
                        changed = true;
                    }
                    ("set.bg.cycle_fps", _) => {
                        cfg.background.fps = match cfg.background.fps {
                            15 => 30,
                            30 => 60,
                            _ => 15,
                        };
                        changed = true;
                    }
                    ("set.bg.mouse", _) => {
                        cfg.background.mouse_reactive = !cfg.background.mouse_reactive;
                        changed = true;
                    }
                    ("set.bg.audio", _) => {
                        cfg.background.audio_reactive = !cfg.background.audio_reactive;
                        changed = true;
                    }
                    ("set.bg.auto_quality", _) => {
                        cfg.background.auto_quality = !cfg.background.auto_quality;
                        changed = true;
                    }
                    ("set.bg.pause_unfocus", _) => {
                        cfg.background.pause_on_unfocus = !cfg.background.pause_on_unfocus;
                        changed = true;
                    }
                    ("set.bg.cycle_tui", _) => {
                        cfg.background.tui_behavior = match cfg.background.tui_behavior {
                            BackgroundTuiBehavior::Pause => BackgroundTuiBehavior::Hide,
                            BackgroundTuiBehavior::Hide => BackgroundTuiBehavior::Keep,
                            BackgroundTuiBehavior::Keep => BackgroundTuiBehavior::Pause,
                        };
                        changed = true;
                    }
                    ("set.media.enabled", _) => {
                        cfg.media.enabled = !cfg.media.enabled;
                        changed = true;
                    }
                    ("set.media.mpris", _) => {
                        cfg.media.mpris_enabled = !cfg.media.mpris_enabled;
                        changed = true;
                    }
                    ("set.media.player", _) => {
                        cfg.media.player_enabled = !cfg.media.player_enabled;
                        changed = true;
                    }
                    ("set.media.capture", _) => {
                        cfg.media.audio_capture_enabled = !cfg.media.audio_capture_enabled;
                        changed = true;
                    }
                    ("set.xray.enabled", _) => {
                        cfg.xray.enabled = !cfg.xray.enabled;
                        changed = true;
                    }
                    ("set.xray.compact", _) => {
                        cfg.xray.compact_indicator = !cfg.xray.compact_indicator;
                        changed = true;
                    }
                    ("set.xray.privacy", _) => {
                        cfg.xray.privacy_mode = !cfg.xray.privacy_mode;
                        changed = true;
                    }
                    ("set.xray.cycle_level", _) => {
                        cfg.xray.level = match cfg.xray.level {
                            XRayLevel::Light => XRayLevel::Inspect,
                            _ => XRayLevel::Light,
                        };
                        changed = true;
                    }
                    _ => {}
                }
                return (true, changed);
            }
        }
        (false, false)
    }
}

/// Emit alignment snap guides into the overlay scene while dragging in the Visual Editor.
pub fn emit_snap_guides(scene: &mut Scene, guides: &[SnapGuide], win_size: (f32, f32)) {
    for g in guides {
        if g.vertical {
            scene.overlays.push(SceneRect {
                x: g.position_px,
                y: 0.0,
                w: 1.5,
                h: win_size.1,
                color: [0.35, 0.85, 0.96, 0.65],
            });
        } else {
            scene.overlays.push(SceneRect {
                x: 0.0,
                y: g.position_px,
                w: win_size.0,
                h: 1.5,
                color: [0.35, 0.85, 0.96, 0.65],
            });
        }
    }
}

/// Emit the adaptive Visual Layout Editor inspector drawer (Catalog, Properties,
/// Layers, Profiles, Undo/Redo) alongside the live interactive terminal canvas.
#[allow(clippy::too_many_arguments)]
pub fn emit_visual_editor_panel(
    mgr: &WidgetManager,
    win_size: (f32, f32),
    scale: f32,
    pointer_px: (f32, f32),
    theme: &Theme,
    renderer: &mut Renderer,
    scene: &mut Scene,
    hits: &mut Vec<UiHitTarget>,
) {
    if !mgr.editor.active {
        return;
    }
    emit_snap_guides(scene, &mgr.editor.active_guides, win_size);

    let logical_w = win_size.0 / scale.max(0.25);
    let bp = ViewportBreakpoint::from_logical_width(logical_w);
    let (px, py, pw, ph) = if bp.is_compact() {
        let h = (230.0 * scale).min(win_size.1 * 0.48);
        (
            10.0 * scale,
            win_size.1 - h - 10.0 * scale,
            win_size.0 - 20.0 * scale,
            h,
        )
    } else {
        let w = (310.0 * scale).min(win_size.0 * 0.38);
        let h = (win_size.1 - 24.0 * scale).max(200.0);
        (12.0 * scale, 12.0 * scale, w, h)
    };

    let tabs = vec![
        "Catalog".into(),
        "Props".into(),
        "Layers".into(),
        "Profiles".into(),
    ];
    let active_tab = match mgr.editor.active_tab {
        EditorTab::Catalog => 0,
        EditorTab::Properties => 1,
        EditorTab::Layers => 2,
        EditorTab::Profiles | EditorTab::Background | EditorTab::XRay => 3,
    };

    let renders = mgr.render_snapshots();
    let mut content: Vec<UiNode> = Vec::new();
    match mgr.editor.active_tab {
        EditorTab::Catalog => {
            if mgr.extensions.is_empty() {
                content.push(UiNode::Text {
                    text: "No extensions loaded".into(),
                    size: Some(12.0),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
            }
            for (id, ext) in &mgr.extensions {
                let trust = mgr
                    .trust_levels
                    .get(id)
                    .copied()
                    .unwrap_or(ExtensionTrust::ThirdParty);
                let err = renders.get(id).and_then(|r| r.last_error.as_deref());
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Button {
                            id: format!("ed.select.{id}"),
                            label: ext.manifest.name.clone(),
                            bg: Some(UiColor::Token(
                                if mgr.editor.selected_widget.as_deref() == Some(id.as_str()) {
                                    "accent-soft"
                                } else {
                                    "surface"
                                }
                                .into(),
                            )),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(135.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                        UiNode::Badge {
                            text: if trust == ExtensionTrust::HostOfficial {
                                "Official".into()
                            } else {
                                format!("v{}", ext.manifest.version)
                            },
                            color: Some(UiColor::Token(
                                if err.is_some() { "error" } else { "accent" }.into(),
                            )),
                            bg: None,
                        },
                        UiNode::Button {
                            id: format!("ed.reload.{id}"),
                            label: "↻".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("muted".into())),
                            width: Some(26.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                });
                if let Some(e) = err {
                    content.push(UiNode::Text {
                        text: format!("JS error: {e}"),
                        size: Some(10.5),
                        color: Some(UiColor::Token("error".into())),
                        align: Align::Start,
                    });
                }
            }
        }
        EditorTab::Properties => {
            if let Some(sel) = &mgr.editor.selected_widget
                && let Some(cfg) = mgr.placements.get(sel)
            {
                content.push(UiNode::Text {
                    text: format!("Selected: {sel}"),
                    size: Some(12.0),
                    color: Some(UiColor::Token("accent".into())),
                    align: Align::Start,
                });
                content.push(UiNode::Text {
                    text: format!(
                        "Pos ({:.0}, {:.0})  Size {:.0}x{:.0}  Z:{}  Alpha:{:.0}%",
                        cfg.x,
                        cfg.y,
                        cfg.width,
                        cfg.height,
                        cfg.z_index,
                        cfg.opacity * 100.0
                    ),
                    size: Some(11.0),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Button {
                            id: "ed.prop.mode".into(),
                            label: format!("Mode: {:?}", cfg.mode),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(130.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                        UiNode::Button {
                            id: "ed.prop.anchor".into(),
                            label: format!("{:?}", cfg.anchor),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(110.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                });
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Button {
                            id: "ed.prop.tui".into(),
                            label: format!("TUI: {:?}", cfg.tui_behavior),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(118.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                        UiNode::Button {
                            id: "ed.prop.narrow".into(),
                            label: format!("Narrow: {:?}", cfg.narrow_behavior),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(122.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                });
                content.push(UiNode::Toggle {
                    id: "ed.prop.lock".into(),
                    label: "Lock position".into(),
                    checked: cfg.locked,
                    disabled: false,
                });
                content.push(UiNode::Toggle {
                    id: "ed.prop.click_through".into(),
                    label: "Click-through".into(),
                    checked: cfg.click_through,
                    disabled: false,
                });
                content.push(UiNode::Toggle {
                    id: "ed.prop.hover".into(),
                    label: "Show on hover".into(),
                    checked: cfg.show_on_hover,
                    disabled: false,
                });
                content.push(UiNode::Toggle {
                    id: "ed.prop.snap".into(),
                    label: "Snap to terminal grid".into(),
                    checked: cfg.snap_to_grid,
                    disabled: false,
                });
                content.push(UiNode::Toggle {
                    id: "ed.prop.avoid".into(),
                    label: "Avoid overlap with other widgets".into(),
                    checked: cfg.avoid_overlap,
                    disabled: false,
                });
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Opacity {:.0}%", cfg.opacity * 100.0),
                            size: Some(11.0),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Slider {
                            id: "ed.prop.opacity".into(),
                            value: cfg.opacity.clamp(0.1, 1.0),
                            width: Some(130.0),
                            height: Some(14.0),
                            color: Some(UiColor::Token("accent".into())),
                            track: None,
                        },
                    ],
                });
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Button {
                            id: "ed.prop.w_dec".into(),
                            label: "W -".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(42.0),
                            height: Some(22.0),
                            radius: 5.0,
                        },
                        UiNode::Button {
                            id: "ed.prop.w_inc".into(),
                            label: "W +".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(42.0),
                            height: Some(22.0),
                            radius: 5.0,
                        },
                        UiNode::Button {
                            id: "ed.prop.h_dec".into(),
                            label: "H -".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(42.0),
                            height: Some(22.0),
                            radius: 5.0,
                        },
                        UiNode::Button {
                            id: "ed.prop.h_inc".into(),
                            label: "H +".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(42.0),
                            height: Some(22.0),
                            radius: 5.0,
                        },
                    ],
                });
                // Auto-generated extension settings_schema controls
                if let Some(ext) = mgr.extensions.get(sel)
                    && !ext.manifest.settings_schema.is_empty()
                {
                    content.push(UiNode::Divider {
                        vertical: false,
                        color: None,
                    });
                    let cur_vals = mgr.extension_settings.get(sel);
                    for field in &ext.manifest.settings_schema {
                        let lbl = if field.label.is_empty() {
                            field.key.clone()
                        } else {
                            field.label.clone()
                        };
                        match field.kind {
                            super::manifest::SettingFieldKind::Boolean => {
                                let checked = cur_vals
                                    .and_then(|v| v.get(&field.key))
                                    .and_then(|v| v.as_bool())
                                    .or_else(|| field.default.as_ref().and_then(|v| v.as_bool()))
                                    .unwrap_or(false);
                                content.push(UiNode::Toggle {
                                    id: format!("ed.schema.bool.{}", field.key),
                                    label: lbl,
                                    checked,
                                    disabled: false,
                                });
                            }
                            super::manifest::SettingFieldKind::Enum
                                if !field.options.is_empty() =>
                            {
                                let cur = cur_vals
                                    .and_then(|v| v.get(&field.key))
                                    .and_then(|v| v.as_str())
                                    .or_else(|| field.default.as_ref().and_then(|v| v.as_str()))
                                    .unwrap_or(field.options[0].as_str());
                                content.push(UiNode::Button {
                                    id: format!("ed.schema.enum.{}", field.key),
                                    label: format!("{lbl}: {cur}"),
                                    bg: Some(UiColor::Token("surface".into())),
                                    color: Some(UiColor::Token("accent".into())),
                                    width: Some(240.0),
                                    height: Some(24.0),
                                    radius: 6.0,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            } else {
                content.push(UiNode::Text {
                    text: "Click a widget to edit its properties".into(),
                    size: Some(12.0),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
            }
        }
        EditorTab::Layers => {
            let mut layers: Vec<(&String, &super::layout::WidgetPlacementConfig)> =
                mgr.placements.iter().collect();
            layers.sort_by_key(|(_, c)| -c.z_index);
            for (id, cfg) in layers {
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Z{:+} {id}", cfg.z_index),
                            size: Some(11.5),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Button {
                            id: format!("ed.zup.{id}"),
                            label: "▲".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("accent".into())),
                            width: Some(26.0),
                            height: Some(22.0),
                            radius: 5.0,
                        },
                        UiNode::Button {
                            id: format!("ed.zdn.{id}"),
                            label: "▼".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("muted".into())),
                            width: Some(26.0),
                            height: Some(22.0),
                            radius: 5.0,
                        },
                    ],
                });
            }
        }
        EditorTab::Profiles | EditorTab::Background | EditorTab::XRay => {
            content.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Button {
                        id: "ed.profile.new".into(),
                        label: "+ New".into(),
                        bg: Some(UiColor::Token("accent-soft".into())),
                        color: Some(UiColor::Token("accent".into())),
                        width: Some(64.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                    UiNode::Button {
                        id: "ed.profile.dup".into(),
                        label: "Duplicate".into(),
                        bg: Some(UiColor::Token("surface".into())),
                        color: Some(UiColor::Token("fg".into())),
                        width: Some(78.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                    UiNode::Button {
                        id: "ed.profile.save".into(),
                        label: "Save".into(),
                        bg: Some(UiColor::Token("surface".into())),
                        color: Some(UiColor::Token("success".into())),
                        width: Some(58.0),
                        height: Some(24.0),
                        radius: 6.0,
                    },
                ],
            });
            for name in mgr.profiles.profiles.keys() {
                let is_active = mgr.profiles.active == *name;
                let mut row_items = vec![UiNode::Button {
                    id: format!("ed.profile.{name}"),
                    label: if is_active {
                        format!("● {name} (active)")
                    } else {
                        name.clone()
                    },
                    bg: Some(UiColor::Token(
                        if is_active { "accent-soft" } else { "surface" }.into(),
                    )),
                    color: Some(UiColor::Token(
                        if is_active { "accent" } else { "fg" }.into(),
                    )),
                    width: Some(196.0),
                    height: Some(24.0),
                    radius: 6.0,
                }];
                if name != "default" {
                    row_items.push(UiNode::Button {
                        id: format!("ed.profile.del.{name}"),
                        label: "✕".into(),
                        bg: Some(UiColor::Token("surface".into())),
                        color: Some(UiColor::Token("error".into())),
                        width: Some(26.0),
                        height: Some(24.0),
                        radius: 6.0,
                    });
                }
                content.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: row_items,
                });
            }
        }
    }

    let mut root_children = vec![
        UiNode::Row {
            gap: 6.0,
            align: Align::Center,
            children: vec![
                UiNode::Text {
                    text: "Layout Editor".into(),
                    size: Some(14.0),
                    color: Some(UiColor::Token("fg".into())),
                    align: Align::Start,
                },
                UiNode::Button {
                    id: "ed.undo".into(),
                    label: "↶".into(),
                    bg: Some(UiColor::Token("surface".into())),
                    color: Some(UiColor::Token("fg".into())),
                    width: Some(26.0),
                    height: Some(22.0),
                    radius: 5.0,
                },
                UiNode::Button {
                    id: "ed.redo".into(),
                    label: "↷".into(),
                    bg: Some(UiColor::Token("surface".into())),
                    color: Some(UiColor::Token("fg".into())),
                    width: Some(26.0),
                    height: Some(22.0),
                    radius: 5.0,
                },
                UiNode::Button {
                    id: "ed.close".into(),
                    label: "Done".into(),
                    bg: Some(UiColor::Token("accent-soft".into())),
                    color: Some(UiColor::Token("accent".into())),
                    width: Some(50.0),
                    height: Some(22.0),
                    radius: 5.0,
                },
            ],
        },
        UiNode::Tabs {
            id: "ed.tabs".into(),
            tabs,
            active: active_tab,
        },
        UiNode::Divider {
            vertical: false,
            color: None,
        },
    ];
    root_children.extend(content);

    let panel = UiNode::Container {
        padding: 12.0,
        bg: Some(UiColor::Token("surface-elevated".into())),
        border: Some(UiColor::Token("border".into())),
        radius: 10.0,
        width: None,
        height: None,
        child: Box::new(UiNode::Column {
            gap: 8.0,
            align: Align::Stretch,
            children: root_children,
        }),
    };

    emit_ui_tree_interactive(
        "beyrix-visual-editor",
        &panel,
        (px, py, pw, ph),
        scale,
        0.96,
        UiLayer::Overlay,
        Some(pointer_px),
        theme,
        renderer,
        scene,
        hits,
    );
}

pub fn cycle_placement_mode(mode: PlacementMode) -> PlacementMode {
    match mode {
        PlacementMode::Overlay => PlacementMode::SmartOverlay,
        PlacementMode::SmartOverlay => PlacementMode::Dock,
        PlacementMode::Dock => PlacementMode::Behind,
        PlacementMode::Behind => PlacementMode::Inline,
        PlacementMode::Inline => PlacementMode::Hidden,
        PlacementMode::Hidden | PlacementMode::Float => PlacementMode::Overlay,
    }
}

pub fn cycle_anchor(anchor: Anchor) -> Anchor {
    match anchor {
        Anchor::TopLeft => Anchor::TopCenter,
        Anchor::TopCenter => Anchor::TopRight,
        Anchor::TopRight => Anchor::CenterLeft,
        Anchor::CenterLeft => Anchor::Center,
        Anchor::Center => Anchor::CenterRight,
        Anchor::CenterRight => Anchor::BottomLeft,
        Anchor::BottomLeft => Anchor::BottomCenter,
        Anchor::BottomCenter => Anchor::BottomRight,
        Anchor::BottomRight => Anchor::TopLeft,
    }
}

pub fn cycle_tui_behavior(b: TuiBehavior) -> TuiBehavior {
    match b {
        TuiBehavior::Overlay => TuiBehavior::Behind,
        TuiBehavior::Behind => TuiBehavior::Hidden,
        TuiBehavior::Hidden => TuiBehavior::Overlay,
    }
}

pub fn cycle_narrow_behavior(b: NarrowBehavior) -> NarrowBehavior {
    match b {
        NarrowBehavior::Compact => NarrowBehavior::Shrink,
        NarrowBehavior::Shrink => NarrowBehavior::Hide,
        NarrowBehavior::Hide => NarrowBehavior::Compact,
    }
}
