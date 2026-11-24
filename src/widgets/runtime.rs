// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sandboxed JavaScript Runtime for BeyrixWidgets powered by `rquickjs` (QuickJS).
//!
//! Runs on a dedicated worker thread, completely isolated from PTY, input, and
//! rendering threads. Enforces memory limits, stack limits, execution timeouts
//! (interrupt handler), bounded event queues, capability-based permissions,
//! hot reload, and clean resource disposal on unload.

use rquickjs::{Context, Function, Runtime};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use crate::events::{
    AppEvent, AudioFramePayload, EventBus, EventEnvelope, EventKind, KeyboardLayoutEvent,
    MediaEventPayload, XRaySummaryPayload,
};
use crate::theme::Theme;

use super::manifest::{ExtensionManifest, SDK_VERSION};
use super::permissions::{Capability, PermissionSet};
use super::ui::UiNode;

/// Action sent from the JS runtime to host subsystems (Media, Background, X-Ray, etc.).
#[derive(Clone, Debug, PartialEq)]
pub enum HostAction {
    MediaPlayPause,
    MediaNext,
    MediaPrev,
    MediaSeek(f64),
    MediaSetVolume(f64),
    MediaPlayFile(String),
    MediaEnqueue(String),
    BackgroundSetPreset {
        owner: String,
        preset: String,
    },
    BackgroundReleaseScene {
        owner: String,
    },
    BackgroundClearScene {
        owner: String,
    },
    BackgroundAddObject {
        owner: String,
        spec: crate::background::CustomObjectSpec,
    },
    BackgroundUpdateObject {
        owner: String,
        id: String,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
        scale: Option<[f32; 3]>,
        color: Option<[f32; 3]>,
    },
    BackgroundRemoveObject {
        owner: String,
        id: String,
    },
    BackgroundSetCamera {
        owner: String,
        position: Option<[f32; 3]>,
        target: Option<[f32; 3]>,
        fov_deg: Option<f32>,
        ortho: Option<bool>,
    },
    BackgroundSetLight {
        owner: String,
        direction: Option<[f32; 3]>,
        color: Option<[f32; 3]>,
        intensity: Option<f32>,
    },
    BackgroundSetCharset {
        owner: String,
        charset: String,
    },
    BackgroundBind {
        owner: String,
        binding: crate::background::bindings::DataBinding,
    },
    BackgroundUnbind {
        owner: String,
        id: String,
    },
    WidgetLog {
        extension_id: String,
        level: String,
        message: String,
    },
}

/// Hard safety caps for extension-emitted UI trees so a faulty or malicious
/// extension cannot exhaust memory or stall the renderer.
pub const MAX_UI_NODES: usize = 512;
pub const MAX_UI_DEPTH: usize = 24;
pub const MAX_UI_JSON_BYTES: usize = 256 * 1024;

/// Snapshot of a widget's rendered UI tree and diagnostics.
#[derive(Clone, Debug)]
pub struct WidgetRenderState {
    pub id: String,
    pub generation: u64,
    pub tree: Option<UiNode>,
    pub node_count: usize,
    pub last_eval_us: u64,
    pub last_error: Option<String>,
    pub visible: bool,
    pub updated_at: Instant,
}

/// Action envelope tagged with extension ID and instance generation to drop stale
/// actions from unloaded or reloaded extension instances.
#[derive(Clone, Debug, PartialEq)]
pub struct HostActionEnvelope {
    pub extension_id: String,
    pub generation: u64,
    pub action: HostAction,
}

/// High-priority lifecycle control commands delivered over a dedicated
/// non-blocking unbounded channel so they are NEVER dropped and NEVER stall
/// the UI/PTY thread even if the bounded event queue is saturated.
pub enum LifecycleCommand {
    LoadExtension {
        id: String,
        generation: u64,
        entry_path: PathBuf,
        manifest: Box<ExtensionManifest>,
        permissions: PermissionSet,
    },
    UnloadExtension {
        id: String,
    },
    ReloadExtension {
        id: String,
        generation: u64,
    },
    UpdateSettings {
        id: String,
        settings_json: String,
    },
    Shutdown,
}

/// Best-effort bounded messages sent from UI / main thread to the JS worker thread.
pub enum VmCommand {
    UiEvent {
        widget_id: String,
        node_id: String,
        kind: String,
        value: f32,
    },
    UpdateTheme(Theme),
    UpdateWindow {
        width: i32,
        height: i32,
        scale: f32,
        focused: bool,
    },
}

/// Shared state readable by the main thread without blocking on JS execution.
#[derive(Default)]
pub struct SharedWidgetState {
    pub renders: HashMap<String, WidgetRenderState>,
    pub pending_actions: VecDeque<HostActionEnvelope>,
    pub latest_keyboard: Option<KeyboardLayoutEvent>,
    pub latest_media: Option<MediaEventPayload>,
    pub latest_audio: Option<AudioFramePayload>,
    pub latest_xray: Option<XRaySummaryPayload>,
    pub render_dirty: bool,
}

