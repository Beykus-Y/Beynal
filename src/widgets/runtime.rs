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
    BackgroundSetPreset(String),
    BackgroundBind {
        source: String,
        target: String,
        min: f32,
        max: f32,
        smoothing: f32,
    },
    WidgetLog {
        extension_id: String,
        level: String,
        message: String,
    },
}

/// Snapshot of a widget's rendered UI tree and diagnostics.
#[derive(Clone, Debug)]
pub struct WidgetRenderState {
    pub id: String,
    pub tree: Option<UiNode>,
    pub last_error: Option<String>,
    pub visible: bool,
    pub updated_at: Instant,
}

/// Messages sent from UI / main thread to the JS worker thread.
pub enum VmCommand {
    LoadExtension {
        id: String,
        entry_path: PathBuf,
        manifest: Box<ExtensionManifest>,
        permissions: PermissionSet,
    },
    UnloadExtension {
        id: String,
    },
    ReloadExtension {
        id: String,
    },
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
    Shutdown,
}

/// Shared state readable by the main thread without blocking on JS execution.
#[derive(Default)]
pub struct SharedWidgetState {
    pub renders: HashMap<String, WidgetRenderState>,
    pub pending_actions: VecDeque<HostAction>,
    pub latest_keyboard: Option<KeyboardLayoutEvent>,
    pub latest_media: Option<MediaEventPayload>,
    pub latest_audio: Option<AudioFramePayload>,
    pub latest_xray: Option<XRaySummaryPayload>,
}

/// Handle to the background JS VM worker thread.
pub struct WidgetRuntime {
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
            tx,
            shared,
            shutdown_flag,
            worker: Some(worker),
        })
    }

    pub fn send(&self, cmd: VmCommand) {
        let _ = self.tx.try_send(cmd);
    }

    pub fn snapshot_renders(&self) -> HashMap<String, WidgetRenderState> {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        s.renders.clone()
    }

    pub fn drain_actions(&self) -> Vec<HostAction> {
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        s.pending_actions.drain(..).collect()
    }
}

impl Drop for WidgetRuntime {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(VmCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

struct LoadedVmExtension {
    id: String,
    entry_path: PathBuf,
    last_mtime: Option<SystemTime>,
    manifest: ExtensionManifest,
    permissions: PermissionSet,
    context: Context,
    storage: BTreeMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
fn run_vm_loop(
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
        // 1. Process incoming VM commands
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                VmCommand::Shutdown => return,
                VmCommand::UpdateTheme(t) => theme = t,
                VmCommand::UpdateWindow {
                    width,
                    height,
                    scale,
                    focused,
                } => win_state = (width, height, scale, focused),
                VmCommand::LoadExtension {
                    id,
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
                        entry_path,
                        manifest,
                        permissions,
                        &theme,
                        win_state,
                    );
                }
                VmCommand::UnloadExtension { id } => {
                    extensions.remove(&id);
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.renders.remove(&id);
                }
                VmCommand::ReloadExtension { id } => {
                    if let Some(ext) = extensions.get(&id).map(|e| {
                        (
                            e.entry_path.clone(),
                            Box::new(e.manifest.clone()),
                            e.permissions.clone(),
                        )
                    }) {
                        extensions.remove(&id);
                        load_into_vm(
                            &rt,
                            &mut extensions,
                            &shared,
                            &deadline,
                            timeout,
                            id,
                            ext.0,
                            ext.1,
                            ext.2,
                            &theme,
                            win_state,
                        );
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
            "if (globalThis.__beyrix_emit) {{ globalThis.__beyrix_emit('mediaSession', {{ sessionId: {}, source: {}, title: {}, artist: {}, album: {}, playing: {}, position: {}, duration: {} }}); }}",
            serde_json_str(&m.session_id),
            serde_json_str(&m.source),
            serde_json_str(&m.title),
            serde_json_str(&m.artist),
            serde_json_str(&m.album),
            m.playing,
            m.position_secs,
            m.duration_secs,
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
                    tree: None,
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
            handle_host_call(&eid, &perms, &sh, &action, &arg);
        });
        if let Ok(f) = dispatch_fn {
            let _ = globals.set("__beyrix_host_call", f);
        }
    });

