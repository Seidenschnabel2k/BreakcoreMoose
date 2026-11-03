//! Band levels of the audio being played, for the web player's visualizer.
//! A handful of Goertzel filters over the last ~21 ms, a few times a second.

use std::f32::consts::PI;

use crate::voice::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

pub const BANDS: usize = 20;
/// 0–255 per band, low to high frequencies.
pub type Levels = [u8; BANDS];

const WINDOW: usize = 1024;
/// Analyse every this many 20 ms frames (≈ 17 updates a second).
const EVERY: u32 = 3;
const LOW_HZ: f32 = 50.0;
const HIGH_HZ: f32 = 16_000.0;
/// dB range mapped onto 0–255.
const FLOOR_DB: f32 = -66.0;
const CEIL_DB: f32 = -6.0;

pub struct Analyzer {
    mono: Vec<f32>,
    frames: u32,
    window: Vec<f32>,
    coeffs: [f32; BANDS],
}

impl Default for Analyzer {
    fn default() -> Self {
        let window = (0..WINDOW)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (WINDOW - 1) as f32).cos())
            .collect();
        let coeffs = std::array::from_fn(|b| {
            // Log-spaced centre frequencies.
            let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(b as f32 / (BANDS - 1) as f32);
            2.0 * (2.0 * PI * hz / SAMPLE_RATE as f32).cos()
        });
        Self {
            mono: Vec::with_capacity(WINDOW + FRAME_SAMPLES),
            frames: 0,
            window,
            coeffs,
        }
    }
}

impl Analyzer {
    /// Feeds one interleaved stereo frame; returns new levels now and then.
    pub fn push(&mut self, frame: &[i16]) -> Option<Levels> {
        let channels = CHANNELS as usize;
        self.mono.extend(
            frame
                .chunks_exact(channels)
                .map(|s| (s[0] as f32 + s[1] as f32) / (2.0 * 32768.0)),
        );
        if self.mono.len() > WINDOW {
            self.mono.drain(..self.mono.len() - WINDOW);
        }
        self.frames += 1;
        if !self.frames.is_multiple_of(EVERY) || self.mono.len() < WINDOW {
            return None;
        }
        Some(self.levels())
    }

    fn levels(&self) -> Levels {
        std::array::from_fn(|b| {
            let coeff = self.coeffs[b];
            let (mut s1, mut s2) = (0f32, 0f32);
            for (x, w) in self.mono.iter().zip(&self.window) {
                let s = x * w + coeff * s1 - s2;
                s2 = s1;
                s1 = s;
            }
            let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
            // A full-scale sine gives an amplitude near 1 (Hann gain is ½).
            let amplitude = power.sqrt() / (WINDOW as f32 / 4.0);
            let db = 20.0 * amplitude.max(1e-9).log10();
            ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB) * 255.0).clamp(0.0, 255.0) as u8
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_lights_its_band() {
        let mut analyzer = Analyzer::default();
        // 1 kHz, half scale.
        let mut t = 0usize;
        let mut last = None;
        for _ in 0..12 {
            let frame: Vec<i16> = (0..FRAME_SAMPLES / 2)
                .flat_map(|_| {
                    let v = ((2.0 * PI * 1000.0 * t as f32 / SAMPLE_RATE as f32).sin() * 16000.0)
                        as i16;
                    t += 1;
                    [v, v]
                })
                .collect();
            last = analyzer.push(&frame).or(last);
        }
        let levels = last.expect("levels after a few frames");
        let peak = (0..BANDS).max_by_key(|&b| levels[b]).unwrap();
        let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(peak as f32 / (BANDS - 1) as f32);
        assert!(
            (700.0..1400.0).contains(&hz),
            "peak band at {hz} Hz: {levels:?}"
        );
        assert!(levels[peak] > 150);
        assert!(levels[0] < 60 && levels[BANDS - 1] < 60, "{levels:?}");

        let silent = Analyzer::default().levels();
        assert_eq!(silent, [0; BANDS]);
    }
}