/// Handle to the background JS VM worker thread.
pub struct WidgetRuntime {
    lifecycle_tx: mpsc::Sender<LifecycleCommand>,
    tx: mpsc::SyncSender<VmCommand>,
    shared: Arc<Mutex<SharedWidgetState>>,
    shutdown_flag: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WidgetRuntime {
    pub fn start(
        memory_limit_mb: usize,
        stack_limit_kb: usize,
        execution_timeout_ms: u64,
        theme: Theme,
        bus: EventBus,
    ) -> Result<Self, String> {
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel::<LifecycleCommand>();
        let (tx, rx) = mpsc::sync_channel::<VmCommand>(128);
        let shared = Arc::new(Mutex::new(SharedWidgetState::default()));
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let shared_clone = Arc::clone(&shared);
        let shutdown_clone = Arc::clone(&shutdown_flag);
        let sub = bus.subscribe(
            Some(vec![
                EventKind::KeyboardLayout,
                EventKind::MediaSession,
                EventKind::AudioFrame,
                EventKind::XRayUpdate,
                EventKind::ThemeChanged,
                EventKind::WindowFocus,
                EventKind::WindowResize,
            ]),
            128,
        );

        let worker = std::thread::Builder::new()
            .name("beyrix-widgets-vm".to_string())
            .spawn(move || {
                run_vm_loop(
                    lifecycle_rx,
                    rx,
                    shared_clone,
                    shutdown_clone,
                    sub,
                    memory_limit_mb,
                    stack_limit_kb,
                    execution_timeout_ms,
                    theme,
                );
            })
            .map_err(|e| format!("spawn widget VM thread: {e}"))?;

        Ok(Self {
            lifecycle_tx,
            tx,
            shared,
            shutdown_flag,
            worker: Some(worker),
        })
    }

    pub fn send(&self, cmd: VmCommand) {
        let _ = self.tx.try_send(cmd);
    }

    /// Non-blocking, guaranteed delivery for lifecycle commands (`LoadExtension`,
    /// `UnloadExtension`, `ReloadExtension`, `UpdateSettings`, `Shutdown`).
    /// Never blocks the UI/PTY thread even if the bounded event channel is full,
    /// and returns `Err` if the VM worker thread has exited.
    pub fn send_lifecycle(&self, cmd: LifecycleCommand) -> Result<(), String> {
        self.lifecycle_tx
            .send(cmd)
            .map_err(|_| "widget VM worker disconnected".to_string())
    }

    pub fn is_alive(&self) -> bool {
        self.worker.as_ref().is_some_and(|h| !h.is_finished())
    }

    pub fn snapshot_renders(&self) -> HashMap<String, WidgetRenderState> {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        s.renders.clone()
    }

    pub fn drain_actions(&self) -> Vec<HostActionEnvelope> {
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        s.pending_actions.drain(..).collect()
    }

    pub fn take_render_dirty(&self) -> bool {
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut s.render_dirty, false)
    }
}

