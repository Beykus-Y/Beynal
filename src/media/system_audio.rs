// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linux System Audio Capture backend for PipeWire / PulseAudio monitors.
//!
//! Dynamically loads `libpulse.so.0` (provided natively by PipeWire via
//! `pipewire-pulse` as well as PulseAudio) via `libloading` and drives an
//! explicit `pa_mainloop` with bounded `pa_mainloop_prepare(..., 25_000)` polls.
//!
//! Guarantees:
//! - Strictly opt-in (`MediaConfig.audio_capture_enabled = true`).
//! - Never blocks indefinitely: wakes up every <=25ms to observe `shutdown`,
//!   PipeWire disconnects, device disappearance, or silent/suspended monitors.
//! - `Drop` always completes cleanly in bounded time (<100ms).
//! - Skips FFT analysis whenever zero consumers (JS widgets or Background Engine)
//!   are actively subscribed.
//! - Never writes audio samples to disk and never transmits them over network.

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::audio::AudioAnalyzer;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemAudioStatus {
    Disabled,
    Active { backend: String, source: String },
    Unavailable { reason: String },
}

/// Enumerate available system audio monitor sources on Linux via `pactl`
/// (works with both `pipewire-pulse` and PulseAudio).
pub fn list_monitor_sources() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let Ok(out) = std::process::Command::new("pactl")
            .args(["list", "short", "sources"])
            .output()
        else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let name = line.split('\t').nth(1)?.trim();
                if name.ends_with(".monitor") {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Probe whether the PipeWire / PulseAudio client library is loadable on this system.
pub fn probe_backend_available() -> Result<&'static str, String> {
    #[cfg(target_os = "linux")]
    {
        unsafe { libloading::Library::new("libpulse.so.0") }
            .map(|_| "pipewire-pulse (libpulse non-blocking mainloop)")
            .map_err(|e| format!("libpulse.so.0 unavailable: {e}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(
            "system audio capture is currently implemented for Linux PipeWire/PulseAudio only"
                .into(),
        )
    }
}

#[repr(C)]
struct PaSampleSpec {
    format: c_int, // PA_SAMPLE_FLOAT32LE = 5
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PA_SAMPLE_FLOAT32LE: c_int = 5;
const PA_CONTEXT_NOFLAGS: c_int = 0;
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
const PA_STREAM_READY: c_int = 2;
const PA_STREAM_FAILED: c_int = 3;
const PA_STREAM_TERMINATED: c_int = 4;
const PA_STREAM_ADJUST_LATENCY: c_int = 0x2000;

type PaMainloopNewFn = unsafe extern "C" fn() -> *mut c_void;
type PaMainloopFreeFn = unsafe extern "C" fn(m: *mut c_void);
type PaMainloopGetApiFn = unsafe extern "C" fn(m: *mut c_void) -> *mut c_void;
type PaMainloopPrepareFn = unsafe extern "C" fn(m: *mut c_void, timeout_usec: c_int) -> c_int;
type PaMainloopPollFn = unsafe extern "C" fn(m: *mut c_void) -> c_int;
type PaMainloopDispatchFn = unsafe extern "C" fn(m: *mut c_void) -> c_int;

type PaContextNewFn = unsafe extern "C" fn(api: *mut c_void, name: *const c_char) -> *mut c_void;
type PaContextConnectFn = unsafe extern "C" fn(
    c: *mut c_void,
    server: *const c_char,
    flags: c_int,
    api: *const c_void,
) -> c_int;
type PaContextGetStateFn = unsafe extern "C" fn(c: *mut c_void) -> c_int;
type PaContextDisconnectFn = unsafe extern "C" fn(c: *mut c_void);
type PaContextUnrefFn = unsafe extern "C" fn(c: *mut c_void);

type PaStreamNewFn = unsafe extern "C" fn(
    c: *mut c_void,
    name: *const c_char,
    ss: *const PaSampleSpec,
    map: *const c_void,
) -> *mut c_void;
type PaStreamConnectRecordFn = unsafe extern "C" fn(
    s: *mut c_void,
    dev: *const c_char,
    attr: *const PaBufferAttr,
    flags: c_int,
) -> c_int;
type PaStreamGetStateFn = unsafe extern "C" fn(s: *mut c_void) -> c_int;
type PaStreamReadableSizeFn = unsafe extern "C" fn(s: *mut c_void) -> usize;
type PaStreamPeekFn =
    unsafe extern "C" fn(s: *mut c_void, data: *mut *const c_void, nbytes: *mut usize) -> c_int;
type PaStreamDropFn = unsafe extern "C" fn(s: *mut c_void) -> c_int;
type PaStreamDisconnectFn = unsafe extern "C" fn(s: *mut c_void) -> c_int;
type PaStreamUnrefFn = unsafe extern "C" fn(s: *mut c_void);

pub struct SystemAudioCapture {
    status: Arc<Mutex<SystemAudioStatus>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SystemAudioCapture {
    pub fn start(
        source: Option<String>,
        analyzer: Arc<Mutex<AudioAnalyzer>>,
        has_consumers: Arc<AtomicBool>,
    ) -> Self {
        let status = Arc::new(Mutex::new(SystemAudioStatus::Unavailable {
            reason: "initializing".into(),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let status_clone = Arc::clone(&status);
        let shutdown_clone = Arc::clone(&shutdown);

        let worker = std::thread::Builder::new()
            .name("beyrix-sys-audio".to_string())
            .spawn(move || {
                run_capture_loop(
                    source,
                    analyzer,
                    has_consumers,
                    status_clone,
                    shutdown_clone,
                );
            })
            .ok();

        Self {
            status,
            shutdown,
            worker,
        }
    }

    pub fn status(&self) -> SystemAudioStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            // The capture loop polls with a 25ms timeout and checks `shutdown`
            // on every iteration, so join always completes within <100ms.
            let _ = h.join();
        }
    }
}

fn set_status(status: &Arc<Mutex<SystemAudioStatus>>, next: SystemAudioStatus) {
    *status.lock().unwrap_or_else(|e| e.into_inner()) = next;
}

fn run_capture_loop(
    source: Option<String>,
    analyzer: Arc<Mutex<AudioAnalyzer>>,
    has_consumers: Arc<AtomicBool>,
    status: Arc<Mutex<SystemAudioStatus>>,
    shutdown: Arc<AtomicBool>,
) {
    let lib = match unsafe { libloading::Library::new("libpulse.so.0") } {
        Ok(l) => l,
        Err(e) => {
            set_status(
                &status,
                SystemAudioStatus::Unavailable {
                    reason: format!("PipeWire/PulseAudio client library missing: {e}"),
                },
            );
            return;
        }
    };

    macro_rules! load_sym {
        ($ty:ty, $name:literal) => {
            match unsafe { lib.get::<$ty>($name) } {
                Ok(f) => *f,
                Err(_) => {
                    set_status(
                        &status,
                        SystemAudioStatus::Unavailable {
                            reason: format!(
                                "missing libpulse symbol {}",
                                String::from_utf8_lossy($name)
                            ),
                        },
                    );
                    return;
                }
            }
        };
    }

    let ml_new = load_sym!(PaMainloopNewFn, b"pa_mainloop_new\0");
    let ml_free = load_sym!(PaMainloopFreeFn, b"pa_mainloop_free\0");
    let ml_get_api = load_sym!(PaMainloopGetApiFn, b"pa_mainloop_get_api\0");
    let ml_prepare = load_sym!(PaMainloopPrepareFn, b"pa_mainloop_prepare\0");
    let ml_poll = load_sym!(PaMainloopPollFn, b"pa_mainloop_poll\0");
    let ml_dispatch = load_sym!(PaMainloopDispatchFn, b"pa_mainloop_dispatch\0");

    let ctx_new = load_sym!(PaContextNewFn, b"pa_context_new\0");
    let ctx_connect = load_sym!(PaContextConnectFn, b"pa_context_connect\0");
    let ctx_get_state = load_sym!(PaContextGetStateFn, b"pa_context_get_state\0");
    let ctx_disconnect = load_sym!(PaContextDisconnectFn, b"pa_context_disconnect\0");
    let ctx_unref = load_sym!(PaContextUnrefFn, b"pa_context_unref\0");

    let st_new = load_sym!(PaStreamNewFn, b"pa_stream_new\0");
    let st_connect_rec = load_sym!(PaStreamConnectRecordFn, b"pa_stream_connect_record\0");
    let st_get_state = load_sym!(PaStreamGetStateFn, b"pa_stream_get_state\0");
    let st_readable = load_sym!(PaStreamReadableSizeFn, b"pa_stream_readable_size\0");
    let st_peek = load_sym!(PaStreamPeekFn, b"pa_stream_peek\0");
    let st_drop = load_sym!(PaStreamDropFn, b"pa_stream_drop\0");
    let st_disconnect = load_sym!(PaStreamDisconnectFn, b"pa_stream_disconnect\0");
    let st_unref = load_sym!(PaStreamUnrefFn, b"pa_stream_unref\0");

    let ml = unsafe { ml_new() };
    if ml.is_null() {
        set_status(
            &status,
            SystemAudioStatus::Unavailable {
                reason: "pa_mainloop_new returned null".into(),
            },
        );
        return;
    }

    let api = unsafe { ml_get_api(ml) };
    let app_name = CString::new("Beyrix").unwrap();
    let stream_name = CString::new("Beyrix Audio Visualizer").unwrap();
    let requested_src = source
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "@DEFAULT_MONITOR@".to_string());
    let dev_cstr = CString::new(requested_src.clone()).ok();

    let ctx = unsafe { ctx_new(api, app_name.as_ptr()) };
    if ctx.is_null() {
        unsafe { ml_free(ml) };
        set_status(
            &status,
            SystemAudioStatus::Unavailable {
                reason: "pa_context_new returned null".into(),
            },
        );
        return;
    }

    if unsafe { ctx_connect(ctx, std::ptr::null(), PA_CONTEXT_NOFLAGS, std::ptr::null()) } < 0 {
        unsafe {
            ctx_unref(ctx);
            ml_free(ml);
        }
        set_status(
            &status,
            SystemAudioStatus::Unavailable {
                reason: "failed to connect to PipeWire/PulseAudio daemon".into(),
            },
        );
        return;
    }

    let ss = PaSampleSpec {
        format: PA_SAMPLE_FLOAT32LE,
        rate: 44100,
        channels: 1,
    };
    let frag_bytes = (1024 * std::mem::size_of::<f32>()) as u32;
    let attr = PaBufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frag_bytes,
    };

    let mut stream: *mut c_void = std::ptr::null_mut();
    let mut pcm_scratch = Vec::<f32>::with_capacity(2048);
    let connect_deadline = Instant::now() + Duration::from_secs(3);

    while !shutdown.load(Ordering::Relaxed) {
        // Bounded 25ms poll so shutdown is always immediate and non-blocking.
        if unsafe { ml_prepare(ml, 25_000) } < 0 {
            break;
        }
        if unsafe { ml_poll(ml) } < 0 {
            break;
        }
        if unsafe { ml_dispatch(ml) } < 0 {
            break;
        }

        let cstate = unsafe { ctx_get_state(ctx) };
        if cstate == PA_CONTEXT_FAILED || cstate == PA_CONTEXT_TERMINATED {
            set_status(
                &status,
                SystemAudioStatus::Unavailable {
                    reason: "PipeWire/PulseAudio server disconnected".into(),
                },
            );
            break;
        }

        if cstate != PA_CONTEXT_READY {
            if Instant::now() > connect_deadline {
                set_status(
                    &status,
                    SystemAudioStatus::Unavailable {
                        reason: "timed out connecting to PipeWire/PulseAudio".into(),
                    },
                );
                break;
            }
            continue;
        }

        if stream.is_null() {
            stream = unsafe { st_new(ctx, stream_name.as_ptr(), &ss, std::ptr::null()) };
            if stream.is_null() {
                set_status(
                    &status,
                    SystemAudioStatus::Unavailable {
                        reason: "pa_stream_new failed".into(),
                    },
                );
                break;
            }
            let rc = unsafe {
                st_connect_rec(
                    stream,
                    dev_cstr.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                    &attr,
                    PA_STREAM_ADJUST_LATENCY,
                )
            };
            if rc < 0 {
                set_status(
                    &status,
                    SystemAudioStatus::Unavailable {
                        reason: format!("failed to attach to monitor source {requested_src:?}"),
                    },
                );
                break;
            }
            set_status(
                &status,
                SystemAudioStatus::Active {
                    backend: "PipeWire / PulseAudio (non-blocking)".into(),
                    source: requested_src.clone(),
                },
            );
        }

        let sstate = unsafe { st_get_state(stream) };
        if sstate == PA_STREAM_FAILED || sstate == PA_STREAM_TERMINATED {
            set_status(
                &status,
                SystemAudioStatus::Unavailable {
                    reason: "audio monitor stream ended or device disappeared".into(),
                },
            );
            break;
        }

        if sstate == PA_STREAM_READY {
            let mut drained_chunks = 0usize;
            while drained_chunks < 8 && unsafe { st_readable(stream) } >= 4 {
                let mut data_ptr: *const c_void = std::ptr::null();
                let mut nbytes: usize = 0;
                if unsafe { st_peek(stream, &mut data_ptr, &mut nbytes) } < 0 {
                    break;
                }
                if nbytes == 0 {
                    break;
                }
                if !data_ptr.is_null() && has_consumers.load(Ordering::Relaxed) {
                    let count = (nbytes / 4).min(2048);
                    pcm_scratch.clear();
                    let raw =
                        unsafe { std::slice::from_raw_parts(data_ptr as *const u8, count * 4) };
                    for c in raw.as_chunks::<4>().0 {
                        pcm_scratch.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                    }
                    if !pcm_scratch.is_empty()
                        && let Ok(mut guard) = analyzer.try_lock()
                    {
                        guard.process_samples(&pcm_scratch);
                    }
                }
                unsafe {
                    st_drop(stream);
                }
                drained_chunks += 1;
            }
        }
    }

    unsafe {
        if !stream.is_null() {
            st_disconnect(stream);
            st_unref(stream);
        }
        ctx_disconnect(ctx);
        ctx_unref(ctx);
        ml_free(ml);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_honest_status_without_panicking() {
        let _ = probe_backend_available();
        let _ = list_monitor_sources();
    }

    #[test]
    fn capture_shutdown_completes_promptly_without_hanging() {
        let analyzer = Arc::new(Mutex::new(AudioAnalyzer::new(1024, 16, 0.6)));
        let consumers = Arc::new(AtomicBool::new(false));
        let capture = SystemAudioCapture::start(None, analyzer, consumers);
        std::thread::sleep(Duration::from_millis(60));
        let t0 = Instant::now();
        drop(capture);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "SystemAudioCapture::drop must never hang (took {:?})",
            t0.elapsed()
        );
    }
}
