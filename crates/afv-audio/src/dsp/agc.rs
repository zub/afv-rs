//! Per-stream RMS automatic gain control. Port of afv-native's `RmsAgc`.

use super::{db_to_gain, pole};
use crate::{FRAME_DURATION_MS, Frame};

#[derive(Debug, Clone)]
pub struct RmsAgc {
    enabled: bool,
    target: f32,
    max_gain: f32,
    min_gain: f32,
    attack: f32,
    release: f32,
    envelope: f32,
    gain: f32,
}

// afv-native's settings: gain limited to ±12 dB, attack 5 ms, release 150 ms.
const MAX_GAIN_DB: f32 = 12.0;
const MIN_GAIN_DB: f32 = -12.0;
const ATTACK_MS: f32 = 5.0;
const RELEASE_MS: f32 = 150.0;
/// Below this RMS the input counts as silence and gets unity gain rather than the maximum.
const SILENCE: f32 = 1e-6;

impl Default for RmsAgc {
    /// Target [`RmsAgc::DEFAULT_TARGET_DB`].
    fn default() -> Self {
        Self {
            enabled: true,
            target: db_to_gain(Self::DEFAULT_TARGET_DB),
            max_gain: db_to_gain(MAX_GAIN_DB),
            min_gain: db_to_gain(MIN_GAIN_DB),
            attack: pole(FRAME_DURATION_MS, ATTACK_MS),
            release: pole(FRAME_DURATION_MS, RELEASE_MS),
            envelope: 0.0,
            gain: 1.0,
        }
    }
}

impl RmsAgc {
    /// afv-native's target level.
    pub const DEFAULT_TARGET_DB: f32 = -18.0;

    pub fn set_target_db(&mut self, db: f32) {
        self.target = db_to_gain(db);
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.gain = 1.0;
    }

    pub fn process(&mut self, frame: &mut Frame) {
        if !self.enabled {
            return;
        }
        let sum_squares: f32 = frame.iter().map(|&s| s * s).sum();
        let rms = (sum_squares / frame.len() as f32).sqrt();
        let coef = if rms > self.envelope { self.attack } else { self.release };
        self.envelope = rms + coef * (self.envelope - rms);

        let desired = if self.envelope > SILENCE { self.target / self.envelope } else { 1.0 };
        let desired = desired.min(self.max_gain).max(self.min_gain);
        // Ramp the gain across the frame to avoid zipper noise.
        let step = (desired - self.gain) / frame.len() as f32;
        for (i, s) in frame.iter_mut().enumerate() {
            *s *= self.gain + step * i as f32;
        }
        self.gain = desired;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{db, rms, sine_frame};

    /// Runs `frames` frames of a 450 Hz sine (9 whole cycles per frame) with the given RMS in dB and returns the last output.
    fn settle(agc: &mut RmsAgc, rms_db: f32, frames: usize) -> Frame {
        let amplitude = (db_to_gain(rms_db) * std::f32::consts::SQRT_2) as f64;
        let mut frame = [0.0; crate::FRAME_SAMPLES];
        for i in 0..frames {
            frame = sine_frame(i, 450.0, amplitude);
            agc.process(&mut frame);
        }
        frame
    }

    #[test]
    fn levels_converge_to_the_target_within_the_gain_limits() {
        // Default: target −18 dB, gain within ±12 dB.
        for (input_db, expected_db) in [(-24.0, -18.0), (-12.0, -18.0), (-40.0, -28.0), (-3.0, -15.0)] {
            let out = settle(&mut RmsAgc::default(), input_db, 100);
            let got = db(rms(&out));
            assert!((got - expected_db).abs() < 0.05, "{input_db} dB in: {got} dB out, expected {expected_db}");
        }
    }

    #[test]
    fn disabled_is_a_passthrough() {
        let mut agc = RmsAgc::default();
        agc.set_enabled(false);
        let input = sine_frame(0, 440.0, 0.01);
        let mut frame = input;
        agc.process(&mut frame);
        assert_eq!(frame, input);
    }

    #[test]
    fn silence_stays_silent_and_keeps_unity_gain() {
        let mut agc = RmsAgc::default();
        let mut frame = [0.0; crate::FRAME_SAMPLES];
        agc.process(&mut frame);
        assert!(frame.iter().all(|&s| s == 0.0));
        assert_eq!(agc.gain, 1.0);
    }

    #[test]
    fn gain_ramps_linearly_across_a_frame() {
        let mut agc = RmsAgc::default();
        let mut frame = [0.5; crate::FRAME_SAMPLES];
        agc.process(&mut frame);
        // Envelope after one 20 ms frame with a 5 ms attack: 0.5·(1 − e^−4).
        let envelope = 0.5 * (1.0 - (-4f32).exp());
        let desired = db_to_gain(-18.0) / envelope;
        assert!((agc.gain - desired).abs() < 1e-5);
        let step = (desired - 1.0) / crate::FRAME_SAMPLES as f32;
        for (i, &s) in frame.iter().enumerate() {
            assert!((s - 0.5 * (1.0 + step * i as f32)).abs() < 1e-5, "sample {i}");
        }
    }

    #[test]
    fn attack_is_fast_and_release_slow() {
        let mut agc = RmsAgc::default();
        settle(&mut agc, -30.0, 100);
        // A 12 dB jump settles within two frames (5 ms attack against 20 ms frames).
        settle(&mut agc, -18.0, 1);
        let out = settle(&mut agc, -18.0, 1);
        assert!((db(rms(&out)) + 18.0).abs() < 0.3, "{}", db(rms(&out)));

        // Dropping 12 dB: with a 150 ms release the envelope only falls by e^(−20/150) per frame.
        settle(&mut agc, -18.0, 100);
        let before = agc.gain;
        settle(&mut agc, -30.0, 1);
        let expected_envelope = db_to_gain(-30.0) + (-20f32 / 150.0).exp() * (db_to_gain(-18.0) - db_to_gain(-30.0));
        assert!((agc.envelope - expected_envelope).abs() < 1e-4, "{} vs {expected_envelope}", agc.envelope);
        assert!(agc.gain < before * 1.2);
    }

    #[test]
    fn disabling_resets_the_gain() {
        let mut agc = RmsAgc::default();
        settle(&mut agc, -40.0, 50);
        agc.set_enabled(false);
        agc.set_enabled(true);
        assert_eq!((agc.gain, agc.envelope), (1.0, 0.0));
    }
}