impl Drop for WidgetRuntime {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        let _ = self.lifecycle_tx.send(LifecycleCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

struct LoadedVmExtension {
    id: String,
    generation: u64,
    entry_path: PathBuf,
    last_mtime: Option<SystemTime>,
    manifest: ExtensionManifest,
    permissions: PermissionSet,
    context: Context,
    storage: BTreeMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
fn run_vm_loop(
    lifecycle_rx: mpsc::Receiver<LifecycleCommand>,
    rx: mpsc::Receiver<VmCommand>,
    shared: Arc<Mutex<SharedWidgetState>>,
    shutdown: Arc<AtomicBool>,
    event_sub: crate::events::EventSubscription,
    memory_limit_mb: usize,
    stack_limit_kb: usize,
    execution_timeout_ms: u64,
    mut theme: Theme,
) {
    let Ok(rt) = Runtime::new() else {
        return;
    };
    rt.set_memory_limit(memory_limit_mb.saturating_mul(1024 * 1024));
    rt.set_max_stack_size(stack_limit_kb.saturating_mul(1024));

    let deadline = Arc::new(Mutex::new(None::<Instant>));
    let deadline_cb = Arc::clone(&deadline);
    rt.set_interrupt_handler(Some(Box::new(move || {
        let guard = deadline_cb.lock().unwrap_or_else(|e| e.into_inner());
        guard.is_some_and(|dl| Instant::now() >= dl)
    })));

    let mut extensions: HashMap<String, LoadedVmExtension> = HashMap::new();
    let mut win_state = (1280i32, 720i32, 1.0f32, true);
    let timeout = Duration::from_millis(execution_timeout_ms.max(10));

    while !shutdown.load(Ordering::Relaxed) {
        // 1. Always drain high-priority lifecycle commands FIRST so Load/Unload/
        //    Reload/UpdateSettings/Shutdown are never delayed or dropped by a
        //    flooded UI event queue.
        while let Ok(cmd) = lifecycle_rx.try_recv() {
            match cmd {
                LifecycleCommand::Shutdown => return,
                LifecycleCommand::LoadExtension {
                    id,
                    generation,
                    entry_path,
                    manifest,
                    permissions,
                } => {
                    load_into_vm(
                        &rt,
                        &mut extensions,
                        &shared,
                        &deadline,
                        timeout,
                        id,
                        generation,
                        entry_path,
                        manifest,
                        permissions,
                        &theme,
                        win_state,
                    );
                }
                LifecycleCommand::UnloadExtension { id } => {
                    extensions.remove(&id);
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.renders.remove(&id);
                    s.pending_actions.retain(|env| env.extension_id != id);
                    s.render_dirty = true;
                }
                LifecycleCommand::ReloadExtension { id, generation } => {
                    if let Some(ext) = extensions.get(&id).map(|e| {
                        (
                            e.entry_path.clone(),
                            Box::new(e.manifest.clone()),
                            e.permissions.clone(),
                        )
                    }) {
                        extensions.remove(&id);
                        {
                            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                            s.renders.remove(&id);
                            s.pending_actions.retain(|env| env.extension_id != id);
                        }
                        load_into_vm(
                            &rt,
                            &mut extensions,
                            &shared,
                            &deadline,
                            timeout,
                            id,
                            generation,
                            ext.0,
                            ext.1,
                            ext.2,
                            &theme,
                            win_state,
                        );
                    }
                }
                LifecycleCommand::UpdateSettings { id, settings_json } => {
                    if let Some(ext) = extensions.get_mut(&id) {
                        let script = format!(
                            "if (globalThis.__beyrix_update_settings) {{ globalThis.__beyrix_update_settings({settings_json}); }}"
                        );
                        eval_with_timeout(ext, &deadline, timeout, &script, &shared);
                        sync_render_from_vm(ext, &deadline, timeout, &shared);
                    }
                }
            }
        }

        // 2. Process bounded best-effort VM commands (UI events, theme/window updates)
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                VmCommand::UpdateTheme(t) => theme = t,
                VmCommand::UpdateWindow {
                    width,
                    height,
                    scale,
                    focused,
                } => {
                    let changed = win_state.0 != width
                        || win_state.1 != height
                        || (win_state.2 - scale).abs() > 0.01
                        || win_state.3 != focused;
                    win_state = (width, height, scale, focused);
                    if changed {
                        let script = format!(
                            "if (globalThis.beyrix && globalThis.beyrix.window) {{ \
                                beyrix.window.width = {width}; \
                                beyrix.window.height = {height}; \
                                beyrix.window.scale = {scale}; \
                                beyrix.window.focused = {focused}; \
                                if (globalThis.__beyrix_emit) {{ \
                                    globalThis.__beyrix_emit('windowResize', {{ width: {width}, height: {height}, scale: {scale}, focused: {focused} }}); \
                                }} \
                            }}"
                        );
                        for ext in extensions.values_mut() {
                            if ext.permissions.has(Capability::Window) {
                                eval_with_timeout(ext, &deadline, timeout, &script, &shared);
                                sync_render_from_vm(ext, &deadline, timeout, &shared);
                            }
                        }
                    }
                }
                VmCommand::UiEvent {
                    widget_id,
                    node_id,
                    kind,
                    value,
                } => {
                    if let Some(ext) = extensions.get_mut(&widget_id) {
                        let script = format!(
                            "if (globalThis.__beyrix_dispatch_ui) {{ globalThis.__beyrix_dispatch_ui({}, {}, {}); }}",
                            serde_json_str(&node_id),
                            serde_json_str(&kind),
                            value
                        );
                        eval_with_timeout(ext, &deadline, timeout, &script, &shared);
                        sync_render_from_vm(ext, &deadline, timeout, &shared);
                    }
                }
            }
        }

        // 2. Drain EventBus events and dispatch to subscribed extensions
        let events = event_sub.drain(64);
        if !events.is_empty() {
            update_shared_snapshots(&shared, &events);
            for ext in extensions.values_mut() {
                for ev in &events {
                    if let Some(js_call) = event_to_js(ev, &ext.permissions) {
                        eval_with_timeout(ext, &deadline, timeout, &js_call, &shared);
                    }
                }
                sync_render_from_vm(ext, &deadline, timeout, &shared);
            }
        }

        // 3. Pump bounded JS timers (onTick) every ~100ms
        for ext in extensions.values_mut() {
            eval_with_timeout(
                ext,
                &deadline,
                timeout,
                "if (globalThis.__beyrix_pump_timers) { globalThis.__beyrix_pump_timers(); }",
                &shared,
            );
            sync_render_from_vm(ext, &deadline, timeout, &shared);
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn update_shared_snapshots(shared: &Arc<Mutex<SharedWidgetState>>, events: &[EventEnvelope]) {
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    for env in events {
        match &env.event {
            AppEvent::KeyboardLayout(k) => s.latest_keyboard = Some(k.clone()),
            AppEvent::MediaSession(m) => s.latest_media = Some(m.clone()),
            AppEvent::AudioFrame(a) => s.latest_audio = Some(a.clone()),
            AppEvent::XRayUpdate(x) => s.latest_xray = Some(x.clone()),
            _ => {}
        }
    }
}

fn event_to_js(env: &EventEnvelope, perms: &PermissionSet) -> Option<String> {
    match &env.event {
        AppEvent::KeyboardLayout(k) if perms.has(Capability::KeyboardLayout) => Some(format!(
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('keyboardLayout', {{ activeIndex: {}, activeName: {}, shortName: {}, layouts: [{}] }}); }}",
            k.active_index,
            serde_json_str(&k.active_name),
            serde_json_str(&k.short_name),
            k.layouts
                .iter()
                .map(|l| serde_json_str(l))
                .collect::<Vec<_>>()
                .join(",")
        )),
        AppEvent::MediaSession(m) if perms.has(Capability::MediaControl) => Some(format!(
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('mediaSession', {{ sessionId: {}, source: {}, title: {}, artist: {}, album: {}, artUrl: {}, playing: {}, position: {}, duration: {}, volume: {}, canPlay: {}, canPause: {}, canGoNext: {}, canGoPrevious: {}, canSeek: {}, availableSessions: [{}] }}); }}",
            serde_json_str(&m.session_id),
            serde_json_str(&m.source),
            serde_json_str(&m.title),
            serde_json_str(&m.artist),
            serde_json_str(&m.album),
            m.art_url
                .as_deref()
                .map_or_else(|| "null".to_string(), serde_json_str),
            m.playing,
            m.position_secs,
            m.duration_secs,
            m.volume
                .map_or_else(|| "null".to_string(), |v| format!("{v:.3}")),
            m.can_play,
            m.can_pause,
            m.can_go_next,
            m.can_go_previous,
            m.can_seek,
            m.available_sessions
                .iter()
                .map(|s| serde_json_str(s))
                .collect::<Vec<_>>()
                .join(","),
        )),
        AppEvent::AudioFrame(a) if perms.has(Capability::AudioAnalysis) => Some(format!(
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('audioFrame', {{ amplitude: {}, rms: {}, peak: {}, beat: {}, bass: {}, mid: {}, treble: {} }}); }}",
            a.amplitude, a.rms, a.peak, a.beat, a.bass, a.mid, a.treble
        )),
        AppEvent::XRayUpdate(x) if perms.has(Capability::XRayRead) => Some(format!(
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('xrayUpdate', {{ rootPid: {}, processCount: {}, cpuPercent: {}, rssBytes: {}, stateLabel: {}, waitingReason: {} }}); }}",
            x.root_pid,
            x.process_count,
            x.cpu_percent,
            x.rss_bytes,
            serde_json_str(&x.state_label),
            serde_json_str(&x.waiting_reason)
        )),
        AppEvent::ThemeChanged { name } if perms.has(Capability::Theme) => Some(format!(
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('themeChanged', {{ name: {} }}); }}",
            serde_json_str(name)
        )),
        AppEvent::WindowFocus(focused) if perms.has(Capability::Window) => Some(format!(
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('windowFocus', {{ focused: {focused} }}); }}"
        )),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn load_into_vm(
    rt: &Runtime,
    extensions: &mut HashMap<String, LoadedVmExtension>,
    shared: &Arc<Mutex<SharedWidgetState>>,
    deadline: &Arc<Mutex<Option<Instant>>>,
    timeout: Duration,
    id: String,
    generation: u64,
    entry_path: PathBuf,
    manifest: Box<ExtensionManifest>,
    permissions: PermissionSet,
    theme: &Theme,
    win_state: (i32, i32, f32, bool),
) {
    let Ok(context) = Context::full(rt) else {
        return;
    };
    let mtime = std::fs::metadata(&entry_path)
        .and_then(|m| m.modified())
        .ok();
    let code = match std::fs::read_to_string(&entry_path) {
        Ok(c) => c,
        Err(e) => {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.renders.insert(
                id.clone(),
                WidgetRenderState {
                    id,
                    generation,
                    tree: None,
                    node_count: 0,
                    last_eval_us: 0,
                    last_error: Some(format!("read entry: {e}")),
                    visible: false,
                    updated_at: Instant::now(),
                },
            );
            return;
        }
    };

    let mut ext = LoadedVmExtension {
        id: id.clone(),
        generation,
        entry_path,
        last_mtime: mtime,
        manifest: *manifest,
        permissions: permissions.clone(),
        context,
        storage: BTreeMap::new(),
    };

    // Inject SDK preamble & host bridge
    let preamble = build_sdk_preamble(&id, &permissions, theme, win_state);
    eval_with_timeout(&mut ext, deadline, timeout, &preamble, shared);

    // Register native bridge functions
    let shared_bridge = Arc::clone(shared);
    let ext_id = id.clone();
    let perms_bridge = permissions.clone();
    ext.context.with(|ctx| {
        let globals = ctx.globals();
        let sh = Arc::clone(&shared_bridge);
        let eid = ext_id.clone();
        let perms = perms_bridge.clone();
        let dispatch_fn = Function::new(ctx.clone(), move |action: String, arg: String| {
            handle_host_call(&eid, generation, &perms, &sh, &action, &arg);
        });
        if let Ok(f) = dispatch_fn {
            let _ = globals.set("__beyrix_host_call", f);
        }
    });

    // Execute user extension code
    eval_with_timeout(&mut ext, deadline, timeout, &code, shared);

    // Replay latest keyboard layout & media snapshots immediately so newly
    // loaded widgets display accurate initial state without waiting for the
    // first layout switch or media poll.
    let (init_kbd, init_media) = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        (s.latest_keyboard.clone(), s.latest_media.clone())
    };
    if let Some(k) = init_kbd
        && let Some(js) = event_to_js(
            &EventEnvelope {
                seq: 0,
                timestamp: Instant::now(),
                wall_time: SystemTime::now(),
                event: AppEvent::KeyboardLayout(k),
            },
            &ext.permissions,
        )
    {
        eval_with_timeout(&mut ext, deadline, timeout, &js, shared);
    }
    if let Some(m) = init_media
        && let Some(js) = event_to_js(
            &EventEnvelope {
                seq: 0,
                timestamp: Instant::now(),
                wall_time: SystemTime::now(),
                event: AppEvent::MediaSession(m),
            },
            &ext.permissions,
        )
    {
        eval_with_timeout(&mut ext, deadline, timeout, &js, shared);
    }

    sync_render_from_vm(&mut ext, deadline, timeout, shared);
    extensions.insert(id, ext);
}

