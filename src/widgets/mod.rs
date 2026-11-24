// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! BeyrixWidgets subsystem coordinator.

#![allow(dead_code)]

pub mod layout;
pub mod manifest;
pub mod permissions;
pub mod runtime;
pub mod ui;

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::WidgetsConfig;
use crate::events::EventBus;
use crate::render::{Renderer, Scene};
use crate::theme::Theme;

use layout::{
    DragOperation, ResolvedWidgetRect, VisualLayoutEditor, WidgetPlacementConfig,
    compute_dock_insets, resolve_widget_rect_with_cursor,
};
use manifest::{ExtensionManifest, LoadedExtension};
use permissions::PermissionSet;
use runtime::{HostAction, VmCommand, WidgetRuntime};
use ui::{HitKind, UiHitTarget, UiLayer, emit_editor_chrome, emit_ui_tree};

/// Host-determined trust level for an extension.
///
/// Trust is NEVER read from the extension's own manifest (`manifest.official`);
/// only extensions explicitly loaded from host-controlled official paths
/// receive `HostOfficial`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionTrust {
    ThirdParty,
    HostOfficial,
}

/// Top-level coordinator for BeyrixWidgets.
///
/// Lazy-initializes the JS runtime only when `config.enabled` is true.
pub struct WidgetManager {
    pub config: WidgetsConfig,
    pub extensions: BTreeMap<String, LoadedExtension>,
    pub granted_permissions: BTreeMap<String, PermissionSet>,
    pub placements: BTreeMap<String, WidgetPlacementConfig>,
    pub editor: VisualLayoutEditor,
    pub hit_targets: Vec<UiHitTarget>,
    pub resolved_rects: BTreeMap<String, ResolvedWidgetRect>,
    unloaded_extensions: Vec<String>,
    runtime: Option<WidgetRuntime>,
}