    // Execute user extension code
    eval_with_timeout(&mut ext, deadline, timeout, &code, shared);
    sync_render_from_vm(&mut ext, deadline, timeout, shared);
    extensions.insert(id, ext);
}

fn handle_host_call(
    ext_id: &str,
    perms: &PermissionSet,
    shared: &Arc<Mutex<SharedWidgetState>>,
    action: &str,
    arg: &str,
) {
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    if s.pending_actions.len() >= 128 {
        s.pending_actions.pop_front();
    }
    match action {
        "log" => s.pending_actions.push_back(HostAction::WidgetLog {
            extension_id: ext_id.to_string(),
            level: "info".to_string(),
            message: arg.to_string(),
        }),
        "media.playPause" if perms.has(Capability::MediaControl) => {
            s.pending_actions.push_back(HostAction::MediaPlayPause);
        }
        "media.next" if perms.has(Capability::MediaControl) => {
            s.pending_actions.push_back(HostAction::MediaNext);
        }
        "media.prev" if perms.has(Capability::MediaControl) => {
            s.pending_actions.push_back(HostAction::MediaPrev);
        }
        "media.seek" if perms.has(Capability::MediaControl) => {
            if let Ok(secs) = arg.parse::<f64>() {
                s.pending_actions.push_back(HostAction::MediaSeek(secs));
            }
        }
        "media.setVolume" if perms.has(Capability::MediaControl) => {
            if let Ok(v) = arg.parse::<f64>() {
                s.pending_actions.push_back(HostAction::MediaSetVolume(v));
            }
        }
        "media.playFile" if perms.has(Capability::MediaControl) => {
            s.pending_actions
                .push_back(HostAction::MediaPlayFile(arg.to_string()));
        }
        "background.setPreset" if perms.has(Capability::BackgroundControl) => {
            s.pending_actions
                .push_back(HostAction::BackgroundSetPreset(arg.to_string()));
        }
        _ => {}
    }
}

fn eval_with_timeout(
    ext: &mut LoadedVmExtension,
    deadline: &Arc<Mutex<Option<Instant>>>,
    timeout: Duration,
    code: &str,
    shared: &Arc<Mutex<SharedWidgetState>>,
) {
    {
        let mut guard = deadline.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Instant::now() + timeout);
    }
    let res = ext.context.with(|ctx| ctx.eval::<(), _>(code));
    {
        let mut guard = deadline.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
    if let Err(err) = res {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let entry = s
            .renders
            .entry(ext.id.clone())
            .or_insert_with(|| WidgetRenderState {
                id: ext.id.clone(),
                tree: None,
                last_error: None,
                visible: true,
                updated_at: Instant::now(),
            });
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
    if let Some(json) = json_opt {
        let parsed: Result<UiNode, _> = serde_json::from_str(&json);
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        match parsed {
            Ok(tree) => {
                s.renders.insert(
                    ext.id.clone(),
                    WidgetRenderState {
                        id: ext.id.clone(),
                        tree: Some(tree),
                        last_error: None,
                        visible: true,
                        updated_at: Instant::now(),
                    },
                );
            }
            Err(e) => {
                let entry = s
                    .renders
                    .entry(ext.id.clone())
                    .or_insert_with(|| WidgetRenderState {
                        id: ext.id.clone(),
                        tree: None,
                        last_error: None,
                        visible: true,
                        updated_at: Instant::now(),
                    });
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
        scrollView(height, children, scrollY = 0) {{ return {{ kind: "scroll-view", height, children, scroll_y: scrollY }}; }},
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
    }},
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
        vm.send(VmCommand::LoadExtension {
            id: "test.loop".into(),
            entry_path: loaded.entry_path,
            manifest: Box::new(loaded.manifest),
            permissions: PermissionSet::new([Capability::Ui]),
        });
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
        vm.send(VmCommand::LoadExtension {
            id: "test.kbd".into(),
            entry_path: loaded.entry_path,
            manifest: Box::new(loaded.manifest),
            permissions: PermissionSet::new([Capability::Ui, Capability::KeyboardLayout]),
        });
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
    }
}
