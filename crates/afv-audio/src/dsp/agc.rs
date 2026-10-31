//! Per-stream RMS automatic gain control. Port of afv-native's `RmsAgc`.

use crate::{FRAME_DURATION_MS, Frame};

#[derive(Debug, Clone)]
pub struct RmsAgc {
    enabled: bool,
    target_db: f64,
    target: f64,
    max_gain: f64,
    min_gain: f64,
    attack: f64,
    release: f64,
    envelope: f64,
    gain: f64,
}

impl Default for RmsAgc {
    /// Target −18 dBFS, gain limited to ±12 dB, attack 5 ms, release 150 ms.
    fn default() -> Self {
        Self::new(-18.0, 12.0, -12.0, 5.0, 150.0)
    }
}

fn time_constant(ms: f64) -> f64 {
    (-FRAME_DURATION_MS / ms).exp()
}

fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

impl RmsAgc {
    pub fn new(target_db: f64, max_gain_db: f64, min_gain_db: f64, attack_ms: f64, release_ms: f64) -> Self {
        Self {
            enabled: true,
            target_db,
            target: db_to_lin(target_db),
            max_gain: db_to_lin(max_gain_db),
            min_gain: db_to_lin(min_gain_db),
            attack: time_constant(attack_ms),
            release: time_constant(release_ms),
            envelope: 0.0,
            gain: 1.0,
        }
    }

    pub fn target_db(&self) -> f64 {
        self.target_db
    }

    pub fn set_target_db(&mut self, db: f64) {
        self.target_db = db;
        self.target = db_to_lin(db);
    }

    pub fn enabled(&self) -> bool {
        self.enabled
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
        let sum_squares: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let rms = (sum_squares / frame.len() as f64).sqrt();
        let coef = if rms > self.envelope { self.attack } else { self.release };
        self.envelope = rms + coef * (self.envelope - rms);

        let desired = if self.envelope > 1e-6 { self.target / self.envelope } else { 1.0 };
        let desired = desired.min(self.max_gain).max(self.min_gain);
        // Ramp the gain across the frame to avoid zipper noise.
        let step = (desired - self.gain) / frame.len() as f64;
        let mut gain = self.gain;
        for s in frame.iter_mut() {
            *s = (*s as f64 * gain) as f32;
            gain += step;
        }
        self.gain = gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{db, rms, sine_frame};

    /// Runs `frames` frames of a 450 Hz sine (9 whole cycles per frame) with the given RMS in dB and returns the last output.
    fn settle(agc: &mut RmsAgc, rms_db: f64, frames: usize) -> Frame {
        let amplitude = db_to_lin(rms_db) * std::f64::consts::SQRT_2;
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
        let envelope = 0.5 * (1.0 - (-4f64).exp());
        let desired = db_to_lin(-18.0) / envelope;
        assert!((agc.gain - desired).abs() < 1e-12);
        let step = (desired - 1.0) / crate::FRAME_SAMPLES as f64;
        for (i, &s) in frame.iter().enumerate() {
            assert!((s as f64 - 0.5 * (1.0 + step * i as f64)).abs() < 1e-6, "sample {i}");
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
        let expected_envelope = db_to_lin(-30.0) + (-20f64 / 150.0).exp() * (db_to_lin(-18.0) - db_to_lin(-30.0));
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
