// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Beyrix Studio (Visual Workspace & Layout Editor) and Unified Settings Overlay.
//!
//! Renders completely natively via `UiNode` -> `Scene`, using opaque design-system
//! surfaces (zero terminal text bleed-through), proportional UI typography, vector
//! icons, real input controls (TextInput, Dropdown, Slider, Toggle), and responsive
//! layouts from 320px micro windows up to 1600px+ wide displays.

use crate::config::{
    BackgroundFitMode, BackgroundPreset, BackgroundTuiBehavior, CtrlVPolicy, PlatformConfig,
    XRayLevel,
};
use crate::render::{Renderer, Scene, SceneRect};
use crate::theme::Theme;

use super::layout::{
    Anchor, NarrowBehavior, NarrowStudioPane, PlacementMode, SnapGuide, StudioLeftTab, TuiBehavior,
    ViewportBreakpoint,
};
use super::ui::{Align, HitKind, UiColor, UiHitTarget, UiLayer, UiNode, emit_ui_tree_interactive};
use super::{ExtensionTrust, WidgetManager};

pub const STUDIO_TARGET_BACKGROUND: &str = "__beyrix_background__";
pub const STUDIO_TARGET_XRAY: &str = "__beyrix_xray__";

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
            Self::MediaAudio => "Media",
            Self::XRay => "X-Ray",
            Self::PrivacyPerf => "Privacy",
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
    pub panel_rect: Option<(f32, f32, f32, f32)>,
}

