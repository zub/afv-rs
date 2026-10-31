//! Signal helpers shared by unit tests.

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

/// `n` samples of a sine starting at phase 0.
pub fn sine(freq: f64, amplitude: f64, n: usize) -> Vec<f32> {
    let step = std::f64::consts::TAU * freq / SAMPLE_RATE as f64;
    (0..n).map(|i| (amplitude * (step * i as f64).sin()) as f32).collect()
}

/// Frame `index` of a continuous sine.
pub fn sine_frame(index: usize, freq: f64, amplitude: f64) -> Frame {
    let step = std::f64::consts::TAU * freq / SAMPLE_RATE as f64;
    std::array::from_fn(|i| (amplitude * (step * (index * FRAME_SAMPLES + i) as f64).sin()) as f32)
}

pub fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / x.len() as f64).sqrt() as f32
}

pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

pub fn db(lin: f32) -> f32 {
    20.0 * lin.log10()
}

/// Amplitude of the `freq` component of `x` (a single DFT bin). Exact when `x` spans a whole
/// number of periods of `freq`.
pub fn tone(x: &[f32], freq: f64) -> f32 {
    let (mut re, mut im) = (0f64, 0f64);
    for (i, s) in x.iter().enumerate() {
        let a = std::f64::consts::TAU * freq * i as f64 / SAMPLE_RATE as f64;
        re += *s as f64 * a.cos();
        im += *s as f64 * a.sin();
    }
    (2.0 * re.hypot(im) / x.len() as f64) as f32
}