fn handle_host_call(
    ext_id: &str,
    generation: u64,
    perms: &PermissionSet,
    shared: &Arc<Mutex<SharedWidgetState>>,
    action: &str,
    arg: &str,
) {
    let parsed: Option<HostAction> = match action {
        "log" => Some(HostAction::WidgetLog {
            extension_id: ext_id.to_string(),
            level: "info".to_string(),
            message: arg.to_string(),
        }),
        "media.playPause" if perms.has(Capability::MediaControl) => {
            Some(HostAction::MediaPlayPause)
        }
        "media.next" if perms.has(Capability::MediaControl) => Some(HostAction::MediaNext),
        "media.prev" if perms.has(Capability::MediaControl) => Some(HostAction::MediaPrev),
        "media.seek" if perms.has(Capability::MediaControl) => {
            arg.parse::<f64>().ok().map(HostAction::MediaSeek)
        }
        "media.setVolume" if perms.has(Capability::MediaControl) => {
            arg.parse::<f64>().ok().map(HostAction::MediaSetVolume)
        }
        "media.playFile" if perms.has(Capability::MediaControl) => {
            Some(HostAction::MediaPlayFile(arg.to_string()))
        }
        "background.setPreset" if perms.has(Capability::BackgroundControl) => {
            Some(HostAction::BackgroundSetPreset {
                owner: ext_id.to_string(),
                preset: arg.to_string(),
            })
        }
        "background.releaseScene" if perms.has(Capability::BackgroundControl) => {
            Some(HostAction::BackgroundReleaseScene {
                owner: ext_id.to_string(),
            })
        }
        "background.clearScene" if perms.has(Capability::BackgroundControl) => {
            Some(HostAction::BackgroundClearScene {
                owner: ext_id.to_string(),
            })
        }
        "background.addObject" if perms.has(Capability::BackgroundControl) => {
            serde_json::from_str::<crate::background::CustomObjectSpec>(arg)
                .ok()
                .map(|spec| HostAction::BackgroundAddObject {
                    owner: ext_id.to_string(),
                    spec,
                })
        }
        "background.updateObject" if perms.has(Capability::BackgroundControl) => {
            #[derive(serde::Deserialize)]
            struct Upd {
                id: String,
                position: Option<[f32; 3]>,
                rotation: Option<[f32; 3]>,
                scale: Option<[f32; 3]>,
                color: Option<[f32; 3]>,
            }
            serde_json::from_str::<Upd>(arg)
                .ok()
                .map(|u| HostAction::BackgroundUpdateObject {
                    owner: ext_id.to_string(),
                    id: u.id,
                    position: u.position,
                    rotation: u.rotation,
                    scale: u.scale,
                    color: u.color,
                })
        }
        "background.removeObject" if perms.has(Capability::BackgroundControl) => {
            Some(HostAction::BackgroundRemoveObject {
                owner: ext_id.to_string(),
                id: arg.to_string(),
            })
        }
        "background.setCamera" if perms.has(Capability::BackgroundControl) => {
            #[derive(serde::Deserialize)]
            struct Cam {
                position: Option<[f32; 3]>,
                target: Option<[f32; 3]>,
                fov: Option<f32>,
                ortho: Option<bool>,
            }
            serde_json::from_str::<Cam>(arg)
                .ok()
                .map(|c| HostAction::BackgroundSetCamera {
                    owner: ext_id.to_string(),
                    position: c.position,
                    target: c.target,
                    fov_deg: c.fov,
                    ortho: c.ortho,
                })
        }
        "background.setLight" if perms.has(Capability::BackgroundControl) => {
            #[derive(serde::Deserialize)]
            struct Light {
                direction: Option<[f32; 3]>,
                color: Option<[f32; 3]>,
                intensity: Option<f32>,
            }
            serde_json::from_str::<Light>(arg)
                .ok()
                .map(|l| HostAction::BackgroundSetLight {
                    owner: ext_id.to_string(),
                    direction: l.direction,
                    color: l.color,
                    intensity: l.intensity,
                })
        }
        "background.setCharset" if perms.has(Capability::BackgroundControl) => {
            Some(HostAction::BackgroundSetCharset {
                owner: ext_id.to_string(),
                charset: arg.to_string(),
            })
        }
        "background.bind" if perms.has(Capability::BackgroundControl) => {
            serde_json::from_str::<crate::background::bindings::DataBinding>(arg)
                .ok()
                .map(|binding| HostAction::BackgroundBind {
                    owner: ext_id.to_string(),
                    binding,
                })
        }
        "background.unbind" if perms.has(Capability::BackgroundControl) => {
            Some(HostAction::BackgroundUnbind {
                owner: ext_id.to_string(),
                id: arg.to_string(),
            })
        }
        _ => None,
    };

    if let Some(act) = parsed {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        if s.pending_actions.len() >= 128 {
            s.pending_actions.pop_front();
        }
        s.pending_actions.push_back(HostActionEnvelope {
            extension_id: ext_id.to_string(),
            generation,
            action: act,
        });
    }
}

