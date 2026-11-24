// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linux System Audio Capture backend for PipeWire / PulseAudio monitors.
//!
//! Dynamically loads `libpulse-simple.so.0` (provided natively by PipeWire via
//! `pipewire-pulse` as well as PulseAudio) via `libloading` so Beyrix requires
//! no compile-time link dependency and degrades honestly when unavailable.
//!
//! Privacy & safety guarantees:
//! - Strictly opt-in (`MediaConfig.audio_capture_enabled = true`).
//! - Never writes audio samples to disk and never transmits them over network.
//! - Skips FFT analysis whenever zero consumers (JS widgets or Background Engine)
//!   are actively subscribed.
//! - Uses a fixed pre-allocated PCM buffer on the capture thread.

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Probe whether the PipeWire / PulseAudio simple capture library is loadable
/// on this system.
pub fn probe_backend_available() -> Result<&'static str, String> {
    #[cfg(target_os = "linux")]
    {
        unsafe { libloading::Library::new("libpulse-simple.so.0") }
            .map(|_| "pipewire-pulse (libpulse-simple)")
            .map_err(|e| format!("libpulse-simple.so.0 unavailable: {e}"))
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

const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_FLOAT32LE: c_int = 5;

type PaSimpleNewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const PaSampleSpec,
    map: *const c_void,
    attr: *const PaBufferAttr,
    error: *mut c_int,
) -> *mut c_void;

type PaSimpleReadFn = unsafe extern "C" fn(
    s: *mut c_void,
    data: *mut c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;

type PaSimpleFreeFn = unsafe extern "C" fn(s: *mut c_void);

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
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

fn run_capture_loop(
    source: Option<String>,
    analyzer: Arc<Mutex<AudioAnalyzer>>,
    has_consumers: Arc<AtomicBool>,
    status: Arc<Mutex<SystemAudioStatus>>,
    shutdown: Arc<AtomicBool>,
) {
    let lib = match unsafe { libloading::Library::new("libpulse-simple.so.0") } {
        Ok(l) => l,
        Err(e) => {
            *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Unavailable {
                reason: format!("PipeWire/PulseAudio client library missing: {e}"),
            };
            return;
        }
    };

    let (pa_new, pa_read, pa_free): (PaSimpleNewFn, PaSimpleReadFn, PaSimpleFreeFn) = unsafe {
        let Ok(f_new) = lib.get::<PaSimpleNewFn>(b"pa_simple_new\0") else {
            *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Unavailable {
                reason: "pa_simple_new symbol missing".into(),
            };
            return;
        };
        let Ok(f_read) = lib.get::<PaSimpleReadFn>(b"pa_simple_read\0") else {
            *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Unavailable {
                reason: "pa_simple_read symbol missing".into(),
            };
            return;
        };
        let Ok(f_free) = lib.get::<PaSimpleFreeFn>(b"pa_simple_free\0") else {
            *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Unavailable {
                reason: "pa_simple_free symbol missing".into(),
            };
            return;
        };
        (*f_new, *f_read, *f_free)
    };

    let app_name = CString::new("Beyrix").unwrap();
    let stream_name = CString::new("Beyrix Audio Visualizer").unwrap();
    let requested_src = source
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "@DEFAULT_MONITOR@".to_string());
    let dev_cstr = CString::new(requested_src.clone()).ok();

    let ss = PaSampleSpec {
        format: PA_SAMPLE_FLOAT32LE,
        rate: 44100,
        channels: 1,
    };
    let chunk_samples: usize = 1024;
    let chunk_bytes = (chunk_samples * std::mem::size_of::<f32>()) as u32;
    let attr = PaBufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: chunk_bytes,
    };

    let mut err: c_int = 0;
    let stream = unsafe {
        pa_new(
            std::ptr::null(),
            app_name.as_ptr(),
            PA_STREAM_RECORD,
            dev_cstr.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            stream_name.as_ptr(),
            &ss,
            std::ptr::null(),
            &attr,
            &mut err,
        )
    };

    if stream.is_null() {
        *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Unavailable {
            reason: format!(
                "failed to connect to PipeWire/PulseAudio monitor {requested_src:?} (err={err})"
            ),
        };
        return;
    }

    *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Active {
        backend: "PipeWire / PulseAudio".into(),
        source: requested_src,
    };

    // Fixed pre-allocated stack buffer: zero heap allocations inside capture loop.
    let mut pcm = [0.0f32; 1024];
    let byte_len = std::mem::size_of_val(&pcm);

    while !shutdown.load(Ordering::Relaxed) {
        let rc = unsafe { pa_read(stream, pcm.as_mut_ptr().cast(), byte_len, &mut err) };
        if rc < 0 {
            *status.lock().unwrap_or_else(|e| e.into_inner()) = SystemAudioStatus::Unavailable {
                reason: format!("audio stream interrupted (err={err})"),
            };
            break;
        }
        // Skip FFT completely when no widget or Background Engine is consuming audio!
        if has_consumers.load(Ordering::Relaxed) {
            if let Ok(mut guard) = analyzer.try_lock() {
                guard.process_samples(&pcm);
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    unsafe {
        pa_free(stream);
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
}