impl WidgetManager {
    pub fn new(config: WidgetsConfig) -> Self {
        Self {
            config,
            extensions: BTreeMap::new(),
            granted_permissions: BTreeMap::new(),
            placements: BTreeMap::new(),
            editor: VisualLayoutEditor::new(),
            hit_targets: Vec::new(),
            resolved_rects: BTreeMap::new(),
            unloaded_extensions: Vec::new(),
            runtime: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn ensure_started(&mut self, theme: Theme, bus: EventBus) -> Result<(), String> {
        if !self.config.enabled || self.runtime.is_some() {
            return Ok(());
        }
        let vm = WidgetRuntime::start(
            self.config.memory_limit_mb,
            self.config.stack_limit_kb,
            self.config.execution_timeout_ms,
            theme,
            bus,
        )?;
        self.runtime = Some(vm);
        if let Some(dir) = self
            .config
            .extensions_dir
            .clone()
            .or_else(crate::config::PlatformConfig::default_extensions_dir)
            && dir.is_dir()
        {
            let _ = self.load_directory(&dir, ExtensionTrust::ThirdParty);
        }
        Ok(())
    }

    pub fn load_directory(&mut self, dir: &Path, trust: ExtensionTrust) -> Result<usize, String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        let mut count = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && path.join("beyrix-ext.toml").is_file()
                && let Ok(loaded) = ExtensionManifest::load_from_dir(&path)
            {
                self.register_extension(loaded, trust);
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn register_extension(&mut self, loaded: LoadedExtension, trust: ExtensionTrust) {
        let id = loaded.manifest.id.clone();
        // Security: Never trust `loaded.manifest.official` from an untrusted manifest!
        let perms = match trust {
            ExtensionTrust::HostOfficial => {
                PermissionSet::new(loaded.manifest.permissions.iter().copied())
            }
            ExtensionTrust::ThirdParty => {
                PermissionSet::safe_default(&loaded.manifest.permissions, &[])
            }
        };
        self.granted_permissions.insert(id.clone(), perms.clone());
        self.placements
            .entry(id.clone())
            .or_insert_with(|| loaded.manifest.placement.clone());
        if let Some(vm) = &self.runtime {
            vm.send(VmCommand::LoadExtension {
                id: id.clone(),
                entry_path: loaded.entry_path.clone(),
                manifest: Box::new(loaded.manifest.clone()),
                permissions: perms,
            });
        }
        self.extensions.insert(id, loaded);
    }

    pub fn unload_extension(&mut self, id: &str) {
        self.extensions.remove(id);
        self.granted_permissions.remove(id);
        self.unloaded_extensions.push(id.to_string());
        if let Some(vm) = &self.runtime {
            vm.send(VmCommand::UnloadExtension { id: id.to_string() });
        }
    }

    pub fn reload_extension(&mut self, id: &str) {
        self.unloaded_extensions.push(id.to_string());
        if let Some(vm) = &self.runtime {
            vm.send(VmCommand::ReloadExtension { id: id.to_string() });
        }
    }

    pub fn drain_actions(&mut self) -> Vec<HostAction> {
        let mut actions: Vec<HostAction> = self
            .unloaded_extensions
            .drain(..)
            .map(|owner| HostAction::BackgroundReleaseScene { owner })
            .collect();
        if let Some(vm) = &self.runtime {
            actions.extend(vm.drain_actions());
        }
        actions
    }

    pub fn take_dirty(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(WidgetRuntime::take_render_dirty)
    }

    pub fn dock_insets(&self, in_tui: bool, scale: f32) -> layout::ViewportInsets {
        if !self.config.enabled {
            return layout::ViewportInsets::default();
        }
        let cfgs: Vec<&WidgetPlacementConfig> = self.placements.values().collect();
        compute_dock_insets(&cfgs, in_tui, scale)
    }

    /// Emit all visible widgets and editor chrome into `Scene`.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_to_scene(
        &mut self,
        win_size: (f32, f32),
        scale: f32,
        cell_size: (f32, f32),
        in_tui: bool,
        pointer_px: (f32, f32),
        inline_y_px: Option<f32>,
        cursor_row_px: Option<(f32, f32)>,
        theme: &Theme,
        renderer: &mut Renderer,
        scene: &mut Scene,
    ) {
        self.hit_targets.clear();
        self.resolved_rects.clear();
        if !self.config.enabled {
            return;
        }
        let Some(vm) = &self.runtime else {
            return;
        };
        let renders = vm.snapshot_renders();
        let mut ordered: Vec<(&String, &WidgetPlacementConfig)> = self.placements.iter().collect();
        ordered.sort_by_key(|(_, cfg)| cfg.z_index);

        for (id, cfg) in ordered {
            let Some(state) = renders.get(id) else {
                continue;
            };
            let pre_rect = resolve_widget_rect_with_cursor(
                cfg,
                win_size.0,
                win_size.1,
                scale,
                cell_size.0,
                cell_size.1,
                in_tui,
                true,
                inline_y_px,
                cursor_row_px,
            );
            let hovered = pointer_px.0 >= pre_rect.x
                && pointer_px.0 <= pre_rect.x + pre_rect.w
                && pointer_px.1 >= pre_rect.y
                && pointer_px.1 <= pre_rect.y + pre_rect.h;
            let resolved = resolve_widget_rect_with_cursor(
                cfg,
                win_size.0,
                win_size.1,
                scale,
                cell_size.0,
                cell_size.1,
                in_tui,
                hovered,
                inline_y_px,
                cursor_row_px,
            );
            self.resolved_rects.insert(id.clone(), resolved);
            if !resolved.visible && !self.editor.active {
                continue;
            }
            let layer = if resolved.effective_mode == layout::PlacementMode::Behind {
                UiLayer::Behind
            } else {
                UiLayer::Overlay
            };
            if let Some(tree) = &state.tree {
                emit_ui_tree(
                    id,
                    tree,
                    (resolved.x, resolved.y, resolved.w, resolved.h),
                    scale * cfg.scale,
                    cfg.opacity,
                    layer,
                    theme,
                    renderer,
                    scene,
                    &mut self.hit_targets,
                );
            }
            if self.editor.active {
                let selected = self.editor.selected_widget.as_deref() == Some(id.as_str());
                emit_editor_chrome(
                    scene,
                    (resolved.x, resolved.y, resolved.w, resolved.h),
                    selected,
                    scale,
                );
            }
        }
    }

    /// Handle pointer press; returns `true` if a widget or the layout editor consumed it.
    pub fn handle_pointer_press(&mut self, pointer_px: (f32, f32), scale: f32) -> bool {
        if !self.config.enabled {
            return false;
        }
        if self.editor.active {
            for (id, rect) in &self.resolved_rects {
                if pointer_px.0 >= rect.x
                    && pointer_px.0 <= rect.x + rect.w
                    && pointer_px.1 >= rect.y
                    && pointer_px.1 <= rect.y + rect.h
                {
                    let handle = 12.0 * scale;
                    let op = if pointer_px.0 >= rect.x + rect.w - handle
                        && pointer_px.1 >= rect.y + rect.h - handle
                    {
                        DragOperation::ResizeBottomRight
                    } else {
                        DragOperation::Move
                    };
                    self.editor.begin_drag(
                        id.clone(),
                        op,
                        pointer_px,
                        (rect.x, rect.y, rect.w, rect.h),
                    );
                    return true;
                }
            }
            return false;
        }

        for hit in self.hit_targets.iter().rev() {
            if pointer_px.0 >= hit.x
                && pointer_px.0 <= hit.x + hit.w
                && pointer_px.1 >= hit.y
                && pointer_px.1 <= hit.y + hit.h
            {
                let interactive = self
                    .resolved_rects
                    .get(&hit.widget_id)
                    .is_some_and(|r| r.interactive);
                if !interactive {
                    continue;
                }
                if let Some(vm) = &self.runtime {
                    let value = match hit.kind {
                        HitKind::Button => 1.0,
                        HitKind::Slider => {
                            ((pointer_px.0 - hit.x) / hit.w.max(1.0)).clamp(0.0, 1.0)
                        }
                    };
                    vm.send(VmCommand::UiEvent {
                        widget_id: hit.widget_id.clone(),
                        node_id: hit.node_id.clone(),
                        kind: match hit.kind {
                            HitKind::Button => "click".into(),
                            HitKind::Slider => "change".into(),
                        },
                        value,
                    });
                }
                return true;
            }
        }
        false
    }

    pub fn handle_pointer_motion(
        &mut self,
        pointer_px: (f32, f32),
        win_size: (f32, f32),
        scale: f32,
        cell_size: (f32, f32),
    ) -> bool {
        if !self.editor.active || self.editor.drag_mode.is_none() {
            return false;
        }
        if let Some(id) = &self.editor.selected_widget
            && let Some(cfg) = self.placements.get_mut(id)
        {
            self.editor.update_drag(
                cfg,
                pointer_px,
                win_size.0,
                win_size.1,
                scale,
                cell_size.0,
                cell_size.1,
            );
            return true;
        }
        false
    }

    pub fn handle_pointer_release(&mut self) {
        self.editor.end_drag();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use permissions::Capability;

    #[test]
    fn third_party_cannot_escalate_privileges_via_manifest_official_flag() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("sneaky");
        std::fs::create_dir(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("index.js"), "beyrix.log('hi');").unwrap();
        std::fs::write(
            ext_dir.join("beyrix-ext.toml"),
            r#"
id = "org.untrusted.sneaky"
name = "Sneaky"
sdk_version = "1.0.0"
official = true
permissions = ["ui", "terminal-read", "network", "process-spawn", "xray-control"]
"#,
        )
        .unwrap();

        let _ = ExtensionManifest::load_from_dir(&ext_dir).unwrap();
        let mut mgr = WidgetManager::new(WidgetsConfig::default());
        let count = mgr
            .load_directory(dir.path(), ExtensionTrust::ThirdParty)
            .unwrap();
        assert_eq!(count, 1);

        let perms = mgr.granted_permissions.get("org.untrusted.sneaky").unwrap();
        assert!(perms.has(Capability::Ui));
        assert!(!perms.has(Capability::TerminalRead));
        assert!(!perms.has(Capability::Network));
        assert!(!perms.has(Capability::ProcessSpawn));
        assert!(!perms.has(Capability::XRayControl));
    }
}