fn count_ui_nodes(node: &UiNode, depth: usize, count: &mut usize) -> Result<(), String> {
    if depth > MAX_UI_DEPTH {
        return Err(format!("UI tree exceeds max depth {MAX_UI_DEPTH}"));
    }
    *count += 1;
    if *count > MAX_UI_NODES {
        return Err(format!("UI tree exceeds max node count {MAX_UI_NODES}"));
    }
    match node {
        UiNode::Container { child, .. } => count_ui_nodes(child, depth + 1, count),
        UiNode::Row { children, .. }
        | UiNode::Column { children, .. }
        | UiNode::Grid { children, .. }
        | UiNode::Stack { children }
        | UiNode::ScrollView { children, .. } => {
            for c in children {
                count_ui_nodes(c, depth + 1, count)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn eval_with_timeout(
    ext: &mut LoadedVmExtension,
    deadline: &Arc<Mutex<Option<Instant>>>,
    timeout: Duration,
    code: &str,
    shared: &Arc<Mutex<SharedWidgetState>>,
) {
    let t0 = Instant::now();
    {
        let mut guard = deadline.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Instant::now() + timeout);
    }
    let res = ext.context.with(|ctx| ctx.eval::<(), _>(code));
    {
        let mut guard = deadline.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
    let elapsed_us = t0.elapsed().as_micros() as u64;
    if let Err(err) = res {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let entry = s
            .renders
            .entry(ext.id.clone())
            .or_insert_with(|| WidgetRenderState {
                id: ext.id.clone(),
                generation: ext.generation,
                tree: None,
                node_count: 0,
                last_eval_us: elapsed_us,
                last_error: None,
                visible: true,
                updated_at: Instant::now(),
            });
        entry.generation = ext.generation;
        entry.last_eval_us = elapsed_us;
        entry.last_error = Some(format!("{err:?}"));
        entry.updated_at = Instant::now();
    }
}

fn sync_render_from_vm(
    ext: &mut LoadedVmExtension,
    deadline: &Arc<Mutex<Option<Instant>>>,
    timeout: Duration,
    shared: &Arc<Mutex<SharedWidgetState>>,
) {
    if !ext.permissions.has(Capability::Ui) {
        return;
    }
    let t0 = Instant::now();
    {
        let mut guard = deadline.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Instant::now() + timeout);
    }
    let json_opt: Option<String> = ext.context.with(|ctx| {
        ctx.eval::<Option<String>, _>(
            "globalThis.__beyrix_dirty ? (globalThis.__beyrix_dirty = false, JSON.stringify(globalThis.__beyrix_tree)) : null",
        )
        .ok()
        .flatten()
    });
    {
        let mut guard = deadline.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
    let elapsed_us = t0.elapsed().as_micros() as u64;
    if let Some(json) = json_opt {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        if json.len() > MAX_UI_JSON_BYTES {
            let entry = s
                .renders
                .entry(ext.id.clone())
                .or_insert_with(|| WidgetRenderState {
                    id: ext.id.clone(),
                    generation: ext.generation,
                    tree: None,
                    node_count: 0,
                    last_eval_us: elapsed_us,
                    last_error: None,
                    visible: true,
                    updated_at: Instant::now(),
                });
            entry.generation = ext.generation;
            entry.last_error = Some(format!(
                "UI payload ({} bytes) exceeds limit ({MAX_UI_JSON_BYTES} bytes)",
                json.len()
            ));
            return;
        }
        let parsed: Result<UiNode, _> = serde_json::from_str(&json);
        match parsed {
            Ok(tree) => {
                let mut node_count = 0usize;
                if let Err(limit_err) = count_ui_nodes(&tree, 0, &mut node_count) {
                    let entry =
                        s.renders
                            .entry(ext.id.clone())
                            .or_insert_with(|| WidgetRenderState {
                                id: ext.id.clone(),
                                generation: ext.generation,
                                tree: None,
                                node_count: 0,
                                last_eval_us: elapsed_us,
                                last_error: None,
                                visible: true,
                                updated_at: Instant::now(),
                            });
                    entry.generation = ext.generation;
                    entry.last_error = Some(limit_err);
                    return;
                }
                s.renders.insert(
                    ext.id.clone(),
                    WidgetRenderState {
                        id: ext.id.clone(),
                        generation: ext.generation,
                        tree: Some(tree),
                        node_count,
                        last_eval_us: elapsed_us,
                        last_error: None,
                        visible: true,
                        updated_at: Instant::now(),
                    },
                );
                s.render_dirty = true;
            }
            Err(e) => {
                let entry = s
                    .renders
                    .entry(ext.id.clone())
                    .or_insert_with(|| WidgetRenderState {
                        id: ext.id.clone(),
                        generation: ext.generation,
                        tree: None,
                        node_count: 0,
                        last_eval_us: elapsed_us,
                        last_error: None,
                        visible: true,
                        updated_at: Instant::now(),
                    });
                entry.generation = ext.generation;
                entry.last_error = Some(format!("invalid UI tree: {e}"));
            }
        }
    }
}

fn build_sdk_preamble(
    ext_id: &str,
    perms: &PermissionSet,
    theme: &Theme,
    win_state: (i32, i32, f32, bool),
) -> String {
    let has = |c: Capability| perms.has(c);
    format!(
        r#"
globalThis.__beyrix_tree = null;
globalThis.__beyrix_dirty = false;
globalThis.__beyrix_listeners = {{}};
globalThis.__beyrix_ui_handlers = {{}};
globalThis.__beyrix_timers = [];
globalThis.__beyrix_next_timer = 1;

globalThis.__beyrix_emit = function(event, payload) {{
    const list = globalThis.__beyrix_listeners[event];
    if (list) {{
        for (let i = 0; i < list.length; i++) {{
            try {{ list[i](payload); }} catch (e) {{ beyrix.log("event error: " + e); }}
        }}
    }}
}};

globalThis.__beyrix_dispatch_ui = function(nodeId, kind, value) {{
    const fn = globalThis.__beyrix_ui_handlers[nodeId];
    if (fn) {{
        try {{ fn({{ id: nodeId, kind, value }}); }} catch (e) {{ beyrix.log("ui handler error: " + e); }}
    }}
}};

globalThis.__beyrix_pump_timers = function() {{
    const now = Date.now();
    const keep = [];
    for (let i = 0; i < globalThis.__beyrix_timers.length; i++) {{
        const t = globalThis.__beyrix_timers[i];
        if (now >= t.at) {{
            try {{ t.fn(); }} catch (e) {{ beyrix.log("timer error: " + e); }}
            if (t.interval > 0) {{
                t.at = now + t.interval;
                keep.push(t);
            }}
        }} else {{
            keep.push(t);
        }}
    }}
    globalThis.__beyrix_timers = keep;
}};

function requirePerm(ok, name) {{
    if (!ok) throw new Error("BeyrixSDK permission denied: " + name);
}}

globalThis.beyrix = {{
    sdkVersion: {sdk},
    extensionId: {id},
    log(msg) {{ if (globalThis.__beyrix_host_call) __beyrix_host_call("log", String(msg)); }},
    events: {{
        on(event, cb) {{
            if (!globalThis.__beyrix_listeners[event]) globalThis.__beyrix_listeners[event] = [];
            if (globalThis.__beyrix_listeners[event].length < 32) {{
                globalThis.__beyrix_listeners[event].push(cb);
            }}
        }},
    }},
    timers: {{
        setTimeout(fn, ms) {{
            if (globalThis.__beyrix_timers.length >= 32) return 0;
            const id = globalThis.__beyrix_next_timer++;
            globalThis.__beyrix_timers.push({{ id, fn, at: Date.now() + Math.max(10, ms|0), interval: 0 }});
            return id;
        }},
        setInterval(fn, ms) {{
            if (globalThis.__beyrix_timers.length >= 32) return 0;
            const id = globalThis.__beyrix_next_timer++;
            const iv = Math.max(50, ms|0);
            globalThis.__beyrix_timers.push({{ id, fn, at: Date.now() + iv, interval: iv }});
            return id;
        }},
        clearTimer(id) {{
            globalThis.__beyrix_timers = globalThis.__beyrix_timers.filter(t => t.id !== id);
        }},
    }},
    ui: {{
        render(tree) {{
            requirePerm({perm_ui}, "ui");
            globalThis.__beyrix_tree = tree;
            globalThis.__beyrix_dirty = true;
        }},
        text(text, opts = {{}}) {{ return {{ kind: "text", text: String(text), ...opts }}; }},
        icon(name, opts = {{}}) {{ return {{ kind: "icon", name: String(name), ...opts }}; }},
        button(id, label, onClick, opts = {{}}) {{
            if (onClick) globalThis.__beyrix_ui_handlers[id] = onClick;
            return {{ kind: "button", id, label: String(label), ...opts }};
        }},
        container(child, opts = {{}}) {{ return {{ kind: "container", child, ...opts }}; }},
        row(children, opts = {{}}) {{ return {{ kind: "row", children, ...opts }}; }},
        column(children, opts = {{}}) {{ return {{ kind: "column", children, ...opts }}; }},
        grid(columns, children, opts = {{}}) {{ return {{ kind: "grid", columns, children, ...opts }}; }},
        stack(children) {{ return {{ kind: "stack", children }}; }},
        progress(value, opts = {{}}) {{ return {{ kind: "progress", value: Number(value), ...opts }}; }},
        slider(id, value, onChange, opts = {{}}) {{
            if (onChange) globalThis.__beyrix_ui_handlers[id] = onChange;
            return {{ kind: "slider", id, value: Number(value), ...opts }};
        }},
        canvas(width, height, commands) {{ return {{ kind: "canvas", width, height, commands }}; }},
        scrollView(height, children, scrollY = 0, id = "", onScroll = null) {{
            if (id && onScroll) globalThis.__beyrix_ui_handlers[id] = onScroll;
            return {{ kind: "scroll-view", id, height, children, scroll_y: scrollY }};
        }},
        image(id, width, height, opts = {{}}) {{
            return {{ kind: "image", id: Number(id), width: Number(width), height: Number(height), ...opts }};
        }},
        toggle(id, label, checked, onToggle, opts = {{}}) {{
            if (onToggle) globalThis.__beyrix_ui_handlers[id] = onToggle;
            return {{ kind: "toggle", id, label: String(label), checked: Boolean(checked), ...opts }};
        }},
        checkbox(id, label, checked, onToggle, opts = {{}}) {{
            if (onToggle) globalThis.__beyrix_ui_handlers[id] = onToggle;
            return {{ kind: "checkbox", id, label: String(label), checked: Boolean(checked), ...opts }};
        }},
        dropdown(id, options, selected, onCycle, opts = {{}}) {{
            if (onCycle) globalThis.__beyrix_ui_handlers[id] = onCycle;
            return {{ kind: "dropdown", id, options: Array.from(options).map(String), selected: Number(selected), ...opts }};
        }},
        tabs(id, tabs, active, onSelect) {{
            if (onSelect) globalThis.__beyrix_ui_handlers[id] = onSelect;
            return {{ kind: "tabs", id, tabs: Array.from(tabs).map(String), active: Number(active) }};
        }},
        badge(text, opts = {{}}) {{ return {{ kind: "badge", text: String(text), ...opts }}; }},
        divider(opts = {{}}) {{ return {{ kind: "divider", ...opts }}; }},
        sparkline(values, width, height, opts = {{}}) {{
            return {{ kind: "sparkline", values: Array.from(values).map(Number), width: Number(width), height: Number(height), ...opts }};
        }},
    }},
    keyboard: {{
        onLayoutChange(cb) {{
            requirePerm({perm_kbd}, "keyboard-layout");
            beyrix.events.on("keyboardLayout", cb);
        }},
    }},
    media: {{
        onSessionChange(cb) {{
            requirePerm({perm_media}, "media-control");
            beyrix.events.on("mediaSession", cb);
        }},
        playPause() {{ requirePerm({perm_media}, "media-control"); __beyrix_host_call("media.playPause", ""); }},
        next() {{ requirePerm({perm_media}, "media-control"); __beyrix_host_call("media.next", ""); }},
        prev() {{ requirePerm({perm_media}, "media-control"); __beyrix_host_call("media.prev", ""); }},
        seek(secs) {{ requirePerm({perm_media}, "media-control"); __beyrix_host_call("media.seek", String(secs)); }},
        setVolume(v) {{ requirePerm({perm_media}, "media-control"); __beyrix_host_call("media.setVolume", String(v)); }},
        playFile(path) {{ requirePerm({perm_media}, "media-control"); __beyrix_host_call("media.playFile", String(path)); }},
    }},
    audio: {{
        onFrame(cb) {{
            requirePerm({perm_audio}, "audio-analysis");
            beyrix.events.on("audioFrame", cb);
        }},
    }},
    background: {{
        setPreset(name) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.setPreset", String(name));
        }},
        clearScene() {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.clearScene", "");
        }},
        releaseScene() {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.releaseScene", "");
        }},
        addObject(spec) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.addObject", JSON.stringify(spec));
        }},
        updateObject(id, patch) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.updateObject", JSON.stringify({{ id, ...patch }}));
        }},
        removeObject(id) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.removeObject", String(id));
        }},
        setCamera(cam) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.setCamera", JSON.stringify(cam));
        }},
        setLight(light) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.setLight", JSON.stringify(light));
        }},
        setCharset(charset) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.setCharset", String(charset));
        }},
        bind(binding) {{
            requirePerm({perm_bg}, "background-control");
            const payload = {{
                id: String(binding.id || "bind"),
                object_id: binding.objectId ? String(binding.objectId) : null,
                source: binding.source,
                target: binding.target,
                in_min: Number(binding.inMin ?? 0.0),
                in_max: Number(binding.inMax ?? 1.0),
                out_min: Number(binding.outMin ?? 0.0),
                out_max: Number(binding.outMax ?? 1.0),
                smoothing: Number(binding.smoothing ?? 0.35),
                combine: binding.combine || "replace",
            }};
            __beyrix_host_call("background.bind", JSON.stringify(payload));
        }},
        unbind(id) {{
            requirePerm({perm_bg}, "background-control");
            __beyrix_host_call("background.unbind", String(id));
        }},
    }},
    xray: {{
        onUpdate(cb) {{
            requirePerm({perm_xray}, "xray-read");
            beyrix.events.on("xrayUpdate", cb);
        }},
    }},
    theme: {{
        name: {theme_name},
        onChange(cb) {{
            requirePerm({perm_theme}, "theme");
            beyrix.events.on("themeChanged", cb);
        }},
    }},
    window: {{
        width: {win_w},
        height: {win_h},
        scale: {win_scale},
        focused: {win_focus},
        onResize(cb) {{
            requirePerm({perm_win}, "window");
            beyrix.events.on("windowResize", cb);
        }},
        onFocus(cb) {{
            requirePerm({perm_win}, "window");
            beyrix.events.on("windowFocus", cb);
        }},
    }},
    settings: {{
        _values: {{}},
        get(key, fallback) {{
            return Object.prototype.hasOwnProperty.call(this._values, key) ? this._values[key] : fallback;
        }},
        getAll() {{
            return Object.assign({{}}, this._values);
        }},
        onChange(cb) {{
            beyrix.events.on("settingsChanged", cb);
        }},
    }},
}};
globalThis.__beyrix_update_settings = function(values) {{
    if (values && typeof values === "object") {{
        beyrix.settings._values = Object.assign({{}}, beyrix.settings._values, values);
        globalThis.__beyrix_emit("settingsChanged", beyrix.settings.getAll());
    }}
}};
globalThis.console = {{ log: (...a) => beyrix.log(a.join(" ")), error: (...a) => beyrix.log("ERROR: " + a.join(" ")) }};
"#,
        sdk = serde_json_str(SDK_VERSION),
        id = serde_json_str(ext_id),
        perm_ui = has(Capability::Ui),
        perm_kbd = has(Capability::KeyboardLayout),
        perm_media = has(Capability::MediaControl),
        perm_audio = has(Capability::AudioAnalysis),
        perm_bg = has(Capability::BackgroundControl),
        perm_xray = has(Capability::XRayRead),
        perm_theme = has(Capability::Theme),
        perm_win = has(Capability::Window),
        theme_name = serde_json_str(&theme.name),
        win_w = win_state.0,
        win_h = win_state.1,
        win_scale = win_state.2,
        win_focus = win_state.3,
    )
}

