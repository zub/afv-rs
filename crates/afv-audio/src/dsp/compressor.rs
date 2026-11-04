//! Port of the sndfilter dynamics compressor (MIT licence, (c) 2016 Sean Connelly,
//! <https://github.com/velipso/sndfilter>), itself derived from Chromium's WebAudio
//! `DynamicsCompressorKernel`, with the settings of `sf_defaultcomp`, which afv-native uses for
//! every radio. Includes afv-native's change to process a trailing partial chunk. Unlike both,
//! the detector starts settled rather than fully compressed (see [`Compressor::new`]).
//!
//! afv-native only ever feeds the left channel (right is silent), so this port is mono;
//! with a silent right channel the stereo algorithm reduces to exactly this. The metering
//! state, which does not affect the output, is omitted, and so are the settings that
//! `sf_defaultcomp` leaves at no effect: 0 dB pre- and post-gain, fully wet. Arithmetic is `f32`
//! throughout, as in C.

use std::f32::consts::PI;

use crate::SAMPLE_RATE;

const MAX_DELAY: usize = 1024;
/// Samples per envelope update.
const SPU: usize = 32;
const SPACING_DB: f32 = 5.0;

fn db_to_lin(db: f32) -> f32 {
    10.0f32.powf(0.05 * db)
}

fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.log10()
}

fn knee_curve(x: f32, k: f32, linear_threshold: f32) -> f32 {
    linear_threshold + (1.0 - (-k * (x - linear_threshold)).exp()) / k
}

fn knee_slope(x: f32, k: f32, linear_threshold: f32) -> f32 {
    k * x / ((k * linear_threshold + 1.0) * (k * (x - linear_threshold)).exp() - 1.0)
}

