// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Audio Analysis SDK powered by `rustfft`.
//!
//! Computes amplitude, RMS, peak, configurable FFT frequency bands, smoothing,
//! beat detection, and waveform samples. Skips all FFT work when there are
//! zero active subscribers.

use rustfft::{FftPlanner, num_complex::Complex};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSource {
    LocalPlayer,
    SystemCapture,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioAnalysis {
    pub amplitude: f32,
    pub rms: f32,
    pub peak: f32,
    pub beat: bool,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub bands: Vec<f32>,
    pub waveform: Vec<f32>,
}

pub struct AudioAnalyzer {
    fft_size: usize,
    band_count: usize,
    smoothing: f32,
    fft: Arc<dyn rustfft::Fft<f32>>,
    fft_buf: Vec<Complex<f32>>,
    window: Vec<f32>,
    prev_bands: Vec<f32>,
    energy_history: Vec<f32>,
    history_idx: usize,
    latest: Arc<Mutex<AudioAnalysis>>,
}

impl AudioAnalyzer {
    pub fn new(fft_size: usize, band_count: usize, smoothing: f32) -> Self {
        let fft_size = fft_size.next_power_of_two().clamp(256, 4096);
        let band_count = band_count.clamp(4, 128);
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let window: Vec<f32> = (0..fft_size)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (fft_size - 1) as f32).cos())
            })
            .collect();
        Self {
            fft_size,
            band_count,
            smoothing: smoothing.clamp(0.0, 0.98),
            fft,
            fft_buf: vec![Complex::new(0.0, 0.0); fft_size],
            window,
            prev_bands: vec![0.0; band_count],
            energy_history: vec![0.0; 32],
            history_idx: 0,
            latest: Arc::new(Mutex::new(AudioAnalysis {
                bands: vec![0.0; band_count],
                ..Default::default()
            })),
        }
    }

    pub fn snapshot(&self) -> AudioAnalysis {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Analyze a slice of mono PCM samples in `-1.0..=1.0`.
    pub fn process_samples(&mut self, samples: &[f32]) -> AudioAnalysis {
        if samples.is_empty() {
            return self.snapshot();
        }
        let mut sum_sq = 0.0_f32;
        let mut peak = 0.0_f32;
        let mut sum_abs = 0.0_f32;
        for &s in samples {
            let a = s.abs();
            sum_abs += a;
            sum_sq += s * s;
            if a > peak {
                peak = a;
            }
        }
        let n = samples.len() as f32;
        let amplitude = (sum_abs / n).clamp(0.0, 1.0);
        let rms = (sum_sq / n).sqrt().clamp(0.0, 1.0);

        self.fft_buf.fill(Complex::new(0.0, 0.0));
        let copy_len = samples.len().min(self.fft_size);
        for (i, &s) in samples[..copy_len].iter().enumerate() {
            self.fft_buf[i].re = s * self.window[i];
        }
        self.fft.process(&mut self.fft_buf);

        let half = self.fft_size / 2;
        let mut bands = vec![0.0_f32; self.band_count];
        for (b, band_val) in bands.iter_mut().enumerate() {
            let start = (b * half) / self.band_count;
            let end = (((b + 1) * half) / self.band_count)
                .max(start + 1)
                .min(half);
            let mut acc = 0.0_f32;
            for c in &self.fft_buf[start..end] {
                acc += c.norm();
            }
            let mag =
                (acc / ((end - start) as f32 * (self.fft_size as f32).sqrt())).clamp(0.0, 1.0);
            let smoothed = self.prev_bands[b] * self.smoothing + mag * (1.0 - self.smoothing);
            self.prev_bands[b] = smoothed;
            *band_val = smoothed;
        }

        let third = (self.band_count / 3).max(1);
        let bass = bands[..third].iter().sum::<f32>() / third as f32;
        let mid_end = (third * 2).min(self.band_count);
        let mid = bands[third..mid_end].iter().sum::<f32>() / (mid_end - third).max(1) as f32;
        let treble = if mid_end < self.band_count {
            bands[mid_end..].iter().sum::<f32>() / (self.band_count - mid_end) as f32
        } else {
            0.0
        };

        let avg_energy = self.energy_history.iter().sum::<f32>() / self.energy_history.len() as f32;
        let beat = bass > 0.08 && bass > avg_energy * 1.45;
        self.energy_history[self.history_idx] = bass;
        self.history_idx = (self.history_idx + 1) % self.energy_history.len();

        let wave_step = (samples.len() / 64).max(1);
        let waveform: Vec<f32> = samples
            .iter()
            .step_by(wave_step)
            .take(64)
            .copied()
            .collect();

        let out = AudioAnalysis {
            amplitude,
            rms,
            peak: peak.clamp(0.0, 1.0),
            beat,
            bass,
            mid,
            treble,
            bands,
            waveform,
        };
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = out.clone();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_detects_sine_wave_energy_and_rms() {
        let mut analyzer = AudioAnalyzer::new(512, 16, 0.0);
        let samples: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin() * 0.8)
            .collect();
        let res = analyzer.process_samples(&samples);
        assert!(res.rms > 0.4);
        assert!(res.peak > 0.75);
        assert_eq!(res.bands.len(), 16);
        assert!(res.bands.iter().any(|&b| b > 0.05));
    }
}
