// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! BeyrixWidgets subsystem coordinator.

#![allow(dead_code)]

pub mod compositor;
pub mod editor_ui;
pub mod icons;
pub mod layout;
pub mod manifest;
pub mod permissions;
pub mod runtime;
pub mod ui;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
use runtime::{HostAction, LifecycleCommand, VmCommand, WidgetRenderState, WidgetRuntime};
use ui::{HitKind, UiHitTarget, UiLayer, emit_editor_chrome};

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

/// Diagnostic record for an extension that failed to parse, validate, or load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionLoadError {
    pub dir_name: String,
    pub path: PathBuf,
    pub error: String,
}

/// Top-level coordinator for BeyrixWidgets.
///
/// Lazy-initializes the JS runtime only when `config.enabled` is true.
pub struct WidgetManager {
    pub config: WidgetsConfig,
    pub extensions: BTreeMap<String, LoadedExtension>,
    pub load_errors: Vec<ExtensionLoadError>,
    pub disabled_extensions: BTreeSet<String>,
    pub trust_levels: BTreeMap<String, ExtensionTrust>,
    pub granted_permissions: BTreeMap<String, PermissionSet>,
    pub placements: BTreeMap<String, WidgetPlacementConfig>,
    pub profiles: layout::ProfileStore,
    pub extension_settings: BTreeMap<String, serde_json::Value>,
    pub editor: VisualLayoutEditor,
    pub editor_hits: Vec<UiHitTarget>,
    pub editor_panel_rects: Vec<(f32, f32, f32, f32)>,
    pub hit_targets: Vec<UiHitTarget>,
    pub resolved_rects: BTreeMap<String, ResolvedWidgetRect>,
    generations: BTreeMap<String, u64>,
    unloaded_extensions: Vec<String>,
    runtime: Option<WidgetRuntime>,
    active_slider: Option<(String, String, f32, f32)>,
    file_mtimes: BTreeMap<String, std::time::SystemTime>,
    last_reload_poll: Option<std::time::Instant>,
    last_window_state: (f32, f32, f32, bool),
}

impl WidgetManager {
    pub fn new(config: WidgetsConfig) -> Self {
        let mut profiles = layout::ProfileStore::default();
        if let Some(path) = Self::profiles_path()
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(loaded) = serde_json::from_str::<layout::ProfileStore>(&json)
        {
            profiles = loaded;
        }
        Self {
            config,
            extensions: BTreeMap::new(),
            load_errors: Vec::new(),
            disabled_extensions: BTreeSet::new(),
            trust_levels: BTreeMap::new(),
            granted_permissions: BTreeMap::new(),
            placements: BTreeMap::new(),
            profiles,
            extension_settings: BTreeMap::new(),
            editor: VisualLayoutEditor::new(),
            editor_hits: Vec::new(),
            editor_panel_rects: Vec::new(),
            hit_targets: Vec::new(),
            resolved_rects: BTreeMap::new(),
            generations: BTreeMap::new(),
            unloaded_extensions: Vec::new(),
            runtime: None,
            active_slider: None,
            file_mtimes: BTreeMap::new(),
            last_reload_poll: None,
            last_window_state: (0.0, 0.0, 1.0, true),
        }
    }

    pub fn profiles_path() -> Option<std::path::PathBuf> {
        crate::config::PlatformConfig::config_path()
            .and_then(|p| p.parent().map(|d| d.join("profiles.json")))
    }

