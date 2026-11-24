// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native Rust local audio player powered by `symphonia` and `cpal`.
//!
//! Supports MP3, FLAC, OGG/Vorbis, WAV, and AAC/MP4 with queue management,
//! repeat, shuffle, seek, volume, metadata/cover-art extraction, device
//! disconnect recovery, and real-time PCM tap for [`super::audio::AudioAnalyzer`].

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use super::audio::AudioAnalyzer;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}

#[derive(Clone, Debug, Default)]
pub struct TrackMetadata {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: f64,
    pub cover_art: Option<(u32, u32, Arc<Vec<u8>>)>,
}

#[derive(Clone, Debug, Default)]
pub struct PlayerStatus {
    pub playing: bool,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub volume: f32,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub current: Option<TrackMetadata>,
    pub queue_len: usize,
    pub last_error: Option<String>,
}

pub enum PlayerCommand {
    PlayFile(PathBuf),
    Enqueue(PathBuf),
    PlayPause,
    Next,
    Previous,
    Seek(f64),
    SetVolume(f32),
    SetRepeat(RepeatMode),
    SetShuffle(bool),
    Stop,
    Shutdown,
}

pub struct LocalAudioPlayer {
    tx: mpsc::Sender<PlayerCommand>,
    status: Arc<Mutex<PlayerStatus>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LocalAudioPlayer {
    pub fn start(analyzer: Arc<Mutex<AudioAnalyzer>>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(PlayerStatus {
            volume: 0.85,
            ..Default::default()
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let st_clone = Arc::clone(&status);
        let sh_clone = Arc::clone(&shutdown);

        let worker = std::thread::Builder::new()
            .name("beyrix-audio-player".to_string())
            .spawn(move || player_loop(rx, st_clone, analyzer, sh_clone))
            .map_err(|e| format!("spawn audio player thread: {e}"))?;

        Ok(Self {
            tx,
            status,
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn send(&self, cmd: PlayerCommand) {
        let _ = self.tx.send(cmd);
    }

    pub fn status(&self) -> PlayerStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for LocalAudioPlayer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.tx.send(PlayerCommand::Shutdown);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Probe metadata and optional cover art from a local audio file without playing it.
pub fn probe_metadata(path: &Path) -> Result<TrackMetadata, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe {}: {e}", path.display()))?;

    let mut meta = TrackMetadata {
        path: path.to_path_buf(),
        title: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string(),
        artist: String::new(),
        album: String::new(),
        duration_secs: 0.0,
        cover_art: None,
    };

    if let Some(track) = probed.format.default_track() {
        let params = &track.codec_params;
        if let (Some(n_frames), Some(tb)) = (params.n_frames, params.time_base) {
            let t = tb.calc_time(n_frames);
            meta.duration_secs = t.seconds as f64 + t.frac;
        }
    }

    let mut extract_rev = |rev: &symphonia::core::meta::MetadataRevision| {
        for tag in rev.tags() {
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) => meta.title = tag.value.to_string(),
                Some(StandardTagKey::Artist) => meta.artist = tag.value.to_string(),
                Some(StandardTagKey::Album) => meta.album = tag.value.to_string(),
                _ => {}
            }
        }
        if meta.cover_art.is_none()
            && let Some(vis) = rev.visuals().first()
            && let Ok(img) = image::load_from_memory(&vis.data)
        {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            meta.cover_art = Some((w, h, Arc::new(rgba.into_raw())));
        }
    };

    if let Some(m) = probed.metadata.get()
        && let Some(rev) = m.current()
    {
        extract_rev(rev);
    }
    if let Some(rev) = probed.format.metadata().current() {
        extract_rev(rev);
    }

    Ok(meta)
}

fn player_loop(
    rx: mpsc::Receiver<PlayerCommand>,
    status: Arc<Mutex<PlayerStatus>>,
    analyzer: Arc<Mutex<AudioAnalyzer>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    let mut history: Vec<PathBuf> = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut playing = false;
    let mut volume = 0.85_f32;
    let mut repeat = RepeatMode::Off;
    let mut shuffle = false;
    let mut pending_seek: Option<f64> = None;

    while !shutdown.load(Ordering::Relaxed) {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                PlayerCommand::Shutdown => return,
                PlayerCommand::PlayFile(p) => {
                    if let Some(cur) = current_path.take() {
                        history.push(cur);
                    }
                    current_path = Some(p);
                    playing = true;
                    pending_seek = None;
                }
                PlayerCommand::Enqueue(p) => queue.push_back(p),
                PlayerCommand::PlayPause => playing = !playing,
                PlayerCommand::Stop => {
                    playing = false;
                    pending_seek = Some(0.0);
                }
                PlayerCommand::SetVolume(v) => volume = v.clamp(0.0, 1.5),
                PlayerCommand::SetRepeat(r) => repeat = r,
                PlayerCommand::SetShuffle(s) => shuffle = s,
                PlayerCommand::Seek(sec) => pending_seek = Some(sec.max(0.0)),
                PlayerCommand::Next => {
                    if let Some(cur) = current_path.take() {
                        history.push(cur);
                    }
                    current_path = next_from_queue(&mut queue, shuffle);
                    playing = current_path.is_some();
                }
                PlayerCommand::Previous => {
                    if let Some(prev) = history.pop() {
                        if let Some(cur) = current_path.take() {
                            queue.push_front(cur);
                        }
                        current_path = Some(prev);
                        playing = true;
                    }
                }
            }
        }

        if let Some(path) = current_path.clone()
            && playing
        {
            let outcome = decode_and_play(
                &path,
                &rx,
                &status,
                &analyzer,
                &shutdown,
                &mut playing,
                &mut volume,
                &mut repeat,
                &mut shuffle,
                &mut pending_seek,
                &mut queue,
            );
            match outcome {
                PlaybackOutcome::Finished => match repeat {
                    RepeatMode::One => {
                        playing = true;
                    }
                    RepeatMode::All => {
                        queue.push_back(path);
                        current_path = next_from_queue(&mut queue, shuffle);
                        playing = current_path.is_some();
                    }
                    RepeatMode::Off => {
                        history.push(path);
                        current_path = next_from_queue(&mut queue, shuffle);
                        playing = current_path.is_some();
                    }
                },
                PlaybackOutcome::SwitchTo(next_p) => {
                    history.push(path);
                    current_path = Some(next_p);
                }
                PlaybackOutcome::SkipNext => {
                    history.push(path);
                    current_path = next_from_queue(&mut queue, shuffle);
                    playing = current_path.is_some();
                }
                PlaybackOutcome::SkipPrev => {
                    if let Some(prev) = history.pop() {
                        queue.push_front(path);
                        current_path = Some(prev);
                    }
                }
                PlaybackOutcome::Shutdown => return,
                PlaybackOutcome::Error(err) => {
                    {
                        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                        s.last_error = Some(err);
                        s.playing = false;
                    }
                    current_path = next_from_queue(&mut queue, shuffle);
                    playing = current_path.is_some();
                }
            }
        } else {
            {
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.playing = playing;
                s.volume = volume;
                s.repeat = repeat;
                s.shuffle = shuffle;
                s.queue_len = queue.len();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn next_from_queue(queue: &mut VecDeque<PathBuf>, shuffle: bool) -> Option<PathBuf> {
    if queue.is_empty() {
        return None;
    }
    if shuffle && queue.len() > 1 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() as usize);
        let idx = nanos % queue.len();
        return queue.remove(idx);
    }
    queue.pop_front()
}

enum PlaybackOutcome {
    Finished,
    SwitchTo(PathBuf),
    SkipNext,
    SkipPrev,
    Shutdown,
    Error(String),
}

#[allow(clippy::too_many_arguments)]
fn decode_and_play(
    path: &Path,
    rx: &mpsc::Receiver<PlayerCommand>,
    status: &Arc<Mutex<PlayerStatus>>,
    analyzer: &Arc<Mutex<AudioAnalyzer>>,
    shutdown: &Arc<AtomicBool>,
    playing: &mut bool,
    volume: &mut f32,
    repeat: &mut RepeatMode,
    shuffle: &mut bool,
    pending_seek: &mut Option<f64>,
    queue: &mut VecDeque<PathBuf>,
) -> PlaybackOutcome {
    let meta = probe_metadata(path).unwrap_or_else(|_| TrackMetadata {
        path: path.to_path_buf(),
        title: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Track")
            .to_string(),
        ..Default::default()
    });
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return PlaybackOutcome::Error(format!("open: {e}")),
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(e) => return PlaybackOutcome::Error(format!("decode probe: {e}")),
    };
    let Some(track) = probed.format.default_track() else {
        return PlaybackOutcome::Error("no audio track found".to_string());
    };
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map_or(2, |c| c.count());
    let time_base = track.codec_params.time_base;

    let mut decoder = match symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
    {
        Ok(d) => d,
        Err(e) => return PlaybackOutcome::Error(format!("codec init: {e}")),
    };

    // Ring buffer between decoder and cpal audio callback
    let ring = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(
        sample_rate as usize * channels * 2,
    )));
    let ring_cb = Arc::clone(&ring);
    let vol_shared = Arc::new(Mutex::new(*volume));
    let vol_cb = Arc::clone(&vol_shared);
    let playing_shared = Arc::new(AtomicBool::new(*playing));
    let playing_cb = Arc::clone(&playing_shared);

    let host = cpal::default_host();
    let _stream = host.default_output_device().and_then(|device| {
        let config = cpal::StreamConfig {
            channels: channels.min(2) as u16,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !playing_cb.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let v = *vol_cb.lock().unwrap_or_else(|e| e.into_inner());
                    let mut rb = ring_cb.lock().unwrap_or_else(|e| e.into_inner());
                    for sample in data.iter_mut() {
                        *sample = rb.pop_front().unwrap_or(0.0) * v;
                    }
                },
                |_err| {},
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(stream)
    });

    {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.current = Some(meta.clone());
        s.duration_secs = meta.duration_secs;
        s.playing = *playing;
        s.last_error = None;
    }

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut mono_buf = Vec::with_capacity(1024);

    while !shutdown.load(Ordering::Relaxed) {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                PlayerCommand::Shutdown => return PlaybackOutcome::Shutdown,
                PlayerCommand::PlayFile(p) => return PlaybackOutcome::SwitchTo(p),
                PlayerCommand::Enqueue(p) => queue.push_back(p),
                PlayerCommand::PlayPause => {
                    *playing = !*playing;
                    playing_shared.store(*playing, Ordering::Relaxed);
                }
                PlayerCommand::Next => return PlaybackOutcome::SkipNext,
                PlayerCommand::Previous => return PlaybackOutcome::SkipPrev,
                PlayerCommand::SetVolume(v) => {
                    *volume = v.clamp(0.0, 1.5);
                    *vol_shared.lock().unwrap_or_else(|e| e.into_inner()) = *volume;
                }
                PlayerCommand::SetRepeat(r) => *repeat = r,
                PlayerCommand::SetShuffle(s) => *shuffle = s,
                PlayerCommand::Seek(sec) => *pending_seek = Some(sec.max(0.0)),
                PlayerCommand::Stop => {
                    *playing = false;
                    playing_shared.store(false, Ordering::Relaxed);
                    *pending_seek = Some(0.0);
                }
            }
        }