/// Replaces NaN and infinities, which the original guards against too.
fn fix(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

// `sf_defaultcomp`.
const THRESHOLD_DB: f32 = -30.0;
const KNEE_DB: f32 = 30.0;
const RATIO: f32 = 12.0;
const ATTACK_S: f32 = 0.003;
const RELEASE_S: f32 = 0.250;
const PREDELAY_S: f32 = 0.006;
/// Release time, as fractions of the release time, while 12, 9, 6 and 3 dB of gain remain to be
/// recovered: fast out of deep compression, slowing towards the end.
const RELEASE_ZONES: [f32; 4] = [0.090, 0.160, 0.420, 0.980];

// Tuning inside sndfilter (and Chromium's `DynamicsCompressorKernel`, which it follows).
/// The knee's curvature `k` is found by bisection from this start, within this range, in this
/// many steps.
const KNEE_K_START: f32 = 5.0;
const KNEE_K_RANGE: (f32, f32) = (0.1, 10_000.0);
const KNEE_K_STEPS: usize = 15;
/// The detector releases from saturation over this time, at least this many dB at a time.
const SATURATION_RELEASE_S: f32 = 0.0025;
const SATURATION_MIN_DB: f32 = 2.0;
/// Make-up gain is the full-scale reduction raised to this power (an empirical choice).
const MAKEUP_EXPONENT: f32 = 0.6;
/// The release curve covers up to this much compression, mapped onto the four zones.
const RELEASE_RANGE_DB: f32 = 12.0;
const RELEASE_ZONES_PER_DB: f32 = 0.25;
/// Attack aims to reach this fraction of the remaining reduction per attack time, never slower
/// than for this many dB.
const ATTACK_RESIDUE: f32 = 0.25;
const ATTACK_MIN_DB: f32 = 0.5;
/// Below this the input counts as silence and needs no compression.
const SILENCE: f32 = 0.0001;

#[derive(Debug, Clone)]
pub struct Compressor {
    linear_threshold: f32,
    slope: f32,
    attack_samples_inv: f32,
    sat_release_samples_inv: f32,
    k: f32,
    knee_db_offset: f32,
    linear_threshold_knee: f32,
    master_gain: f32,
    /// The cubic through the release zones: `a·x³ + b·x² + c·x + d` samples.
    release: [f32; 4],
    detector_avg: f32,
    comp_gain: f32,
    max_comp_diff_db: f32,
    delay_size: usize,
    delay_write: usize,
    delay_read: usize,
    delay: Box<[f32; MAX_DELAY]>,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor {
    /// `sf_defaultcomp` at 48 kHz.
    pub fn new() -> Self {
        let rate = SAMPLE_RATE as f32;
        let delay_size = ((rate * PREDELAY_S) as i32).clamp(1, MAX_DELAY as i32) as usize;
        let linear_threshold = db_to_lin(THRESHOLD_DB);
        let slope = 1.0 / RATIO;
        let release_samples = rate * RELEASE_S;

        let x_knee = db_to_lin(THRESHOLD_DB + KNEE_DB);
        let mut k = KNEE_K_START;
        let (mut min_k, mut max_k) = KNEE_K_RANGE;
        for _ in 0..KNEE_K_STEPS {
            if knee_slope(x_knee, k, linear_threshold) < slope {
                max_k = k;
            } else {
                min_k = k;
            }
            k = (min_k * max_k).sqrt();
        }

        // The cubic through the four release zones at x = 0, 1, 2, 3.
        let [y1, y2, y3, y4] = RELEASE_ZONES.map(|z| release_samples * z);
        let mut c = Self {
            linear_threshold,
            slope,
            attack_samples_inv: 1.0 / (rate * ATTACK_S),
            sat_release_samples_inv: 1.0 / (rate * SATURATION_RELEASE_S),
            k,
            knee_db_offset: lin_to_db(knee_curve(x_knee, k, linear_threshold)),
            linear_threshold_knee: db_to_lin(THRESHOLD_DB + KNEE_DB),
            master_gain: 0.0,
            release: [
                (-y1 + 3.0 * y2 - 3.0 * y3 + y4) / 6.0,
                y1 - 2.5 * y2 + 2.0 * y3 - 0.5 * y4,
                (-11.0 * y1 + 18.0 * y2 - 9.0 * y3 + 2.0 * y4) / 6.0,
                y1,
            ],
            // sndfilter starts at 0.0, i.e. fully compressed, which made every new compressor
            // about 14 dB quieter for its first 0.3 s. Start settled instead.
            detector_avg: 1.0,
            comp_gain: 1.0,
            max_comp_diff_db: -1.0,
            delay_size,
            delay_write: 0,
            delay_read: if delay_size > 1 { 1 } else { 0 },
            delay: Box::new([0.0; MAX_DELAY]),
        };
        c.master_gain = (1.0 / c.comp_curve(1.0)).powf(MAKEUP_EXPONENT);
        c
    }

    fn comp_curve(&self, x: f32) -> f32 {
        if x < self.linear_threshold {
            return x;
        }
        if x < self.linear_threshold_knee {
            return knee_curve(x, self.k, self.linear_threshold);
        }
        db_to_lin(self.knee_db_offset + self.slope * (lin_to_db(x) - THRESHOLD_DB - KNEE_DB))
    }

    fn adaptive_release(&self, x: f32) -> f32 {
        let [a, b, c, d] = self.release;
        let x2 = x * x;
        a * x2 * x + b * x2 + c * x + d
    }

    /// Back to the state it starts in: settled, with an empty pre-delay.
    pub fn reset(&mut self) {
        (self.detector_avg, self.comp_gain, self.max_comp_diff_db) = (1.0, 1.0, -1.0);
        self.delay_write = 0;
        self.delay_read = if self.delay_size > 1 { 1 } else { 0 };
        self.delay.fill(0.0);
    }

    /// Processes `samples` in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        let ang90 = PI * 0.5;
        let ang90_inv = 2.0 / PI;
        for chunk in samples.chunks_mut(SPU) {
            self.detector_avg = fix(self.detector_avg, 1.0);
            let scaled_desired_gain = self.detector_avg.asin() * ang90_inv;
            let mut comp_diff_db = lin_to_db(self.comp_gain / scaled_desired_gain);

            let envelope_rate = if comp_diff_db < 0.0 {
                // Releasing.
                comp_diff_db = fix(comp_diff_db, -1.0);
                self.max_comp_diff_db = -1.0;
                let x = (comp_diff_db.clamp(-RELEASE_RANGE_DB, 0.0) + RELEASE_RANGE_DB) * RELEASE_ZONES_PER_DB;
                db_to_lin(SPACING_DB / self.adaptive_release(x))
            } else {
                // Attacking.
                comp_diff_db = fix(comp_diff_db, 1.0);
                if self.max_comp_diff_db == -1.0 || self.max_comp_diff_db < comp_diff_db {
                    self.max_comp_diff_db = comp_diff_db;
                }
                let attenuate = self.max_comp_diff_db.max(ATTACK_MIN_DB);
                1.0 - (ATTACK_RESIDUE / attenuate).powf(self.attack_samples_inv)
            };

            for sample in chunk {
                let input = *sample;
                self.delay[self.delay_write] = input;

                let input_max = input.abs();
                let attenuation = if input_max < SILENCE { 1.0 } else { self.comp_curve(input_max) / input_max };

                let rate = if attenuation > self.detector_avg {
                    let attenuation_db = (-lin_to_db(attenuation)).max(SATURATION_MIN_DB);
                    db_to_lin(attenuation_db * self.sat_release_samples_inv) - 1.0
                } else {
                    1.0
                };
                self.detector_avg += (attenuation - self.detector_avg) * rate;
                self.detector_avg = fix(self.detector_avg.min(1.0), 1.0);

                if envelope_rate < 1.0 {
                    self.comp_gain += (scaled_desired_gain - self.comp_gain) * envelope_rate;
                } else {
                    self.comp_gain = (self.comp_gain * envelope_rate).min(1.0);
                }

                let gain = self.master_gain * (ang90 * self.comp_gain).sin();
                *sample = self.delay[self.delay_read] * gain;

                self.delay_read = (self.delay_read + 1) % self.delay_size;
                self.delay_write = (self.delay_write + 1) % self.delay_size;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{db, peak, sine};

    /// The pre-delay is 6 ms; the read cursor starts one ahead of the write cursor in a ring of
    /// that size, so samples come out one sample earlier.
    const DELAY: usize = (SAMPLE_RATE as f32 * PREDELAY_S) as usize - 1;

    fn compressor() -> Compressor {
        Compressor::new()
    }

    /// Steady-state output level in dB for a 450 Hz sine at `input_db` dBFS peak.
    fn level(input_db: f32) -> f32 {
        let mut x = sine(450.0, 10f64.powf(input_db as f64 / 20.0), 48_000);
        compressor().process(&mut x);
        db(peak(&x[24_000..]))
    }

    #[test]
    fn silence_stays_silent() {
        let mut x = vec![0.0; 4800];
        compressor().process(&mut x);
        assert!(x.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn output_is_delayed_by_the_pre_delay() {
        let mut x = vec![0.0; 1000];
        x[100] = 0.01;
        compressor().process(&mut x);
        let first = x.iter().position(|&s| s != 0.0);
        assert_eq!(first, Some(100 + DELAY));
    }

    #[test]
    fn starts_settled_at_the_makeup_gain() {
        // sndfilter starts fully compressed and takes about 0.3 s to release; this port starts
        // settled, so the very first output sample already has the steady-state gain.
        let mut c = compressor();
        let mut x = vec![0.01; 48_000];
        c.process(&mut x);
        let gain = |i: usize| x[i] / 0.01;
        for i in [DELAY, 4800, 24_000, 47_999] {
            assert!((gain(i) - c.master_gain).abs() < 1e-4 * c.master_gain, "sample {i}: {}", gain(i));
        }
    }

    #[test]
    fn quiet_signals_get_a_constant_makeup_gain() {
        // Below the −30 dB threshold the compressor is a delay with constant gain. 1000 samples is
        // not a whole number of 32-sample chunks, so this also checks that the trailing partial
        // chunk is processed.
        let mut c = compressor();
        let input = sine(450.0, 0.01, 1000);
        let mut out = input.clone();
        c.process(&mut out);
        assert!(c.master_gain > 1.0);
        for i in DELAY..input.len() {
            assert!((out[i] - input[i - DELAY] * c.master_gain).abs() < 1e-6, "sample {i}");
        }
    }

    #[test]
    fn makeup_gain_matches_the_soft_knee_design() {
        // Re-derive sndfilter's makeup gain in f64: the knee constant k makes the curve's slope
        // 1/ratio at the end of the knee (here 0 dBFS, so x = 1), and the makeup gain is
        // (1 / curve(1))^0.6. A quiet signal's gain must equal it.
        let (threshold, ratio) = (db_to_lin(-30.0) as f64, 12.0);
        let slope = |k: f64| k / ((k * threshold + 1.0) * (k * (1.0 - threshold)).exp() - 1.0);
        let (mut lo, mut hi) = (0.1f64, 10_000.0f64);
        for _ in 0..100 {
            let k = (lo * hi).sqrt();
            if slope(k) < 1.0 / ratio { hi = k } else { lo = k }
        }
        let k = (lo * hi).sqrt();
        let curve_at_1 = threshold + (1.0 - (-k * (1.0 - threshold)).exp()) / k;
        let expected = (1.0 / curve_at_1).powf(0.6) as f32;

        let mut c = compressor();
        let mut x = vec![0.001; DELAY + 1];
        c.process(&mut x);
        let gain = x[DELAY] / 0.001;
        assert!((db(gain) - db(expected)).abs() < 0.01, "{} dB, expected {} dB", db(gain), db(expected));
    }

    #[test]
    fn static_curve_is_linear_below_threshold_and_soft_knee_above() {
        assert!((level(-40.0) - level(-60.0) - 20.0).abs() < 0.01);
        // Threshold −30 dB with a 30 dB knee: each 10 dB step above the threshold raises the
        // output less than the step before, and always less than 10 dB.
        let steps: Vec<f32> = [-30.0, -20.0, -10.0, 0.0].windows(2).map(|w| level(w[1]) - level(w[0])).collect();
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{steps:?}");
        assert!(steps.iter().all(|&s| s > 0.0 && s < 10.0), "{steps:?}");
    }

    #[test]
    fn look_ahead_compresses_an_onset_before_it_is_heard() {
        let mut x = sine(450.0, 0.01, 48_000);
        x.extend(sine(450.0, 1.0, 48_000));
        let input = x.clone();
        compressor().process(&mut x);
        let gain_at = |at: usize| peak(&x[at + DELAY..at + DELAY + 107]) / peak(&input[at..at + 107]);
        let steady = gain_at(90_000);
        assert!(gain_at(48_000) < steady * 1.05, "onset {} vs steady {steady}", gain_at(48_000));
    }

    #[test]
    fn release_recovers_the_makeup_gain_over_a_quarter_second() {
        let mut x = sine(450.0, 1.0, 48_000);
        x.extend(sine(450.0, 0.01, 96_000));
        let input = x.clone();
        let mut c = compressor();
        c.process(&mut x);
        let gain_at = |ms: usize| {
            let at = 48_000 + ms * 48;
            peak(&x[at + DELAY..at + DELAY + 107]) / peak(&input[at..at + 107])
        };
        assert!(gain_at(10) < 0.5 * c.master_gain, "still compressing 10 ms after: {}", gain_at(10));
        assert!((gain_at(500) - c.master_gain).abs() < 0.01 * c.master_gain, "recovered: {}", gain_at(500));
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let input: Vec<f32> = sine(450.0, 1.0, 4800).into_iter().chain(sine(450.0, 0.01, 4800)).collect();
        let mut c = compressor();
        let mut first = input.clone();
        c.process(&mut first);
        c.reset();
        let mut second = input;
        c.process(&mut second);
        assert_eq!(first, second);
    }

    #[test]
    fn state_carries_across_calls() {
        let input: Vec<f32> = sine(450.0, 0.01, 4800).into_iter().chain(sine(450.0, 1.0, 4800)).collect();
        let mut whole = input.clone();
        compressor().process(&mut whole);
        let mut framed = input;
        let mut c = compressor();
        framed.chunks_mut(960).for_each(|f| c.process(f));
        assert_eq!(whole, framed);
    }
}
