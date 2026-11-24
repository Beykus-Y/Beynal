// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified Media Engine & Audio Analysis coordinator.
//!
//! Combines system media sessions (Linux MPRIS2 / Windows GSMTC), the native
//! Rust local audio player (`symphonia` + `cpal`), and the `rustfft` Audio SDK.
//! Completely inactive (zero threads, zero polling) when `MediaConfig.enabled` is false.

#![allow(dead_code)]

pub mod audio;
#[cfg(target_os = "linux")]
pub mod mpris;
pub mod player;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::MediaConfig;
use crate::events::{AppEvent, AudioFramePayload, EventBus, EventKind, MediaEventPayload};
use audio::{AudioAnalysis, AudioAnalyzer};
use player::{LocalAudioPlayer, PlayerCommand};

pub struct MediaManager {
    pub config: MediaConfig,
    analyzer: Arc<Mutex<AudioAnalyzer>>,
    player: Option<LocalAudioPlayer>,
    active_mpris_bus: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MediaManager {
    pub fn new(config: MediaConfig) -> Self {
        let analyzer = Arc::new(Mutex::new(AudioAnalyzer::new(
            1024,
            config.fft_bands,
            config.smoothing,
        )));
        Self {
            config,
            analyzer,
            player: None,
            active_mpris_bus: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub fn ensure_started(&mut self, bus: EventBus) {
        if !self.config.enabled || self.worker.is_some() {
            return;
        }
        if self.config.player_enabled && self.player.is_none() {
            match LocalAudioPlayer::start(Arc::clone(&self.analyzer)) {
                Ok(p) => self.player = Some(p),
                Err(e) => eprintln!("beyrix media player: {e}"),
            }
        }
        let mpris_enabled = self.config.mpris_enabled;
        let active_bus = Arc::clone(&self.active_mpris_bus);
        let analyzer = Arc::clone(&self.analyzer);
        let shutdown = Arc::clone(&self.shutdown);
        let player_present = self.player.is_some();

        self.worker = std::thread::Builder::new()
            .name("beyrix-media-hub".to_string())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                let dbus = if mpris_enabled {
                    zbus::blocking::Connection::session().ok()
                } else {
                    None
                };

                while !shutdown.load(Ordering::Relaxed) {
                    if bus.subscriber_count(EventKind::AudioFrame) > 0 && player_present {
                        let a = analyzer
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .snapshot();
                        bus.publish(AppEvent::AudioFrame(AudioFramePayload {
                            amplitude: a.amplitude,
                            rms: a.rms,
                            peak: a.peak,
                            beat: a.beat,
                            bass: a.bass,
                            mid: a.mid,
                            treble: a.treble,
                            bands: a.bands,
                        }));
                    }

                    #[cfg(target_os = "linux")]
                    if let Some(conn) = &dbus
                        && bus.subscriber_count(EventKind::MediaSession) > 0
                    {
                        let players = mpris::list_mpris_players(conn);
                        let chosen = {
                            let mut guard = active_bus.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(cur) = guard.as_ref()
                                && players.contains(cur)
                            {
                                Some(cur.clone())
                            } else {
                                let first = players.first().cloned();
                                *guard = first.clone();
                                first
                            }
                        };
                        if let Some(name) = chosen
                            && let Some(sess) = mpris::read_mpris_session(conn, &name)
                        {
                            bus.publish(AppEvent::MediaSession(MediaEventPayload {
                                session_id: sess.bus_name,
                                source: sess.identity,
                                title: sess.title,
                                artist: sess.artist,
                                album: sess.album,
                                playing: sess.playing,
                                position_secs: sess.position_secs,
                                duration_secs: sess.duration_secs,
                                volume: sess.volume,
                            }));
                        }
                    }

                    std::thread::sleep(Duration::from_millis(250));
                }
            })
            .ok();
    }

    pub fn play_pause(&self) {
        if let Some(p) = &self.player
            && p.status().current.is_some()
        {
            p.send(PlayerCommand::PlayPause);
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(bus_name) = self
            .active_mpris_bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && let Ok(conn) = zbus::blocking::Connection::session()
        {
            let _ = mpris::send_mpris_command(&conn, &bus_name, "PlayPause");
        }
    }

    pub fn next(&self) {
        if let Some(p) = &self.player
            && p.status().current.is_some()
        {
            p.send(PlayerCommand::Next);
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(bus_name) = self
            .active_mpris_bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && let Ok(conn) = zbus::blocking::Connection::session()
        {
            let _ = mpris::send_mpris_command(&conn, &bus_name, "Next");
        }
    }

    pub fn prev(&self) {
        if let Some(p) = &self.player
            && p.status().current.is_some()
        {
            p.send(PlayerCommand::Previous);
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(bus_name) = self
            .active_mpris_bus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && let Ok(conn) = zbus::blocking::Connection::session()
        {
            let _ = mpris::send_mpris_command(&conn, &bus_name, "Previous");
        }
    }

    pub fn play_file(&self, path: PathBuf) {
        if let Some(p) = &self.player {
            p.send(PlayerCommand::PlayFile(path));
        }
    }

    pub fn seek(&self, secs: f64) {
        if let Some(p) = &self.player {
            p.send(PlayerCommand::Seek(secs));
        }
    }

    pub fn set_volume(&self, v: f64) {
        if let Some(p) = &self.player {
            p.send(PlayerCommand::SetVolume(v as f32));
        }
    }

    pub fn audio_snapshot(&self) -> AudioAnalysis {
        self.analyzer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }
}

impl Drop for MediaManager {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_media_manager_spawns_no_threads() {
        let mut mgr = MediaManager::new(MediaConfig::default());
        mgr.ensure_started(EventBus::new());
        assert!(mgr.worker.is_none());
        assert!(mgr.player.is_none());
    }
}