fn serde_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::KeyboardLayoutEvent;

    #[test]
    fn vm_interrupts_infinite_loop_and_isolates_errors() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("index.js");
        std::fs::write(&entry, "while (true) {}").unwrap();
        std::fs::write(
            dir.path().join("beyrix-ext.toml"),
            "id = \"test.loop\"\nname = \"Loop\"\nsdk_version = \"1.0.0\"\n",
        )
        .unwrap();

        let loaded = ExtensionManifest::load_from_dir(dir.path()).unwrap();
        let vm = WidgetRuntime::start(8, 256, 40, Theme::default(), bus).unwrap();
        vm.send_lifecycle(LifecycleCommand::LoadExtension {
            id: "test.loop".into(),
            generation: 1,
            entry_path: loaded.entry_path,
            manifest: Box::new(loaded.manifest),
            permissions: PermissionSet::new([Capability::Ui]),
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(160));
        let renders = vm.snapshot_renders();
        let st = renders.get("test.loop").expect("error recorded");
        assert!(st.last_error.is_some());
    }

    #[test]
    fn vm_renders_widget_ui_on_keyboard_layout_event() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("index.js");
        std::fs::write(
            &entry,
            r#"
beyrix.keyboard.onLayoutChange((ev) => {
    beyrix.ui.render(beyrix.ui.container(
        beyrix.ui.text(ev.shortName, { size: 14 }),
        { padding: 8, radius: 6, bg: "card" }
    ));
});
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("beyrix-ext.toml"),
            "id = \"test.kbd\"\nname = \"Kbd\"\nsdk_version = \"1.0.0\"\n",
        )
        .unwrap();

        let loaded = ExtensionManifest::load_from_dir(dir.path()).unwrap();
        let vm = WidgetRuntime::start(16, 256, 50, Theme::default(), bus.clone()).unwrap();
        vm.send_lifecycle(LifecycleCommand::LoadExtension {
            id: "test.kbd".into(),
            generation: 1,
            entry_path: loaded.entry_path,
            manifest: Box::new(loaded.manifest),
            permissions: PermissionSet::new([Capability::Ui, Capability::KeyboardLayout]),
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(80));
        bus.publish(AppEvent::KeyboardLayout(KeyboardLayoutEvent {
            active_index: 1,
            active_name: "Russian".into(),
            short_name: "RU".into(),
            layouts: vec!["English (US)".into(), "Russian".into()],
        }));
        std::thread::sleep(Duration::from_millis(120));
        let renders = vm.snapshot_renders();
        let st = renders.get("test.kbd").expect("rendered");
        assert!(st.last_error.is_none(), "err: {:?}", st.last_error);
        assert!(st.tree.is_some());
        assert_eq!(st.generation, 1);
    }

    #[test]
    fn lifecycle_commands_never_block_or_drop_when_event_queue_is_flooded() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.js"),
            "beyrix.ui.render(beyrix.ui.text('ok'));",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("beyrix-ext.toml"),
            "id = \"test.flood\"\nname = \"Flood\"\nsdk_version = \"1.0.0\"\n",
        )
        .unwrap();
        let loaded = ExtensionManifest::load_from_dir(dir.path()).unwrap();
        let vm = WidgetRuntime::start(16, 256, 50, Theme::default(), bus).unwrap();

        // Completely saturate the bounded UI event channel.
        for i in 0..512 {
            vm.send(VmCommand::UiEvent {
                widget_id: "nonexistent".into(),
                node_id: format!("n{i}"),
                kind: "click".into(),
                value: 1.0,
            });
        }

        // Lifecycle load must return immediately (non-blocking) and succeed!
        let t0 = Instant::now();
        vm.send_lifecycle(LifecycleCommand::LoadExtension {
            id: "test.flood".into(),
            generation: 7,
            entry_path: loaded.entry_path,
            manifest: Box::new(loaded.manifest),
            permissions: PermissionSet::new([Capability::Ui]),
        })
        .unwrap();
        assert!(t0.elapsed() < Duration::from_millis(20));

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(st) = vm.snapshot_renders().get("test.flood")
                && st.generation == 7
                && st.tree.is_some()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("lifecycle load was delayed or dropped under event flood");
    }

    #[test]
    fn excessive_ui_node_tree_is_rejected_without_crashing_runtime() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.js"),
            "const items = []; for (let i = 0; i < 700; i++) items.push(beyrix.ui.text('x')); beyrix.ui.render(beyrix.ui.column(items));",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("beyrix-ext.toml"),
            "id = \"test.huge\"\nname = \"Huge\"\nsdk_version = \"1.0.0\"\n",
        )
        .unwrap();
        let loaded = ExtensionManifest::load_from_dir(dir.path()).unwrap();
        let vm = WidgetRuntime::start(16, 256, 100, Theme::default(), bus).unwrap();
        vm.send_lifecycle(LifecycleCommand::LoadExtension {
            id: "test.huge".into(),
            generation: 1,
            entry_path: loaded.entry_path,
            manifest: Box::new(loaded.manifest),
            permissions: PermissionSet::new([Capability::Ui]),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(st) = vm.snapshot_renders().get("test.huge")
                && st
                    .last_error
                    .as_deref()
                    .is_some_and(|e| e.contains("max node count"))
            {
                assert!(st.tree.is_none());
                assert!(vm.is_alive());
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("excessive UI tree was not rejected");
    }
}