impl SettingsOverlay {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.status_message = None;
            self.hits.clear();
            self.panel_rect = None;
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
        self.panel_rect = None;
        if !self.open {
            return;
        }
        let logical_w = win_size.0 / scale.max(0.25);
        let bp = ViewportBreakpoint::from_logical_width(logical_w);
        let margin = if bp.is_compact() { 10.0 } else { 20.0 } * scale;
        let panel_w = if bp.is_compact() {
            (win_size.0 - margin * 2.0).max(260.0 * scale).min(win_size.0)
        } else {
            (640.0 * scale).min(win_size.0 - margin * 2.0)
        };
        let panel_h = (460.0 * scale).min((win_size.1 - margin * 2.0).max(220.0 * scale));
        let px = ((win_size.0 - panel_w) * 0.5).max(0.0);
        let py = ((win_size.1 - panel_h) * 0.5).max(0.0);
        self.panel_rect = Some((px, py, panel_w, panel_h));

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
                    gap: 10.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: "Ctrl+V policy".into(),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "set.kbd.cycle_ctrl_v".into(),
                            options: vec![
                                "Smart".into(),
                                "Always Paste".into(),
                                "Always Literal".into(),
                            ],
                            selected: match cfg.keyboard.ctrl_v_policy {
                                CtrlVPolicy::Smart => 0,
                                CtrlVPolicy::AlwaysPaste => 1,
                                CtrlVPolicy::AlwaysLiteral => 2,
                            },
                            disabled: false,
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
                        "Profile: {}  ·  Memory cap: {} MB  ·  Timeout: {} ms  ·  Studio: {}",
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
                            text: "Preset".into(),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "set.bg.cycle_preset".into(),
                            options: BackgroundPreset::ALL
                                .iter()
                                .map(|p| p.name().to_string())
                                .collect(),
                            selected: BackgroundPreset::ALL
                                .iter()
                                .position(|&p| p == cfg.background.preset)
                                .unwrap_or(0),
                            disabled: false,
                        },
                        UiNode::Dropdown {
                            id: "set.bg.cycle_fps".into(),
                            options: vec!["15 FPS".into(), "30 FPS".into(), "60 FPS".into()],
                            selected: match cfg.background.fps {
                                15 => 0,
                                60 => 2,
                                _ => 1,
                            },
                            disabled: false,
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
                            text: "Diagnostic level".into(),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "set.xray.cycle_level".into(),
                            options: vec!["Light".into(), "Inspect".into()],
                            selected: usize::from(cfg.xray.level != XRayLevel::Light),
                            disabled: false,
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
                            text: "Background in TUI".into(),
                            size: Some(12.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "set.bg.cycle_tui".into(),
                            options: vec!["Pause".into(), "Hide".into(), "Keep".into()],
                            selected: match cfg.background.tui_behavior {
                                BackgroundTuiBehavior::Pause => 0,
                                BackgroundTuiBehavior::Hide => 1,
                                BackgroundTuiBehavior::Keep => 2,
                            },
                            disabled: false,
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
                    UiNode::Icon {
                        name: "settings".into(),
                        size: Some(16.0),
                        color: Some(UiColor::Token("accent".into())),
                    },
                    UiNode::Text {
                        text: "Beyrix Settings".into(),
                        size: Some(15.0),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                    UiNode::Button {
                        id: "set.save".into(),
                        label: "Save".into(),
                        bg: Some(UiColor::Token("accent-soft".into())),
                        color: Some(UiColor::Token("accent".into())),
                        width: Some(62.0),
                        height: Some(26.0),
                        radius: 6.0,
                    },
                    UiNode::Button {
                        id: "set.reset".into(),
                        label: "Reset".into(),
                        bg: Some(UiColor::Token("surface-control".into())),
                        color: Some(UiColor::Token("warning".into())),
                        width: Some(60.0),
                        height: Some(26.0),
                        radius: 6.0,
                    },
                    UiNode::Button {
                        id: "set.close".into(),
                        label: "✕".into(),
                        bg: Some(UiColor::Token("surface-control".into())),
                        color: Some(UiColor::Token("muted".into())),
                        width: Some(28.0),
                        height: Some(26.0),
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
        rows.push(UiNode::Container {
            padding: 12.0,
            bg: Some(UiColor::Token("surface-card".into())),
            border: Some(UiColor::Token("border-subtle".into())),
            radius: 10.0,
            width: None,
            height: None,
            child: Box::new(UiNode::Column {
                gap: 10.0,
                align: Align::Stretch,
                children: body_children,
            }),
        });
        if let Some(msg) = &self.status_message {
            rows.push(UiNode::Text {
                text: msg.clone(),
                size: Some(11.5),
                color: Some(UiColor::Token("success".into())),
                align: Align::Start,
            });
        }

        let tree = UiNode::Container {
            padding: 16.0,
            bg: Some(UiColor::Token("surface-panel".into())),
            border: Some(UiColor::Token("border-strong".into())),
            radius: 12.0,
            width: None,
            height: None,
            child: Box::new(UiNode::Column {
                gap: 10.0,
                align: Align::Stretch,
                children: rows,
            }),
        };

        // Solid opacity=1.0 on UiLayer::Panel so neither terminal text nor widgets bleed through Settings.
        emit_ui_tree_interactive(
            "beyrix-settings",
            &tree,
            (px, py, panel_w, panel_h),
            scale,
            1.0,
            UiLayer::Panel,
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
        // Any click inside the panel bounding box is consumed so it never leaks to terminal.
        if let Some((px, py, pw, ph)) = self.panel_rect
            && pointer_px.0 >= px
            && pointer_px.0 <= px + pw
            && pointer_px.1 >= py
            && pointer_px.1 <= py + ph
        {
            return (true, false);
        }
        (false, false)
    }
}

/// Emit alignment snap guides into the overlay scene while dragging in Beyrix Studio.
pub fn emit_snap_guides(scene: &mut Scene, guides: &[SnapGuide], win_size: (f32, f32)) {
    for g in guides {
        if g.vertical {
            scene.overlays.push(SceneRect {
                x: g.position_px,
                y: 0.0,
                w: 1.5,
                h: win_size.1,
                color: [0.35, 0.85, 0.96, 0.70],
            });
        } else {
            scene.overlays.push(SceneRect {
                x: 0.0,
                y: g.position_px,
                w: win_size.0,
                h: 1.5,
                color: [0.35, 0.85, 0.96, 0.70],
            });
        }
    }
}

/// Emit the full Beyrix Studio workspace editor.
///
/// - Wide windows (>= 960 logical px): Top toolbar + Left Catalog/Layers sidebar
///   + Center Live Preview + Right Property Inspector.
/// - Narrow windows (< 960 logical px): Compact top toolbar with Preview /
///   Catalog / Inspector switcher and full-width collapsible drawer.
#[allow(clippy::too_many_arguments)]
pub fn emit_visual_editor_panel(
    mgr: &WidgetManager,
    platform_cfg: Option<&PlatformConfig>,
    win_size: (f32, f32),
    scale: f32,
    pointer_px: (f32, f32),
    theme: &Theme,
    renderer: &mut Renderer,
    scene: &mut Scene,
    hits: &mut Vec<UiHitTarget>,
    panel_rects: &mut Vec<(f32, f32, f32, f32)>,
) {
    if !mgr.editor.active {
        return;
    }
    emit_snap_guides(scene, &mgr.editor.active_guides, win_size);

    let logical_w = win_size.0 / scale.max(0.25);
    let is_wide = logical_w >= 960.0;
    let pad = 8.0 * scale;
    let top_h = 44.0 * scale;

    // 1. Top Toolbar (profiles, undo/redo, save, exit)
    let profile_names: Vec<String> = mgr.profiles.profiles.keys().cloned().collect();
    let active_prof_idx = profile_names
        .iter()
        .position(|n| n == &mgr.profiles.active)
        .unwrap_or(0);

    let mut top_items = vec![UiNode::Icon {
        name: "layers".into(),
        size: Some(15.0),
        color: Some(UiColor::Token("accent".into())),
    }];
    if logical_w >= 620.0 {
        top_items.push(UiNode::Text {
            text: "Beyrix Studio".into(),
            size: Some(13.5),
            color: Some(UiColor::Token("fg".into())),
            align: Align::Start,
        });
    }
    if is_wide {
        top_items.push(UiNode::Dropdown {
            id: "ed.profile.dropdown".into(),
            options: profile_names.clone(),
            selected: active_prof_idx,
            disabled: false,
        });
        top_items.push(UiNode::Button {
            id: "ed.profile.new".into(),
            label: "+ New".into(),
            bg: Some(UiColor::Token("surface-control".into())),
            color: Some(UiColor::Token("fg".into())),
            width: Some(58.0),
            height: Some(26.0),
            radius: 6.0,
        });
        top_items.push(UiNode::Button {
            id: "ed.profile.dup".into(),
            label: "Dup".into(),
            bg: Some(UiColor::Token("surface-control".into())),
            color: Some(UiColor::Token("fg".into())),
            width: Some(46.0),
            height: Some(26.0),
            radius: 6.0,
        });
        top_items.push(UiNode::Button {
            id: "ed.profile.save".into(),
            label: "Save".into(),
            bg: Some(UiColor::Token("surface-control".into())),
            color: Some(UiColor::Token("success".into())),
            width: Some(52.0),
            height: Some(26.0),
            radius: 6.0,
        });
    } else {
        top_items.push(UiNode::Tabs {
            id: "ed.narrow.pane".into(),
            tabs: vec!["Preview".into(), "Catalog".into(), "Inspect".into()],
            active: match mgr.editor.narrow_pane {
                NarrowStudioPane::Preview => 0,
                NarrowStudioPane::Catalog => 1,
                NarrowStudioPane::Inspector => 2,
            },
        });
    }
    top_items.push(UiNode::Button {
        id: "ed.undo".into(),
        label: "↶".into(),
        bg: Some(UiColor::Token("surface-control".into())),
        color: Some(UiColor::Token(
            if mgr.editor.undo_stack.is_empty() {
                "muted"
            } else {
                "fg"
            }
            .into(),
        )),
        width: Some(28.0),
        height: Some(26.0),
        radius: 6.0,
    });
    top_items.push(UiNode::Button {
        id: "ed.redo".into(),
        label: "↷".into(),
        bg: Some(UiColor::Token("surface-control".into())),
        color: Some(UiColor::Token(
            if mgr.editor.redo_stack.is_empty() {
                "muted"
            } else {
                "fg"
            }
            .into(),
        )),
        width: Some(28.0),
        height: Some(26.0),
        radius: 6.0,
    });
    top_items.push(UiNode::Button {
        id: "ed.close".into(),
        label: "Done".into(),
        bg: Some(UiColor::Token("accent-soft".into())),
        color: Some(UiColor::Token("accent".into())),
        width: Some(54.0),
        height: Some(26.0),
        radius: 6.0,
    });

    let top_rect = (pad, pad, (win_size.0 - pad * 2.0).max(200.0 * scale), top_h);
    let top_bar = UiNode::Container {
        padding: 8.0,
        bg: Some(UiColor::Token("surface-panel".into())),
        border: Some(UiColor::Token("border-strong".into())),
        radius: 10.0,
        width: None,
        height: None,
        child: Box::new(UiNode::Row {
            gap: 8.0,
            align: Align::Center,
            children: top_items,
        }),
    };
    panel_rects.push(top_rect);
    emit_ui_tree_interactive(
        "beyrix-studio-topbar",
        &top_bar,
        top_rect,
        scale,
        1.0,
        UiLayer::Panel,
        Some(pointer_px),
        theme,
        renderer,
        scene,
        hits,
    );

    let body_y = top_rect.1 + top_rect.3 + pad;
    let body_h = (win_size.1 - body_y - pad).max(120.0 * scale);

    if is_wide {
        let left_w = (248.0 * scale).min(win_size.0 * 0.25);
        let right_w = (276.0 * scale).min(win_size.0 * 0.28);
        let left_rect = (pad, body_y, left_w, body_h);
        let right_rect = ((win_size.0 - pad - right_w).max(pad), body_y, right_w, body_h);

        let left_panel = build_catalog_and_layers_panel(mgr, platform_cfg, false);
        panel_rects.push(left_rect);
        emit_ui_tree_interactive(
            "beyrix-studio-left",
            &left_panel,
            left_rect,
            scale,
            1.0,
            UiLayer::Panel,
            Some(pointer_px),
            theme,
            renderer,
            scene,
            hits,
        );

        let right_panel = build_inspector_panel(mgr, platform_cfg);
        panel_rects.push(right_rect);
        emit_ui_tree_interactive(
            "beyrix-studio-right",
            &right_panel,
            right_rect,
            scale,
            1.0,
            UiLayer::Panel,
            Some(pointer_px),
            theme,
            renderer,
            scene,
            hits,
        );
    } else {
        let drawer_rect = (pad, body_y, (win_size.0 - pad * 2.0).max(200.0 * scale), body_h);
        match mgr.editor.narrow_pane {
            NarrowStudioPane::Preview => {}
            NarrowStudioPane::Catalog => {
                let panel = build_catalog_and_layers_panel(mgr, platform_cfg, true);
                panel_rects.push(drawer_rect);
                emit_ui_tree_interactive(
                    "beyrix-studio-left",
                    &panel,
                    drawer_rect,
                    scale,
                    1.0,
                    UiLayer::Panel,
                    Some(pointer_px),
                    theme,
                    renderer,
                    scene,
                    hits,
                );
            }
            NarrowStudioPane::Inspector => {
                let panel = build_inspector_panel(mgr, platform_cfg);
                panel_rects.push(drawer_rect);
                emit_ui_tree_interactive(
                    "beyrix-studio-right",
                    &panel,
                    drawer_rect,
                    scale,
                    1.0,
                    UiLayer::Panel,
                    Some(pointer_px),
                    theme,
                    renderer,
                    scene,
                    hits,
                );
            }
        }
    }
}

fn build_catalog_and_layers_panel(
    mgr: &WidgetManager,
    platform_cfg: Option<&PlatformConfig>,
    include_profile_picker: bool,
) -> UiNode {
    let mut rows = vec![UiNode::Tabs {
        id: "ed.left.tabs".into(),
        tabs: vec!["Catalog".into(), "Layers".into()],
        active: match mgr.editor.left_tab {
            StudioLeftTab::Catalog => 0,
            StudioLeftTab::Layers => 1,
        },
    }];

    if include_profile_picker {
        let profile_names: Vec<String> = mgr.profiles.profiles.keys().cloned().collect();
        let active_prof_idx = profile_names
            .iter()
            .position(|n| n == &mgr.profiles.active)
            .unwrap_or(0);
        rows.push(UiNode::Row {
            gap: 6.0,
            align: Align::Center,
            children: vec![
                UiNode::Dropdown {
                    id: "ed.profile.dropdown".into(),
                    options: profile_names,
                    selected: active_prof_idx,
                    disabled: false,
                },
                UiNode::Button {
                    id: "ed.profile.save".into(),
                    label: "Save".into(),
                    bg: Some(UiColor::Token("surface-control".into())),
                    color: Some(UiColor::Token("success".into())),
                    width: Some(50.0),
                    height: Some(26.0),
                    radius: 6.0,
                },
            ],
        });
    }

    rows.push(UiNode::Divider {
        vertical: false,
        color: None,
    });

    match mgr.editor.left_tab {
        StudioLeftTab::Catalog => {
            // Built-in workspace subsystems (Background Engine & X-Ray)
            rows.push(UiNode::Text {
                text: "SYSTEM MODULES".into(),
                size: Some(10.0),
                color: Some(UiColor::Token("muted".into())),
                align: Align::Start,
            });
            let bg_on = platform_cfg.is_some_and(|c| c.background.enabled);
            let xray_on = platform_cfg.is_some_and(|c| c.xray.enabled);
            for (target_id, label, icon, active) in [
                (STUDIO_TARGET_BACKGROUND, "Background Engine", "sparkles", bg_on),
                (STUDIO_TARGET_XRAY, "X-Ray Diagnostics", "xray", xray_on),
            ] {
                let selected = mgr.editor.selected_widget.as_deref() == Some(target_id);
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Icon {
                            name: icon.into(),
                            size: Some(13.0),
                            color: Some(UiColor::Token(
                                if active { "accent" } else { "muted" }.into(),
                            )),
                        },
                        UiNode::Button {
                            id: format!("ed.select.{target_id}"),
                            label: label.into(),
                            bg: Some(UiColor::Token(
                                if selected {
                                    "accent-soft"
                                } else {
                                    "surface-card"
                                }
                                .into(),
                            )),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(144.0),
                            height: Some(26.0),
                            radius: 6.0,
                        },
                        UiNode::Badge {
                            text: if active { "On" } else { "Off" }.into(),
                            color: Some(UiColor::Token(
                                if active { "success" } else { "muted" }.into(),
                            )),
                            bg: None,
                        },
                    ],
                });
            }

            rows.push(UiNode::Divider {
                vertical: false,
                color: None,
            });
            rows.push(UiNode::Text {
                text: format!("WIDGET EXTENSIONS ({})", mgr.extensions.len()),
                size: Some(10.0),
                color: Some(UiColor::Token("muted".into())),
                align: Align::Start,
            });

            let renders = mgr.render_snapshots();
            if mgr.extensions.is_empty() {
                rows.push(UiNode::Text {
                    text: "No extensions loaded".into(),
                    size: Some(11.5),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
            }
            for (id, ext) in &mgr.extensions {
                let enabled = mgr.is_extension_enabled(id);
                let trust = mgr
                    .trust_levels
                    .get(id)
                    .copied()
                    .unwrap_or(ExtensionTrust::ThirdParty);
                let err = renders.get(id).and_then(|r| r.last_error.as_deref());
                let selected = mgr.editor.selected_widget.as_deref() == Some(id.as_str());
                rows.push(UiNode::Row {
                    gap: 5.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Button {
                            id: format!("ed.select.{id}"),
                            label: ext.manifest.name.clone(),
                            bg: Some(UiColor::Token(
                                if selected {
                                    "accent-soft"
                                } else {
                                    "surface-card"
                                }
                                .into(),
                            )),
                            color: Some(UiColor::Token(
                                if enabled { "fg" } else { "muted" }.into(),
                            )),
                            width: Some(124.0),
                            height: Some(26.0),
                            radius: 6.0,
                        },
                        UiNode::Button {
                            id: format!("ed.toggle.{id}"),
                            label: if enabled { "On" } else { "Off" }.into(),
                            bg: Some(UiColor::Token(
                                if enabled {
                                    "accent-soft"
                                } else {
                                    "surface-control"
                                }
                                .into(),
                            )),
                            color: Some(UiColor::Token(
                                if enabled { "accent" } else { "muted" }.into(),
                            )),
                            width: Some(36.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                        UiNode::Button {
                            id: format!("ed.reload.{id}"),
                            label: "↻".into(),
                            bg: Some(UiColor::Token("surface-control".into())),
                            color: Some(UiColor::Token(
                                if err.is_some() { "error" } else { "muted" }.into(),
                            )),
                            width: Some(26.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                    ],
                });
                let subtitle = if let Some(e) = err {
                    format!("Runtime error: {e}")
                } else if trust == ExtensionTrust::HostOfficial {
                    format!("Official · v{}", ext.manifest.version)
                } else {
                    format!("Third-party · v{}", ext.manifest.version)
                };
                rows.push(UiNode::Text {
                    text: subtitle,
                    size: Some(10.0),
                    color: Some(UiColor::Token(
                        if err.is_some() { "error" } else { "muted" }.into(),
                    )),
                    align: Align::Start,
                });
            }

            if !mgr.load_errors.is_empty() {
                rows.push(UiNode::Divider {
                    vertical: false,
                    color: None,
                });
                rows.push(UiNode::Text {
                    text: format!("LOAD ERRORS ({})", mgr.load_errors.len()),
                    size: Some(10.0),
                    color: Some(UiColor::Token("error".into())),
                    align: Align::Start,
                });
                for err in &mgr.load_errors {
                    rows.push(UiNode::Container {
                        padding: 6.0,
                        bg: Some(UiColor::Token("surface-card".into())),
                        border: Some(UiColor::Token("error".into())),
                        radius: 6.0,
                        width: None,
                        height: None,
                        child: Box::new(UiNode::Column {
                            gap: 2.0,
                            align: Align::Stretch,
                            children: vec![
                                UiNode::Row {
                                    gap: 5.0,
                                    align: Align::Center,
                                    children: vec![
                                        UiNode::Icon {
                                            name: "error".into(),
                                            size: Some(12.0),
                                            color: Some(UiColor::Token("error".into())),
                                        },
                                        UiNode::Text {
                                            text: err.dir_name.clone(),
                                            size: Some(11.0),
                                            color: Some(UiColor::Token("error".into())),
                                            align: Align::Start,
                                        },
                                    ],
                                },
                                UiNode::Text {
                                    text: err.error.clone(),
                                    size: Some(10.0),
                                    color: Some(UiColor::Token("muted".into())),
                                    align: Align::Start,
                                },
                            ],
                        }),
                    });
                }
            }
        }
        StudioLeftTab::Layers => {
            rows.push(UiNode::Text {
                text: "Z-ORDER LAYERS".into(),
                size: Some(10.0),
                color: Some(UiColor::Token("muted".into())),
                align: Align::Start,
            });
            let mut layers: Vec<(&String, &super::layout::WidgetPlacementConfig)> =
                mgr.placements.iter().collect();
            layers.sort_by_key(|(_, c)| -c.z_index);
            for (id, cfg) in layers {
                let name = mgr
                    .extensions
                    .get(id)
                    .map_or(id.as_str(), |e| e.manifest.name.as_str());
                let selected = mgr.editor.selected_widget.as_deref() == Some(id.as_str());
                rows.push(UiNode::Row {
                    gap: 5.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Badge {
                            text: format!("Z{}", cfg.z_index),
                            color: Some(UiColor::Token("muted".into())),
                            bg: None,
                        },
                        UiNode::Button {
                            id: format!("ed.select.{id}"),
                            label: name.into(),
                            bg: Some(UiColor::Token(
                                if selected {
                                    "accent-soft"
                                } else {
                                    "surface-card"
                                }
                                .into(),
                            )),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(104.0),
                            height: Some(24.0),
                            radius: 6.0,
                        },
                        UiNode::Button {
                            id: format!("ed.zup.{id}"),
                            label: "▲".into(),
                            bg: Some(UiColor::Token("surface-control".into())),
                            color: Some(UiColor::Token("accent".into())),
                            width: Some(24.0),
                            height: Some(24.0),
                            radius: 5.0,
                        },
                        UiNode::Button {
                            id: format!("ed.zdn.{id}"),
                            label: "▼".into(),
                            bg: Some(UiColor::Token("surface-control".into())),
                            color: Some(UiColor::Token("muted".into())),
                            width: Some(24.0),
                            height: Some(24.0),
                            radius: 5.0,
                        },
                    ],
                });
            }
        }
    }

    UiNode::Container {
        padding: 12.0,
        bg: Some(UiColor::Token("surface-panel".into())),
        border: Some(UiColor::Token("border-strong".into())),
        radius: 10.0,
        width: None,
        height: None,
        child: Box::new(UiNode::Column {
            gap: 8.0,
            align: Align::Stretch,
            children: rows,
        }),
    }
}

fn build_inspector_panel(
    mgr: &WidgetManager,
    platform_cfg: Option<&PlatformConfig>,
) -> UiNode {
    let mut rows: Vec<UiNode> = Vec::new();
    let sel = mgr.editor.selected_widget.as_deref();

    match sel {
        Some(STUDIO_TARGET_BACKGROUND) => {
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Icon {
                        name: "sparkles".into(),
                        size: Some(15.0),
                        color: Some(UiColor::Token("accent".into())),
                    },
                    UiNode::Text {
                        text: "Background Engine".into(),
                        size: Some(13.5),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                ],
            });
            rows.push(UiNode::Divider {
                vertical: false,
                color: None,
            });
            if let Some(cfg) = platform_cfg {
                rows.push(UiNode::Toggle {
                    id: "ed.bg.enabled".into(),
                    label: "Enable 3D Background".into(),
                    checked: cfg.background.enabled,
                    disabled: false,
                });
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: "Preset".into(),
                            size: Some(11.5),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "ed.bg.preset".into(),
                            options: BackgroundPreset::ALL
                                .iter()
                                .map(|p| p.name().to_string())
                                .collect(),
                            selected: BackgroundPreset::ALL
                                .iter()
                                .position(|&p| p == cfg.background.preset)
                                .unwrap_or(0),
                            disabled: false,
                        },
                    ],
                });
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: "Fit mode".into(),
                            size: Some(11.5),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "ed.bg.fit".into(),
                            options: vec![
                                "Fit".into(),
                                "Fill".into(),
                                "FixedScale".into(),
                                "Responsive".into(),
                            ],
                            selected: match cfg.background.fit_mode {
                                BackgroundFitMode::Fit => 0,
                                BackgroundFitMode::Fill => 1,
                                BackgroundFitMode::FixedScale => 2,
                                BackgroundFitMode::Responsive => 3,
                            },
                            disabled: false,
                        },
                    ],
                });
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Alpha {:.0}%", cfg.background.alpha * 100.0),
                            size: Some(11.0),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Slider {
                            id: "ed.bg.alpha".into(),
                            value: cfg.background.alpha.clamp(0.05, 1.0),
                            width: Some(120.0),
                            height: Some(14.0),
                            color: Some(UiColor::Token("accent".into())),
                            track: None,
                        },
                    ],
                });
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!("Speed {:.1}x", cfg.background.speed),
                            size: Some(11.0),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Slider {
                            id: "ed.bg.speed".into(),
                            value: (cfg.background.speed / 3.0).clamp(0.05, 1.0),
                            width: Some(120.0),
                            height: Some(14.0),
                            color: Some(UiColor::Token("cyan".into())),
                            track: None,
                        },
                    ],
                });
                rows.push(UiNode::Toggle {
                    id: "ed.bg.mouse".into(),
                    label: "Mouse-reactive camera".into(),
                    checked: cfg.background.mouse_reactive,
                    disabled: false,
                });
                rows.push(UiNode::Toggle {
                    id: "ed.bg.audio".into(),
                    label: "Audio-reactive bindings".into(),
                    checked: cfg.background.audio_reactive,
                    disabled: false,
                });
                rows.push(UiNode::Toggle {
                    id: "ed.bg.auto_quality".into(),
                    label: "Auto-quality throttling".into(),
                    checked: cfg.background.auto_quality,
                    disabled: false,
                });
            }
        }
        Some(STUDIO_TARGET_XRAY) => {
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Icon {
                        name: "xray".into(),
                        size: Some(15.0),
                        color: Some(UiColor::Token("cyan".into())),
                    },
                    UiNode::Text {
                        text: "X-Ray Diagnostics".into(),
                        size: Some(13.5),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                ],
            });
            rows.push(UiNode::Divider {
                vertical: false,
                color: None,
            });
            if let Some(cfg) = platform_cfg {
                rows.push(UiNode::Toggle {
                    id: "ed.xray.enabled".into(),
                    label: "Enable X-Ray".into(),
                    checked: cfg.xray.enabled,
                    disabled: false,
                });
                rows.push(UiNode::Toggle {
                    id: "ed.xray.compact".into(),
                    label: "Show compact indicator".into(),
                    checked: cfg.xray.compact_indicator,
                    disabled: false,
                });
                rows.push(UiNode::Toggle {
                    id: "ed.xray.privacy".into(),
                    label: "Privacy mode (redact secrets)".into(),
                    checked: cfg.xray.privacy_mode,
                    disabled: false,
                });
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: "Level".into(),
                            size: Some(11.5),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Dropdown {
                            id: "ed.xray.level".into(),
                            options: vec!["Light".into(), "Inspect".into()],
                            selected: usize::from(cfg.xray.level != XRayLevel::Light),
                            disabled: false,
                        },
                    ],
                });
            }
        }
        Some(sel_id) if mgr.placements.contains_key(sel_id) => {
            let cfg = &mgr.placements[sel_id];
            let title = mgr
                .extensions
                .get(sel_id)
                .map_or(sel_id, |e| e.manifest.name.as_str());
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Icon {
                        name: "sliders".into(),
                        size: Some(14.0),
                        color: Some(UiColor::Token("accent".into())),
                    },
                    UiNode::Text {
                        text: title.to_string(),
                        size: Some(13.5),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                ],
            });
            rows.push(UiNode::Divider {
                vertical: false,
                color: None,
            });

            let field_val = |input_id: &str, current: String| -> (String, bool) {
                if mgr.editor.focused_input.as_deref() == Some(input_id) {
                    (mgr.editor.input_buffer.clone(), true)
                } else {
                    (current, false)
                }
            };

            // Geometry inputs (exact coordinates, size, corner radius, Z-index)
            rows.push(UiNode::Text {
                text: "GEOMETRY (px)".into(),
                size: Some(10.0),
                color: Some(UiColor::Token("muted".into())),
                align: Align::Start,
            });
            let (vx, fx) = field_val("ed.in.x", format!("{:.0}", cfg.x));
            let (vy, fy) = field_val("ed.in.y", format!("{:.0}", cfg.y));
            let (vw, fw) = field_val("ed.in.w", format!("{:.0}", cfg.width));
            let (vh, fh) = field_val("ed.in.h", format!("{:.0}", cfg.height));
            let (vr, fr) = field_val("ed.in.radius", format!("{:.0}", cfg.border_radius));
            let (vz, fz) = field_val("ed.in.z", format!("{}", cfg.z_index));

            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Text {
                        text: "X".into(),
                        size: Some(11.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    },
                    UiNode::TextInput {
                        id: "ed.in.x".into(),
                        value: vx,
                        placeholder: "0".into(),
                        focused: fx,
                        width: Some(74.0),
                        disabled: false,
                    },
                    UiNode::Text {
                        text: "Y".into(),
                        size: Some(11.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    },
                    UiNode::TextInput {
                        id: "ed.in.y".into(),
                        value: vy,
                        placeholder: "0".into(),
                        focused: fy,
                        width: Some(74.0),
                        disabled: false,
                    },
                ],
            });
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Text {
                        text: "W".into(),
                        size: Some(11.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    },
                    UiNode::TextInput {
                        id: "ed.in.w".into(),
                        value: vw,
                        placeholder: "200".into(),
                        focused: fw,
                        width: Some(74.0),
                        disabled: false,
                    },
                    UiNode::Text {
                        text: "H".into(),
                        size: Some(11.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    },
                    UiNode::TextInput {
                        id: "ed.in.h".into(),
                        value: vh,
                        placeholder: "80".into(),
                        focused: fh,
                        width: Some(74.0),
                        disabled: false,
                    },
                ],
            });
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Text {
                        text: "R".into(),
                        size: Some(11.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    },
                    UiNode::TextInput {
                        id: "ed.in.radius".into(),
                        value: vr,
                        placeholder: "10".into(),
                        focused: fr,
                        width: Some(74.0),
                        disabled: false,
                    },
                    UiNode::Text {
                        text: "Z".into(),
                        size: Some(11.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    },
                    UiNode::TextInput {
                        id: "ed.in.z".into(),
                        value: vz,
                        placeholder: "10".into(),
                        focused: fz,
                        width: Some(74.0),
                        disabled: false,
                    },
                ],
            });

            // Appearance: Opacity slider + Accent/Surface color presets
            rows.push(UiNode::Row {
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
                        width: Some(126.0),
                        height: Some(14.0),
                        color: Some(UiColor::Token("accent".into())),
                        track: None,
                    },
                ],
            });

            // Placement dropdowns (Mode, Anchor, TUI, Narrow)
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Dropdown {
                        id: "ed.prop.mode".into(),
                        options: vec![
                            "Overlay".into(),
                            "SmartOverlay".into(),
                            "Dock".into(),
                            "Behind".into(),
                            "Inline".into(),
                            "Hidden".into(),
                        ],
                        selected: match cfg.mode {
                            PlacementMode::Overlay | PlacementMode::Float => 0,
                            PlacementMode::SmartOverlay => 1,
                            PlacementMode::Dock => 2,
                            PlacementMode::Behind => 3,
                            PlacementMode::Inline => 4,
                            PlacementMode::Hidden => 5,
                        },
                        disabled: false,
                    },
                    UiNode::Dropdown {
                        id: "ed.prop.anchor".into(),
                        options: vec![
                            "TopLeft".into(),
                            "TopCenter".into(),
                            "TopRight".into(),
                            "CenterLeft".into(),
                            "Center".into(),
                            "CenterRight".into(),
                            "BottomLeft".into(),
                            "BottomCenter".into(),
                            "BottomRight".into(),
                        ],
                        selected: match cfg.anchor {
                            Anchor::TopLeft => 0,
                            Anchor::TopCenter => 1,
                            Anchor::TopRight => 2,
                            Anchor::CenterLeft => 3,
                            Anchor::Center => 4,
                            Anchor::CenterRight => 5,
                            Anchor::BottomLeft => 6,
                            Anchor::BottomCenter => 7,
                            Anchor::BottomRight => 8,
                        },
                        disabled: false,
                    },
                ],
            });
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Dropdown {
                        id: "ed.prop.tui".into(),
                        options: vec!["TUI: Overlay".into(), "TUI: Behind".into(), "TUI: Hidden".into()],
                        selected: match cfg.tui_behavior {
                            TuiBehavior::Overlay => 0,
                            TuiBehavior::Behind => 1,
                            TuiBehavior::Hidden => 2,
                        },
                        disabled: false,
                    },
                    UiNode::Dropdown {
                        id: "ed.prop.narrow".into(),
                        options: vec!["Narrow: Compact".into(), "Narrow: Shrink".into(), "Narrow: Hide".into()],
                        selected: match cfg.narrow_behavior {
                            NarrowBehavior::Compact => 0,
                            NarrowBehavior::Shrink => 1,
                            NarrowBehavior::Hide => 2,
                        },
                        disabled: false,
                    },
                ],
            });

            rows.push(UiNode::Toggle {
                id: "ed.prop.lock".into(),
                label: "Lock position".into(),
                checked: cfg.locked,
                disabled: false,
            });
            rows.push(UiNode::Toggle {
                id: "ed.prop.click_through".into(),
                label: "Click-through".into(),
                checked: cfg.click_through,
                disabled: false,
            });
            rows.push(UiNode::Toggle {
                id: "ed.prop.snap".into(),
                label: "Snap to terminal grid".into(),
                checked: cfg.snap_to_grid,
                disabled: false,
            });
            rows.push(UiNode::Toggle {
                id: "ed.prop.avoid".into(),
                label: "Avoid overlap".into(),
                checked: cfg.avoid_overlap,
                disabled: false,
            });

            // Auto-generated extension settings_schema controls
            if let Some(ext) = mgr.extensions.get(sel_id)
                && !ext.manifest.settings_schema.is_empty()
            {
                rows.push(UiNode::Divider {
                    vertical: false,
                    color: None,
                });
                rows.push(UiNode::Text {
                    text: "EXTENSION SETTINGS".into(),
                    size: Some(10.0),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
                let cur_vals = mgr.extension_settings.get(sel_id);
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
                            rows.push(UiNode::Toggle {
                                id: format!("ed.schema.bool.{}", field.key),
                                label: lbl,
                                checked,
                                disabled: false,
                            });
                        }
                        super::manifest::SettingFieldKind::Enum if !field.options.is_empty() => {
                            let cur = cur_vals
                                .and_then(|v| v.get(&field.key))
                                .and_then(|v| v.as_str())
                                .or_else(|| field.default.as_ref().and_then(|v| v.as_str()))
                                .unwrap_or(field.options[0].as_str());
                            let sel_idx = field
                                .options
                                .iter()
                                .position(|o| o == cur)
                                .unwrap_or(0);
                            rows.push(UiNode::Row {
                                gap: 6.0,
                                align: Align::Center,
                                children: vec![
                                    UiNode::Text {
                                        text: lbl,
                                        size: Some(11.0),
                                        color: Some(UiColor::Token("muted".into())),
                                        align: Align::Start,
                                    },
                                    UiNode::Dropdown {
                                        id: format!("ed.schema.enum.{}", field.key),
                                        options: field.options.clone(),
                                        selected: sel_idx,
                                        disabled: false,
                                    },
                                ],
                            });
                        }
                        super::manifest::SettingFieldKind::Slider => {
                            let min = field.min.unwrap_or(0.0) as f32;
                            let max = field.max.unwrap_or(100.0).max(min as f64 + 1.0) as f32;
                            let val = cur_vals
                                .and_then(|v| v.get(&field.key))
                                .and_then(|v| v.as_f64())
                                .or_else(|| {
                                    field.default.as_ref().and_then(|v| {
                                        v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
                                    })
                                })
                                .unwrap_or(min as f64) as f32;
                            let norm = ((val - min) / (max - min)).clamp(0.0, 1.0);
                            rows.push(UiNode::Row {
                                gap: 6.0,
                                align: Align::Center,
                                children: vec![
                                    UiNode::Text {
                                        text: format!("{lbl} ({:.0})", val),
                                        size: Some(11.0),
                                        color: Some(UiColor::Token("muted".into())),
                                        align: Align::Start,
                                    },
                                    UiNode::Slider {
                                        id: format!("ed.schema.slider.{}", field.key),
                                        value: norm,
                                        width: Some(110.0),
                                        height: Some(14.0),
                                        color: Some(UiColor::Token("accent".into())),
                                        track: None,
                                    },
                                ],
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {
            rows.push(UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Icon {
                        name: "sliders".into(),
                        size: Some(14.0),
                        color: Some(UiColor::Token("muted".into())),
                    },
                    UiNode::Text {
                        text: "Inspector".into(),
                        size: Some(13.5),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                ],
            });
            rows.push(UiNode::Divider {
                vertical: false,
                color: None,
            });
            rows.push(UiNode::Text {
                text: "Select a widget, Background Engine, or X-Ray from the Catalog or Preview."
                    .into(),
                size: Some(11.5),
                color: Some(UiColor::Token("muted".into())),
                align: Align::Start,
            });
        }
    }

    UiNode::Container {
        padding: 12.0,
        bg: Some(UiColor::Token("surface-panel".into())),
        border: Some(UiColor::Token("border-strong".into())),
        radius: 10.0,
        width: None,
        height: None,
        child: Box::new(UiNode::Column {
            gap: 7.0,
            align: Align::Stretch,
            children: rows,
        }),
    }
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