        if let Some(sec) = pending_seek.take() {
            let secs = sec.trunc() as u64;
            let frac = sec.fract();
            if probed
                .format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time: Time::new(secs, frac),
                        track_id: Some(track_id),
                    },
                )
                .is_ok()
            {
                ring.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
        }

        if !*playing {
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }

        // Backpressure: wait if ring buffer has >0.5s of audio
        {
            let len = ring.lock().unwrap_or_else(|e| e.into_inner()).len();
            if len > (sample_rate as usize * channels) / 2 {
                std::thread::sleep(Duration::from_millis(15));
                continue;
            }
        }

        let packet = match probed.format.next_packet() {
            Ok(p) => p,
            Err(_) => return PlaybackOutcome::Finished,
        };
        if packet.track_id() != track_id {
            continue;
        }
        if let Some(tb) = time_base {
            let t = tb.calc_time(packet.ts());
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            s.position_secs = t.seconds as f64 + t.frac;
            s.playing = *playing;
            s.volume = *volume;
            s.queue_len = queue.len();
        }
        if let Ok(decoded) = decoder.decode(&packet) {
            let spec = *decoded.spec();
            let sb = sample_buf
                .get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
            sb.copy_interleaved_ref(decoded);
            let samples = sb.samples();
            {
                let mut rb = ring.lock().unwrap_or_else(|e| e.into_inner());
                rb.extend(samples.iter().copied());
            }
            mono_buf.clear();
            let ch = spec.channels.count().max(1);
            for frame in samples.chunks(ch) {
                mono_buf.push(frame.iter().sum::<f32>() / ch as f32);
            }
            if let Ok(mut az) = analyzer.try_lock() {
                az.process_samples(&mono_buf);
            }
        }
    }
    PlaybackOutcome::Shutdown
}