    pub fn persist_profiles(&mut self) -> Result<(), String> {
        let active = self.profiles.active.clone();
        if let Some(prof) = self.profiles.profiles.get_mut(&active) {
            prof.widgets = self.placements.clone();
        }
        let Some(path) = Self::profiles_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
        }
        let json = serde_json::to_string_pretty(&self.profiles)
            .map_err(|e| format!("serialize profiles: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).map_err(|e| format!("write tmp: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename profiles: {e}"))?;
        Ok(())
    }

    /// Poll loaded extension entry files for mtime changes when `config.hot_reload` is enabled.
    /// Does zero filesystem syscalls when `hot_reload` is false or runtime is stopped.
    pub fn poll_hot_reload(&mut self) -> usize {
        if !self.config.enabled || !self.config.hot_reload || self.runtime.is_none() {
            return 0;
        }
        let now = std::time::Instant::now();
        if self
            .last_reload_poll
            .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(350))
        {
            return 0;
        }
        self.last_reload_poll = Some(now);
        let mut to_reload = Vec::new();
        for (id, ext) in &self.extensions {
            if let Ok(meta) = std::fs::metadata(&ext.entry_path)
                && let Ok(mtime) = meta.modified()
            {
                match self.file_mtimes.get(id) {
                    Some(&prev) if prev != mtime => {
                        to_reload.push((id.clone(), mtime));
                    }
                    None => {
                        self.file_mtimes.insert(id.clone(), mtime);
                    }
                    _ => {}
                }
            }
        }
        let mut reloaded = 0;
        for (id, mtime) in to_reload {
            self.file_mtimes.insert(id.clone(), mtime);
            if self.reload_extension(&id).is_ok() {
                reloaded += 1;
            }
        }
        reloaded
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Host-controlled search paths for bundled official extensions.
    pub fn official_extensions_dirs() -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            dirs.push(parent.join("../share/beyrix/extensions/official"));
            dirs.push(parent.join("../../extensions/official"));
        }
        dirs.push(std::path::PathBuf::from(
            "/usr/share/beyrix/extensions/official",
        ));
        dirs
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
        // Load host-official extensions first from host-controlled paths.
        for official_dir in Self::official_extensions_dirs() {
            if official_dir.is_dir() {
                let _ = self.load_directory(&official_dir, ExtensionTrust::HostOfficial);
                break;
            }
        }
        // Load third-party user extensions; never allowed to overwrite official IDs.
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
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("beyrix-ext.toml").is_file())
            .collect();
        dirs.sort();
        for path in dirs {
            let dir_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            self.load_errors.retain(|e| e.path != path);
            match ExtensionManifest::load_from_dir(&path) {
                Ok(loaded) => match self.register_extension(loaded, trust) {
                    Ok(()) => count += 1,
                    Err(err) => {
                        eprintln!("beyrix widgets: failed to register {dir_name}: {err}");
                        self.load_errors.push(ExtensionLoadError {
                            dir_name,
                            path,
                            error: err,
                        });
                    }
                },
                Err(err) => {
                    eprintln!("beyrix widgets: failed to load {dir_name}: {err}");
                    self.load_errors.push(ExtensionLoadError {
                        dir_name,
                        path,
                        error: err,
                    });
                }
            }
        }
        Ok(count)
    }

    pub fn is_extension_enabled(&self, id: &str) -> bool {
        !self.disabled_extensions.contains(id)
    }

    pub fn set_extension_enabled(&mut self, id: &str, enabled: bool) {
        if enabled {
            self.disabled_extensions.remove(id);
        } else {
            self.disabled_extensions.insert(id.to_string());
        }
    }

    pub fn update_window(&mut self, width: f32, height: f32, scale: f32, focused: bool) {
        let next = (width, height, scale, focused);
        if (self.last_window_state.0 - width).abs() <= 0.5
            && (self.last_window_state.1 - height).abs() <= 0.5
            && (self.last_window_state.2 - scale).abs() <= 0.01
            && self.last_window_state.3 == focused
        {
            return;
        }
        self.last_window_state = next;
        if let Some(vm) = &self.runtime {
            vm.send(VmCommand::UpdateWindow {
                width: width.round() as i32,
                height: height.round() as i32,
                scale,
                focused,
            });
        }
    }

    pub fn register_extension(
        &mut self,
        loaded: LoadedExtension,
        trust: ExtensionTrust,
    ) -> Result<(), String> {
        let id = loaded.manifest.id.clone();
        // Prevent third-party extensions from hijacking an already-registered
        // host-official extension ID.
        if trust == ExtensionTrust::ThirdParty
            && self.trust_levels.get(&id) == Some(&ExtensionTrust::HostOfficial)
        {
            return Err(format!(
                "third-party extension cannot overwrite host-official extension {id:?}"
            ));
        }
        // Security: Never trust `loaded.manifest.official` from an untrusted manifest!
        let perms = match trust {
            ExtensionTrust::HostOfficial => {
                PermissionSet::new(loaded.manifest.permissions.iter().copied())
            }
            ExtensionTrust::ThirdParty => {
                PermissionSet::safe_default(&loaded.manifest.permissions, &[])
            }
        };
        let gen_entry = self.generations.entry(id.clone()).or_insert(0);
        *gen_entry += 1;
        let generation = *gen_entry;
        self.trust_levels.insert(id.clone(), trust);
        self.granted_permissions.insert(id.clone(), perms.clone());
        self.placements
            .entry(id.clone())
            .or_insert_with(|| loaded.manifest.placement.clone());
        if let Some(vm) = &self.runtime {
            vm.send_lifecycle(LifecycleCommand::LoadExtension {
                id: id.clone(),
                generation,
                entry_path: loaded.entry_path.clone(),
                manifest: Box::new(loaded.manifest.clone()),
                permissions: perms,
            })?;
        }
        self.extensions.insert(id, loaded);
        Ok(())
    }

    pub fn unload_extension(&mut self, id: &str) {
        self.extensions.remove(id);
        self.trust_levels.remove(id);
        self.granted_permissions.remove(id);
        if let Some(g) = self.generations.get_mut(id) {
            *g += 1;
        }
        self.unloaded_extensions.push(id.to_string());
        if let Some(vm) = &self.runtime {
            let _ = vm.send_lifecycle(LifecycleCommand::UnloadExtension { id: id.to_string() });
        }
    }

    pub fn reload_extension(&mut self, id: &str) -> Result<(), String> {
        let generation = {
            let entry = self.generations.entry(id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        self.unloaded_extensions.push(id.to_string());
        if let Some(vm) = &self.runtime {
            vm.send_lifecycle(LifecycleCommand::ReloadExtension {
                id: id.to_string(),
                generation,
            })?;
        }
        Ok(())
    }

    pub fn update_extension_settings(
        &mut self,
        id: &str,
        values: serde_json::Value,
    ) -> Result<(), String> {
        let json =
            serde_json::to_string(&values).map_err(|e| format!("serialize settings: {e}"))?;
        self.extension_settings.insert(id.to_string(), values);
        if let Some(vm) = &self.runtime {
            vm.send_lifecycle(LifecycleCommand::UpdateSettings {
                id: id.to_string(),
                settings_json: json,
            })?;
        }
        Ok(())
    }

    pub fn render_snapshots(&self) -> std::collections::HashMap<String, WidgetRenderState> {
        let Some(vm) = &self.runtime else {
            return std::collections::HashMap::new();
        };
        vm.snapshot_renders()
            .into_iter()
            .filter(|(id, st)| self.generations.get(id).copied() == Some(st.generation))
            .collect()
    }

    pub fn drain_actions(&mut self) -> Vec<HostAction> {
        let mut actions: Vec<HostAction> = self
            .unloaded_extensions
            .drain(..)
            .map(|owner| HostAction::BackgroundReleaseScene { owner })
            .collect();
        if let Some(vm) = &self.runtime {
            for env in vm.drain_actions() {
                if self.generations.get(&env.extension_id).copied() == Some(env.generation) {
                    actions.push(env.action);
                }
            }
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
        self.emit_to_scene_with_config(
            None,
            win_size,
            scale,
            cell_size,
            in_tui,
            pointer_px,
            inline_y_px,
            cursor_row_px,
            theme,
            renderer,
            scene,
        );
    }

    /// Emit all visible widgets and Beyrix Studio panels into `Scene`.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_to_scene_with_config(
        &mut self,
        platform_cfg: Option<&crate::config::PlatformConfig>,
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
        self.editor_hits.clear();
        self.editor_panel_rects.clear();
        self.resolved_rects.clear();
        if !self.config.enabled {
            return;
        }
        let renders = self.render_snapshots();
        let mut ordered: Vec<(&String, &WidgetPlacementConfig)> = self.placements.iter().collect();
        ordered.sort_by_key(|(_, cfg)| (cfg.z_index, -cfg.priority));

        let mut occupied: Vec<(f32, f32, f32, f32)> = Vec::new();
        for (id, cfg) in ordered {
            if !self.is_extension_enabled(id) {
                continue;
            }
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
            let mut resolved = resolve_widget_rect_with_cursor(
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
            if cfg.avoid_overlap && !self.editor.active {
                resolved = layout::resolve_non_overlapping_position(
                    resolved,
                    &occupied,
                    win_size.0,
                    win_size.1,
                    (cfg.margin * scale).max(6.0 * scale),
                );
            }
            if resolved.visible {
                occupied.push((resolved.x, resolved.y, resolved.w, resolved.h));
            }
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
                ui::emit_ui_tree_interactive(
                    id,
                    tree,
                    (resolved.x, resolved.y, resolved.w, resolved.h),
                    scale * cfg.scale,
                    cfg.opacity,
                    layer,
                    Some(pointer_px),
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
        if self.editor.active {
            let mut ed_hits = Vec::new();
            let mut ed_rects = Vec::new();
            editor_ui::emit_visual_editor_panel(
                self,
                platform_cfg,
                win_size,
                scale,
                pointer_px,
                theme,
                renderer,
                scene,
                &mut ed_hits,
                &mut ed_rects,
            );
            self.editor_hits = ed_hits;
            self.editor_panel_rects = ed_rects;
        }
    }

    /// Handle pointer press; returns `true` if a widget or Beyrix Studio consumed it.
    pub fn handle_pointer_press(&mut self, pointer_px: (f32, f32), scale: f32) -> bool {
        self.handle_pointer_press_with_config(pointer_px, scale, None)
            .0
    }

    /// Handle pointer press with optional mutable `PlatformConfig` for Beyrix Studio
    /// Background Engine & X-Ray controls. Returns `(consumed, platform_config_changed)`.
    pub fn handle_pointer_press_with_config(
        &mut self,
        pointer_px: (f32, f32),
        scale: f32,
        mut platform_cfg: Option<&mut crate::config::PlatformConfig>,
    ) -> (bool, bool) {
        if !self.config.enabled {
            return (false, false);
        }
        let mut cfg_changed = false;
        if self.editor.active {
            // 1. Check Beyrix Studio panel controls first.
            let hit_opt = self
                .editor_hits
                .iter()
                .rev()
                .find(|h| {
                    pointer_px.0 >= h.x
                        && pointer_px.0 <= h.x + h.w
                        && pointer_px.1 >= h.y
                        && pointer_px.1 <= h.y + h.h
                })
                .cloned();
            if let Some(hit) = hit_opt {
                // Clicking any non-input control blurs focused text input.
                if hit.kind != HitKind::TextInput {
                    self.commit_focused_editor_input();
                }
                match (hit.node_id.as_str(), hit.kind) {
                    ("ed.left.tabs", HitKind::Tab(idx)) => {
                        self.editor.left_tab = if idx == 0 {
                            layout::StudioLeftTab::Catalog
                        } else {
                            layout::StudioLeftTab::Layers
                        };
                    }
                    ("ed.narrow.pane", HitKind::Tab(idx)) => {
                        self.editor.narrow_pane = match idx {
                            0 => layout::NarrowStudioPane::Preview,
                            1 => layout::NarrowStudioPane::Catalog,
                            _ => layout::NarrowStudioPane::Inspector,
                        };
                    }
                    ("ed.tabs", HitKind::Tab(idx)) => {
                        self.editor.active_tab = match idx {
                            0 => layout::EditorTab::Catalog,
                            1 => layout::EditorTab::Properties,
                            2 => layout::EditorTab::Layers,
                            _ => layout::EditorTab::Profiles,
                        };
                    }
                    ("ed.close", _) => {
                        let _ = self.persist_profiles();
                        self.editor.toggle();
                    }
                    ("ed.undo", _) => {
                        self.editor.undo(&mut self.placements);
                    }
                    ("ed.redo", _) => {
                        self.editor.redo(&mut self.placements);
                    }
                    ("ed.profile.dropdown", _) => {
                        let names: Vec<String> = self.profiles.profiles.keys().cloned().collect();
                        if !names.is_empty() {
                            let cur = names
                                .iter()
                                .position(|n| n == &self.profiles.active)
                                .unwrap_or(0);
                            let next = names[(cur + 1) % names.len()].clone();
                            self.editor.push_undo(self.placements.clone());
                            if let Some(profile) = self.profiles.switch(&next) {
                                for (wid, pcfg) in &profile.widgets {
                                    self.placements.insert(wid.clone(), pcfg.clone());
                                }
                            }
                        }
                    }
                    (id_str, HitKind::TextInput) if id_str.starts_with("ed.in.") => {
                        if let Some(sel) = self.editor.selected_widget.clone()
                            && let Some(cfg) = self.placements.get(&sel)
                        {
                            let buf = match id_str {
                                "ed.in.x" => format!("{:.0}", cfg.x),
                                "ed.in.y" => format!("{:.0}", cfg.y),
                                "ed.in.w" => format!("{:.0}", cfg.width),
                                "ed.in.h" => format!("{:.0}", cfg.height),
                                "ed.in.radius" => format!("{:.0}", cfg.border_radius),
                                "ed.in.z" => format!("{}", cfg.z_index),
                                _ => String::new(),
                            };
                            self.editor.focused_input = Some(id_str.to_string());
                            self.editor.input_buffer = buf;
                        }
                    }
                    ("ed.bg.enabled", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.background.enabled = !pc.background.enabled;
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.preset", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            let all = crate::config::BackgroundPreset::ALL;
                            let idx = all
                                .iter()
                                .position(|&p| p == pc.background.preset)
                                .unwrap_or(0);
                            pc.background.preset = all[(idx + 1) % all.len()];
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.fit", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            use crate::config::BackgroundFitMode;
                            pc.background.fit_mode = match pc.background.fit_mode {
                                BackgroundFitMode::Fit => BackgroundFitMode::Fill,
                                BackgroundFitMode::Fill => BackgroundFitMode::FixedScale,
                                BackgroundFitMode::FixedScale => BackgroundFitMode::Responsive,
                                BackgroundFitMode::Responsive => BackgroundFitMode::Fit,
                            };
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.alpha", HitKind::Slider) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.background.alpha =
                                ((pointer_px.0 - hit.x) / hit.w.max(1.0)).clamp(0.05, 1.0);
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.speed", HitKind::Slider) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.background.speed =
                                (((pointer_px.0 - hit.x) / hit.w.max(1.0)).clamp(0.05, 1.0)) * 3.0;
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.mouse", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.background.mouse_reactive = !pc.background.mouse_reactive;
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.audio", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.background.audio_reactive = !pc.background.audio_reactive;
                            cfg_changed = true;
                        }
                    }
                    ("ed.bg.auto_quality", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.background.auto_quality = !pc.background.auto_quality;
                            cfg_changed = true;
                        }
                    }
                    ("ed.xray.enabled", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.xray.enabled = !pc.xray.enabled;
                            cfg_changed = true;
                        }
                    }
                    ("ed.xray.compact", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.xray.compact_indicator = !pc.xray.compact_indicator;
                            cfg_changed = true;
                        }
                    }
                    ("ed.xray.privacy", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            pc.xray.privacy_mode = !pc.xray.privacy_mode;
                            cfg_changed = true;
                        }
                    }
                    ("ed.xray.level", _) => {
                        if let Some(pc) = platform_cfg.as_deref_mut() {
                            use crate::config::XRayLevel;
                            pc.xray.level = match pc.xray.level {
                                XRayLevel::Light => XRayLevel::Inspect,
                                _ => XRayLevel::Light,
                            };
                            cfg_changed = true;
                        }
                    }
                    ("ed.prop.mode", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.mode = editor_ui::cycle_placement_mode(cfg.mode);
                            }
                        }
                    }
                    ("ed.prop.anchor", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.anchor = editor_ui::cycle_anchor(cfg.anchor);
                            }
                        }
                    }
                    ("ed.prop.tui", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.tui_behavior = editor_ui::cycle_tui_behavior(cfg.tui_behavior);
                            }
                        }
                    }
                    ("ed.prop.narrow", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.narrow_behavior =
                                    editor_ui::cycle_narrow_behavior(cfg.narrow_behavior);
                            }
                        }
                    }
                    ("ed.prop.lock", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.locked = !cfg.locked;
                            }
                        }
                    }
                    ("ed.prop.click_through", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.click_through = !cfg.click_through;
                            }
                        }
                    }
                    ("ed.prop.hover", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.show_on_hover = !cfg.show_on_hover;
                            }
                        }
                    }
                    ("ed.prop.snap", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.snap_to_grid = !cfg.snap_to_grid;
                            }
                        }
                    }
                    ("ed.prop.avoid", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                cfg.avoid_overlap = !cfg.avoid_overlap;
                            }
                        }
                    }
                    ("ed.prop.opacity", HitKind::Slider) => {
                        if let Some(id) = self.editor.selected_widget.clone()
                            && let Some(cfg) = self.placements.get_mut(&id)
                        {
                            let val = ((pointer_px.0 - hit.x) / hit.w.max(1.0)).clamp(0.1, 1.0);
                            cfg.opacity = val;
                        }
                    }
                    ("ed.prop.w_inc" | "ed.prop.w_dec" | "ed.prop.h_inc" | "ed.prop.h_dec", _) => {
                        if let Some(id) = self.editor.selected_widget.clone() {
                            self.editor.push_undo(self.placements.clone());
                            if let Some(cfg) = self.placements.get_mut(&id) {
                                match hit.node_id.as_str() {
                                    "ed.prop.w_inc" => cfg.width = (cfg.width + 20.0).min(1200.0),
                                    "ed.prop.w_dec" => cfg.width = (cfg.width - 20.0).max(80.0),
                                    "ed.prop.h_inc" => cfg.height = (cfg.height + 16.0).min(800.0),
                                    _ => cfg.height = (cfg.height - 16.0).max(28.0),
                                }
                            }
                        }
                    }
                    (id_str, _) if id_str.starts_with("ed.schema.bool.") => {
                        let key = id_str.trim_start_matches("ed.schema.bool.").to_string();
                        if let Some(sel) = self.editor.selected_widget.clone() {
                            let mut obj = self
                                .extension_settings
                                .get(&sel)
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({}));
                            let cur = obj.get(&key).and_then(|v| v.as_bool()).unwrap_or(false);
                            obj[key] = serde_json::Value::Bool(!cur);
                            let _ = self.update_extension_settings(&sel, obj);
                        }
                    }
                    (id_str, _) if id_str.starts_with("ed.schema.enum.") => {
                        let key = id_str.trim_start_matches("ed.schema.enum.").to_string();
                        if let Some(sel) = self.editor.selected_widget.clone()
                            && let Some(ext) = self.extensions.get(&sel)
                            && let Some(field) =
                                ext.manifest.settings_schema.iter().find(|f| f.key == key)
                            && !field.options.is_empty()
                        {
                            let mut obj = self
                                .extension_settings
                                .get(&sel)
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({}));
                            let cur = obj
                                .get(&key)
                                .and_then(|v| v.as_str())
                                .unwrap_or(field.options[0].as_str());
                            let idx = field.options.iter().position(|o| o == cur).unwrap_or(0);
                            let next = field.options[(idx + 1) % field.options.len()].clone();
                            obj[key] = serde_json::Value::String(next);
                            let _ = self.update_extension_settings(&sel, obj);
                        }
                    }
                    ("ed.profile.new", _) => {
                        let idx = self.profiles.profiles.len() + 1;
                        let name = format!("profile-{idx}");
                        self.profiles.create(&name, self.placements.clone());
                        let _ = self.profiles.switch(&name);
                        let _ = self.persist_profiles();
                    }
                    ("ed.profile.dup", _) => {
                        let cur = self.profiles.active.clone();
                        let name = format!("{cur}-copy");
                        self.profiles.create(&name, self.placements.clone());
                        let _ = self.profiles.switch(&name);
                        let _ = self.persist_profiles();
                    }
                    ("ed.profile.save", _) => {
                        let _ = self.persist_profiles();
                    }
                    (id_str, _) if id_str.starts_with("ed.profile.del.") => {
                        let name = id_str.trim_start_matches("ed.profile.del.");
                        let _ = self.profiles.delete(name);
                        let _ = self.persist_profiles();
                    }
                    (id_str, HitKind::Slider) if id_str.starts_with("ed.schema.slider.") => {
                        let key = id_str.trim_start_matches("ed.schema.slider.").to_string();
                        if let Some(sel) = self.editor.selected_widget.clone()
                            && let Some(ext) = self.extensions.get(&sel)
                            && let Some(field) =
                                ext.manifest.settings_schema.iter().find(|f| f.key == key)
                        {
                            let min = field.min.unwrap_or(0.0);
                            let max = field.max.unwrap_or(100.0).max(min + 1.0);
                            let step = field.step.unwrap_or(1.0).max(0.01);
                            let norm = ((pointer_px.0 - hit.x) / hit.w.max(1.0)).clamp(0.0, 1.0) as f64;
                            let raw = min + norm * (max - min);
                            let snapped = (raw / step).round() * step;
                            let mut obj = self
                                .extension_settings
                                .get(&sel)
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({}));
                            obj[key] = serde_json::json!(snapped);
                            let _ = self.update_extension_settings(&sel, obj);
                        }
                    }
                    (id_str, _) if id_str.starts_with("ed.toggle.") => {
                        let wid = id_str.trim_start_matches("ed.toggle.").to_string();
                        let next = !self.is_extension_enabled(&wid);
                        self.set_extension_enabled(&wid, next);
                    }
                    (id_str, _) if id_str.starts_with("ed.select.") => {
                        let wid = id_str.trim_start_matches("ed.select.").to_string();
                        self.editor.selected_widget = Some(wid);
                        self.editor.active_tab = layout::EditorTab::Properties;
                        self.editor.narrow_pane = layout::NarrowStudioPane::Inspector;
                    }
                    (id_str, _) if id_str.starts_with("ed.reload.") => {
                        let wid = id_str.trim_start_matches("ed.reload.");
                        let _ = self.reload_extension(wid);
                    }
                    (id_str, _) if id_str.starts_with("ed.zup.") => {
                        let wid = id_str.trim_start_matches("ed.zup.").to_string();
                        self.editor.push_undo(self.placements.clone());
                        if let Some(cfg) = self.placements.get_mut(&wid) {
                            cfg.z_index += 1;
                        }
                    }
                    (id_str, _) if id_str.starts_with("ed.zdn.") => {
                        let wid = id_str.trim_start_matches("ed.zdn.").to_string();
                        self.editor.push_undo(self.placements.clone());
                        if let Some(cfg) = self.placements.get_mut(&wid) {
                            cfg.z_index -= 1;
                        }
                    }
                    (id_str, _) if id_str.starts_with("ed.profile.") => {
                        let name = id_str.trim_start_matches("ed.profile.").to_string();
                        self.editor.push_undo(self.placements.clone());
                        if let Some(profile) = self.profiles.switch(&name) {
                            for (wid, pcfg) in &profile.widgets {
                                self.placements.insert(wid.clone(), pcfg.clone());
                            }
                        }
                    }
                    _ => {}
                }
                return (true, cfg_changed);
            }

            self.commit_focused_editor_input();

            // 2. Otherwise check widget canvas for Drag & Drop / Resize handles in reverse Z-order.
            let mut z_ordered: Vec<(&String, &ResolvedWidgetRect)> =
                self.resolved_rects.iter().collect();
            z_ordered.sort_by_key(|(id, _)| {
                self.placements
                    .get(*id)
                    .map_or(0, |c| c.z_index)
            });
            for (id, rect) in z_ordered.into_iter().rev() {
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
                    } else if pointer_px.0 >= rect.x + rect.w - handle {
                        DragOperation::ResizeRight
                    } else if pointer_px.1 >= rect.y + rect.h - handle {
                        DragOperation::ResizeBottom
                    } else if pointer_px.0 <= rect.x + handle {
                        DragOperation::ResizeLeft
                    } else if pointer_px.1 <= rect.y + handle {
                        DragOperation::ResizeTop
                    } else {
                        DragOperation::Move
                    };
                    self.editor.push_undo(self.placements.clone());
                    self.editor.begin_drag(
                        id.clone(),
                        op,
                        pointer_px,
                        (rect.x, rect.y, rect.w, rect.h),
                    );
                    return (true, false);
                }
            }
            return (false, false);
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
                    let (kind, value) = match hit.kind {
                        HitKind::Button => ("click", 1.0),
                        HitKind::Toggle | HitKind::Checkbox => ("toggle", 1.0),
                        HitKind::Dropdown => ("cycle", 1.0),
                        HitKind::ScrollView | HitKind::TextInput => continue,
                        HitKind::Tab(idx) => ("tab", idx as f32),
                        HitKind::Slider => {
                            self.active_slider = Some((
                                hit.widget_id.clone(),
                                hit.node_id.clone(),
                                hit.x,
                                hit.w.max(1.0),
                            ));
                            (
                                "change",
                                ((pointer_px.0 - hit.x) / hit.w.max(1.0)).clamp(0.0, 1.0),
                            )
                        }
                    };
                    vm.send(VmCommand::UiEvent {
                        widget_id: hit.widget_id.clone(),
                        node_id: hit.node_id.clone(),
                        kind: kind.into(),
                        value,
                    });
                }
                return (true, false);
            }
        }
        (false, false)
    }

    pub fn commit_focused_editor_input(&mut self) {
        let Some(input_id) = self.editor.focused_input.take() else {
            return;
        };
        let raw = std::mem::take(&mut self.editor.input_buffer);
        let Some(sel) = self.editor.selected_widget.clone() else {
            return;
        };
        let Ok(val) = raw.trim().parse::<f32>() else {
            return;
        };
        self.editor.push_undo(self.placements.clone());
        if let Some(cfg) = self.placements.get_mut(&sel) {
            match input_id.as_str() {
                "ed.in.x" => cfg.x = val.clamp(-2000.0, 2000.0),
                "ed.in.y" => cfg.y = val.clamp(-2000.0, 2000.0),
                "ed.in.w" => cfg.width = val.clamp(cfg.min_width.max(40.0), 1600.0),
                "ed.in.h" => cfg.height = val.clamp(cfg.min_height.max(24.0), 1200.0),
                "ed.in.radius" => cfg.border_radius = val.clamp(0.0, 32.0),
                "ed.in.z" => cfg.z_index = (val.round() as i32).clamp(-100, 100),
                _ => {}
            }
        }
    }

    /// Handle keyboard input when a Beyrix Studio numeric input is focused.
    /// Returns `true` if the key was consumed.
    pub fn handle_editor_key(
        &mut self,
        ch: Option<char>,
        backspace: bool,
        enter: bool,
        escape: bool,
    ) -> bool {
        if !self.editor.active || self.editor.focused_input.is_none() {
            return false;
        }
        if escape {
            self.editor.focused_input = None;
            self.editor.input_buffer.clear();
            return true;
        }
        if enter {
            self.commit_focused_editor_input();
            return true;
        }
        if backspace {
            self.editor.input_buffer.pop();
            return true;
        }
        if let Some(c) = ch
            && (c.is_ascii_digit() || c == '-' || c == '.')
            && self.editor.input_buffer.len() < 8
        {
            self.editor.input_buffer.push(c);
            return true;
        }
        true
    }

    pub fn handle_pointer_motion(
        &mut self,
        pointer_px: (f32, f32),
        win_size: (f32, f32),
        scale: f32,
        cell_size: (f32, f32),
    ) -> bool {
        // Active widget Slider drag outside Layout Editor
        if !self.editor.active
            && let Some((widget_id, node_id, sx, sw)) = &self.active_slider
        {
            let value = ((pointer_px.0 - *sx) / (*sw).max(1.0)).clamp(0.0, 1.0);
            if let Some(vm) = &self.runtime {
                vm.send(VmCommand::UiEvent {
                    widget_id: widget_id.clone(),
                    node_id: node_id.clone(),
                    kind: "change".into(),
                    value,
                });
            }
            return true;
        }
        if !self.editor.active || self.editor.drag_mode.is_none() {
            return false;
        }
        if let Some(id) = self.editor.selected_widget.clone()
            && let Some(cfg) = self.placements.get_mut(&id)
        {
            let neighbors: Vec<(f32, f32, f32, f32)> = self
                .resolved_rects
                .iter()
                .filter(|(k, r)| *k != &id && r.visible)
                .map(|(_, r)| (r.x, r.y, r.w, r.h))
                .collect();
            self.editor.update_drag_with_neighbors(
                cfg,
                pointer_px,
                win_size.0,
                win_size.1,
                scale,
                cell_size.0,
                cell_size.1,
                &neighbors,
            );
            return true;
        }
        false
    }

    pub fn handle_pointer_scroll(&mut self, pointer_px: (f32, f32), delta_y: f32) -> bool {
        if !self.config.enabled {
            return false;
        }
        for hit in self.hit_targets.iter().rev() {
            if hit.kind == HitKind::ScrollView
                && pointer_px.0 >= hit.x
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
                    vm.send(VmCommand::UiEvent {
                        widget_id: hit.widget_id.clone(),
                        node_id: hit.node_id.clone(),
                        kind: "scroll".into(),
                        value: delta_y,
                    });
                }
                return true;
            }
        }
        false
    }

    pub fn handle_pointer_release(&mut self) {
        self.active_slider = None;
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

    #[test]
    fn stale_actions_from_unloaded_generation_are_dropped() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("bg-ext");
        std::fs::create_dir(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("index.js"),
            "beyrix.background.clearScene(); beyrix.background.addObject({ id: 'o1', primitive: 'cube' });",
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("beyrix-ext.toml"),
            r#"
id = "org.example.bg"
name = "BG"
sdk_version = "1.0.0"
permissions = ["ui", "background-control"]
"#,
        )
        .unwrap();

        let mut mgr = WidgetManager::new(WidgetsConfig {
            enabled: true,
            ..Default::default()
        });
        mgr.ensure_started(Theme::default(), bus).unwrap();
        let loaded = ExtensionManifest::load_from_dir(&ext_dir).unwrap();
        mgr.register_extension(loaded, ExtensionTrust::ThirdParty)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));

        // Unload before draining actions: stale BackgroundClearScene/AddObject from gen 1
        // must be dropped and only BackgroundReleaseScene emitted!
        mgr.unload_extension("org.example.bg");
        let actions = mgr.drain_actions();
        assert!(actions
            .iter()
            .any(|a| matches!(a, HostAction::BackgroundReleaseScene { owner } if owner == "org.example.bg")));
        assert!(!actions.iter().any(|a| matches!(
            a,
            HostAction::BackgroundClearScene { .. } | HostAction::BackgroundAddObject { .. }
        )));
    }

    #[test]
    fn third_party_cannot_overwrite_host_official_extension_id() {
        let dir = tempfile::tempdir().unwrap();
        let off_dir = dir.path().join("off");
        let tp_dir = dir.path().join("tp");
        std::fs::create_dir(&off_dir).unwrap();
        std::fs::create_dir(&tp_dir).unwrap();
        for d in [&off_dir, &tp_dir] {
            std::fs::write(d.join("index.js"), "beyrix.log('hi');").unwrap();
            std::fs::write(
                d.join("beyrix-ext.toml"),
                "id = \"beyrix.official.kbd\"\nname = \"Kbd\"\nsdk_version = \"1.0.0\"\npermissions = [\"ui\", \"keyboard-layout\"]\n",
            )
            .unwrap();
        }
        let mut mgr = WidgetManager::new(WidgetsConfig::default());
        mgr.register_extension(
            ExtensionManifest::load_from_dir(&off_dir).unwrap(),
            ExtensionTrust::HostOfficial,
        )
        .unwrap();
        assert!(
            mgr.register_extension(
                ExtensionManifest::load_from_dir(&tp_dir).unwrap(),
                ExtensionTrust::ThirdParty,
            )
            .is_err()
        );
    }
}
