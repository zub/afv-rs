//! Mistuning and multipath fading of one station's signal.
//!
//! # References
//!
//! - C. C. Watterson, J. R. Juroshek and W. D. Bensema, "Experimental confirmation of an HF
//!   channel model", *IEEE Transactions on Communication Technology* 18(6), 1970: each path's
//!   gain is a complex Gaussian process with a Gaussian Doppler spectrum.
//! - M. Pätzold, *Mobile Fading Channels*, Wiley, 2002: synthesising such a process as a sum of
//!   sinusoids with randomly drawn Doppler frequencies (the Monte Carlo method).
//! - S. O. Rice, "Mathematical analysis of random noise", *Bell System Technical Journal* 23–24,
//!   1944–45: the level-crossing rate the fading-speed test checks against.
//! - O. Niemitalo, ["Polyphase IIR Hilbert transformers"](https://yehar.com/blog/?p=368): the
//!   allpass pair that makes the analytic signal for the frequency shift.
//! - Naval Postgraduate School, 1970 report on tuning SSB by ear
//!   ([calhoun.nps.edu](https://calhoun.nps.edu/entities/publication/f341cc35-61c1-484b-a130-bbc41fa9e149)):
//!   errors of 20–30 Hz are already noticeable.

use std::ops::RangeInclusive;

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::StandardNormal;

use super::HfConditions;
use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

const FRAME_SECONDS: f64 = FRAME_SAMPLES as f64 / SAMPLE_RATE as f64;
/// Drift heads for a new random target every 100–300 frames (2–6 s)…
const DRIFT_HOLD_FRAMES: RangeInclusive<u32> = 100..=300;
/// …and approaches it with this time constant, in seconds.
const DRIFT_TIME_CONSTANT: f64 = 1.5;
/// Slow fading changes over this time constant, in seconds…
const SLOW_FADING_TIME_CONSTANT: f64 = 8.0;
/// …and rises at most this far above the nominal level, in dB (fades are not limited).
const SLOW_FADING_MAX_DB: f64 = 6.0;
/// Mixed into the callsign's hash, so that a station's channel draws different random numbers
/// from a radio's background.
const CHANNEL_STREAM: u64 = 0x6866_5f63_6861_6e6e;
/// Sinusoids summed per path to synthesise its Gaussian fading process.
const SINUSOIDS: usize = 16;
/// Delay line for the second path: up to 10.6 ms.
const HISTORY: usize = 512;

/// One first-order section of the allpass chains: `y[n] = c·(x[n] + y[n−2]) − x[n−2]`.
#[derive(Debug, Clone, Default)]
struct Allpass {
    c: f64,
    x: [f64; 2],
    y: [f64; 2],
}

impl Allpass {
    fn process(&mut self, x: f64) -> f64 {
        let y = self.c * (x + self.y[1]) - self.x[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// Splits a signal into two outputs 90° apart, using two chains of allpass filters (Niemitalo's
/// 8th-order polyphase IIR design). Both outputs keep the input's magnitude.
#[derive(Debug, Clone)]
struct Hilbert {
    a: [Allpass; 4],
    b: [Allpass; 4],
    delayed: f64,
}

/// The two allpass chains of Olli Niemitalo's design (each coefficient is squared in the filter):
/// 90° apart to within a degree from about 20 Hz to 23.9 kHz at 48 kHz.
const HILBERT_A: [f64; 4] = [0.6923878, 0.9360654322959, 0.9882295226860, 0.9987488452737];
const HILBERT_B: [f64; 4] = [0.4021921162426, 0.8561710882420, 0.9722909545651, 0.9952884791278];

impl Default for Hilbert {
    fn default() -> Self {
        let chain = |coefs: [f64; 4]| coefs.map(|a| Allpass { c: a * a, ..Allpass::default() });
        Self { a: chain(HILBERT_A), b: chain(HILBERT_B), delayed: 0.0 }
    }
}

impl Hilbert {
    /// Returns `(i, q)`, with `q` leading `i` by 90°.
    fn process(&mut self, x: f32) -> (f32, f32) {
        let i = self.a.iter_mut().fold(x as f64, |s, f| f.process(s));
        let q = self.b.iter_mut().fold(x as f64, |s, f| f.process(s));
        let out = (self.delayed as f32, q as f32);
        self.delayed = i;
        out
    }
}

type Complex = (f64, f64);

fn mul(a: Complex, b: Complex) -> Complex {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn lerp(a: Complex, b: Complex, t: f64) -> Complex {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// The complex gain of one propagation path (Watterson et al. 1970): a steady component plus a
/// scattered one whose spectrum is Gaussian, synthesised as a sum of sinusoids at normally
/// distributed Doppler frequencies (Pätzold's Monte Carlo method).
#[derive(Debug, Clone)]
struct Path {
    /// Doppler frequencies in units of σ.
    doppler: [f64; SINUSOIDS],
    /// Phases in turns.
    phases: [f64; SINUSOIDS],
    /// The steady component, a unit phasor.
    steady: Complex,
}

impl Path {
    fn new(rng: &mut SmallRng) -> Self {
        let mut doppler: [f64; SINUSOIDS] = std::array::from_fn(|_| rng.sample::<f64, _>(StandardNormal));
        // Scale to exactly σ RMS: the fading speed depends on it, and 16 random draws would
        // otherwise leave it up to ±30 % off.
        let rms = (doppler.iter().map(|f| f * f).sum::<f64>() / SINUSOIDS as f64).sqrt();
        doppler.iter_mut().for_each(|f| *f /= rms);
        let phases = std::array::from_fn(|_| rng.random::<f64>());
        let angle = std::f64::consts::TAU * rng.random::<f64>();
        Self { doppler, phases, steady: (angle.cos(), angle.sin()) }
    }

    /// Moves on by `seconds` with a Doppler spread (2σ) of `spread_hz`.
    fn advance(&mut self, spread_hz: f64, seconds: f64) {
        let sigma = spread_hz / 2.0;
        for (phase, f) in self.phases.iter_mut().zip(&self.doppler) {
            *phase = (*phase + f * sigma * seconds).rem_euclid(1.0);
        }
    }

    /// The gain, with unit mean power; `depth` is the scattered share of that power.
    fn gain(&self, depth: f64) -> Complex {
        let scattered = self.phases.iter().fold((0.0, 0.0), |(re, im), p| {
            let (s, c) = (std::f64::consts::TAU * p).sin_cos();
            (re + c, im + s)
        });
        let (k, m) = ((depth / SINUSOIDS as f64).sqrt(), (1.0 - depth).sqrt());
        (m * self.steady.0 + k * scattered.0, m * self.steady.1 + k * scattered.1)
    }
}

/// HF propagation for one station: everything the ionosphere and a mistuned receiver do to its
/// voice. Deterministic for a given seed.
#[derive(Debug, Clone)]
pub struct HfChannel {
    hilbert: Hilbert,
    rng: SmallRng,
    /// The station's tuning error as a fraction of [`HfConditions::offset_hz`].
    offset: f64,
    /// Current drift as a fraction of [`HfConditions::drift_hz`], heading for `drift_target`.
    drift: f64,
    drift_target: f64,
    drift_hold: u32,
    /// Oscillator phase in turns.
    phase: f64,
    /// Slow fading state, a unit-variance Gaussian process.
    slow: f64,
    paths: [Path; 2],
    /// Path gains at the end of the previous frame, interpolated from in the next; `None` before
    /// the first frame.
    gains: Option<[Complex; 2]>,
    /// The analytic signal, for the delayed path.
    history: Box<[Complex; HISTORY]>,
    write: usize,
    /// The second path's delay in samples during the previous frame.
    delay: usize,
}

impl HfChannel {
    pub fn new(seed: u64) -> Self {
        let mut rng = SmallRng::seed_from_u64(seed);
        let offset = rng.random_range(-1.0..1.0);
        let drift = rng.random_range(-1.0..1.0);
        let drift_target = rng.random_range(-1.0..1.0);
        let drift_hold = rng.random_range(DRIFT_HOLD_FRAMES);
        let paths = [Path::new(&mut rng), Path::new(&mut rng)];
        let slow = rng.sample::<f64, _>(StandardNormal);
        Self {
            hilbert: Hilbert::default(),
            rng,
            offset,
            drift,
            drift_target,
            drift_hold,
            phase: 0.0,
            slow,
            paths,
            gains: None,
            history: Box::new([(0.0, 0.0); HISTORY]),
            write: 0,
            delay: 0,
        }
    }

    /// The channel for a station, the same on every run.
    pub fn for_callsign(callsign: &str) -> Self {
        Self::new(crate::dsp::callsign_hash(callsign) ^ CHANNEL_STREAM)
    }

    /// The current frequency shift under `conditions`, in Hz.
    pub fn shift_hz(&self, conditions: &HfConditions) -> f64 {
        let c = conditions.sanitized();
        self.offset * c.offset_hz as f64 + self.drift * c.drift_hz as f64
    }

    /// Processes one frame of the station's audio.
    pub fn process(&mut self, conditions: &HfConditions, input: &Frame, out: &mut Frame) {
        let c = conditions.sanitized();
        self.advance_slow();
        let spread = c.fading_rate_hz.max(0.0) as f64;
        // Without a Doppler spread the scattered part would freeze, possibly in a deep fade.
        let depth = if spread > 0.0 { c.fading_depth.clamp(0.0, 1.0) as f64 } else { 0.0 };
        let delay =
            ((c.multipath_delay_ms.max(0.0) as f64 * SAMPLE_RATE as f64 / 1000.0).round() as usize).min(HISTORY - 1);
        for path in &mut self.paths {
            path.advance(spread, FRAME_SECONDS);
        }
        // Two paths share the power equally; slow fading scales both.
        let slow = self.slow_gain(&c);
        let target = if delay > 0 {
            let w = std::f64::consts::FRAC_1_SQRT_2 * slow;
            self.paths.each_ref().map(|p| {
                let g = p.gain(depth);
                (g.0 * w, g.1 * w)
            })
        } else {
            let g = self.paths[0].gain(depth);
            [(g.0 * slow, g.1 * slow), (0.0, 0.0)]
        };
        let start = self.gains.unwrap_or(target);
        let previous_delay = if self.gains.is_some() { self.delay } else { delay };
        let step = self.shift_hz(&c) / SAMPLE_RATE as f64;

        for (n, (o, &x)) in out.iter_mut().zip(input).enumerate() {
            let t = (n + 1) as f64 / FRAME_SAMPLES as f64;
            let (i, q) = self.hilbert.process(x);
            // The analytic signal, so that multiplying by e^{jθ} shifts frequencies upwards.
            let z = (i as f64, -(q as f64));
            self.history[self.write] = z;
            let at = |d: usize| self.history[(self.write + HISTORY - d) % HISTORY];
            // A change of delay crossfades between the old and the new position.
            let delayed = if previous_delay == delay { at(delay) } else { lerp(at(previous_delay), at(delay), t) };
            self.write = (self.write + 1) % HISTORY;

            let h = mul(lerp(start[0], target[0], t), z);
            let h2 = mul(lerp(start[1], target[1], t), delayed);
            let (sin, cos) = (std::f64::consts::TAU * self.phase).sin_cos();
            *o = ((h.0 + h2.0) * cos - (h.1 + h2.1) * sin) as f32;
            self.phase = (self.phase + step).rem_euclid(1.0);
        }
        self.gains = Some(target);
        self.delay = delay;
    }

    /// Moves drift and slow fading on by one frame.
    fn advance_slow(&mut self) {
        self.drift_hold = self.drift_hold.saturating_sub(1);
        if self.drift_hold == 0 {
            self.drift_target = self.rng.random_range(-1.0..1.0);
            self.drift_hold = self.rng.random_range(DRIFT_HOLD_FRAMES);
        }
        let k = 1.0 - (-FRAME_SECONDS / DRIFT_TIME_CONSTANT).exp();
        self.drift += (self.drift_target - self.drift) * k;

        let a = (-FRAME_SECONDS / SLOW_FADING_TIME_CONSTANT).exp();
        self.slow = a * self.slow + (1.0 - a * a).sqrt() * self.rng.sample::<f64, _>(StandardNormal);
    }

    /// The slow fading amplitude: log-normal with the configured spread, rising at most
    /// [`SLOW_FADING_MAX_DB`] above nominal.
    fn slow_gain(&self, c: &HfConditions) -> f64 {
        let db = c.slow_fading_db.max(0.0) as f64 * self.slow;
        let db = if db > 0.0 { SLOW_FADING_MAX_DB * (db / SLOW_FADING_MAX_DB).tanh() } else { db };
        10f64.powf(db / 20.0)
    }

    /// The slow fading level in the last frame, in dB.
    #[cfg(test)]
    fn slow_db(&self, c: &HfConditions) -> f64 {
        20.0 * self.slow_gain(c).log10()
    }

    /// Total power of the path gains in the last frame.
    #[cfg(test)]
    fn power(&self) -> f64 {
        self.gains.map_or(0.0, |g| g.iter().map(|(re, im)| re * re + im * im).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{sine, sine_frame, tone};

    /// Complex DFT bin of `x` at `freq`, as (amplitude, phase in degrees).
    fn bin(x: &[f32], freq: f64) -> (f64, f64) {
        let (mut re, mut im) = (0f64, 0f64);
        for (n, s) in x.iter().enumerate() {
            let a = std::f64::consts::TAU * freq * n as f64 / SAMPLE_RATE as f64;
            re += *s as f64 * a.cos();
            im -= *s as f64 * a.sin();
        }
        (2.0 * re.hypot(im) / x.len() as f64, im.atan2(re).to_degrees())
    }

    fn shift_only(offset_hz: f32, drift_hz: f32) -> HfConditions {
        HfConditions { offset_hz, drift_hz, ..HfConditions::OFF }
    }

    fn fading(delay_ms: f32, rate_hz: f32, depth: f32) -> HfConditions {
        HfConditions { multipath_delay_ms: delay_ms, fading_rate_hz: rate_hz, fading_depth: depth, ..HfConditions::OFF }
    }

    /// Runs `frames` frames of a tone through `ch`.
    fn render(ch: &mut HfChannel, c: &HfConditions, freq: f64, frames: usize) -> Vec<f32> {
        let mut out = [0.0; FRAME_SAMPLES];
        (0..frames)
            .flat_map(|n| {
                ch.process(c, &sine_frame(n, freq, 0.5), &mut out);
                out
            })
            .collect()
    }

    /// Path power per frame over `frames` frames of silence.
    fn powers(c: &HfConditions, frames: usize, seed: u64) -> Vec<f64> {
        let mut ch = HfChannel::new(seed);
        let (silence, mut out) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
        (0..frames)
            .map(|_| {
                ch.process(c, &silence, &mut out);
                ch.power()
            })
            .collect()
    }

    #[test]
    fn hilbert_outputs_are_equal_and_in_quadrature() {
        for freq in [50.0, 300.0, 1000.0, 3000.0, 10_000.0] {
            let mut h = Hilbert::default();
            let (i, q): (Vec<f32>, Vec<f32>) = sine(freq, 0.5, 48_000).into_iter().map(|x| h.process(x)).unzip();
            let ((ai, pi), (aq, pq)) = (bin(&i[24_000..], freq), bin(&q[24_000..], freq));
            assert!((ai - 0.5).abs() < 0.005 && (aq - 0.5).abs() < 0.005, "{freq} Hz: amplitudes {ai} {aq}");
            let lead = (pq - pi).rem_euclid(360.0);
            assert!((lead - 90.0).abs() < 1.0, "{freq} Hz: q leads i by {lead}°");
        }
    }

    #[test]
    fn off_keeps_every_tone_and_level() {
        let mut ch = HfChannel::new(1);
        let out = render(&mut ch, &HfConditions::OFF, 1000.0, 50);
        assert!((tone(&out[24_000..], 1000.0) - 0.5).abs() < 0.005, "{}", tone(&out[24_000..], 1000.0));
        assert!((ch.power() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_offset_shifts_every_tone_by_the_same_hz() {
        for (offset, expected) in [(1.0, 1050.0), (-1.0, 950.0)] {
            let mut ch = HfChannel::new(1);
            ch.offset = offset;
            let out = render(&mut ch, &shift_only(50.0, 0.0), 1000.0, 50);
            let settled = &out[24_000..];
            assert!((tone(settled, expected) - 0.5).abs() < 0.01, "{}", tone(settled, expected));
            let image = 2000.0 - expected;
            assert!(tone(settled, image) < 0.005, "image at {image} Hz: {}", tone(settled, image));
            assert!(tone(settled, 1000.0) < 0.005, "unshifted: {}", tone(settled, 1000.0));
        }
    }

    #[test]
    fn the_shift_is_an_offset_not_a_scaling() {
        // Harmonics 500 and 1000 Hz move by the same 30 Hz, to 530 and 1030 Hz.
        let mut ch = HfChannel::new(1);
        ch.offset = 1.0;
        let c = shift_only(30.0, 0.0);
        let mut out = [0.0; FRAME_SAMPLES];
        let rendered: Vec<f32> = (0..50)
            .flat_map(|n| {
                let a = sine_frame(n, 500.0, 0.3);
                let b = sine_frame(n, 1000.0, 0.3);
                let mix: Frame = std::array::from_fn(|i| a[i] + b[i]);
                ch.process(&c, &mix, &mut out);
                out
            })
            .collect();
        let settled = &rendered[24_000..];
        assert!(tone(settled, 530.0) > 0.29 && tone(settled, 1030.0) > 0.29);
        assert!(tone(settled, 1060.0) < 0.005, "not scaled");
    }

    #[test]
    fn drift_wanders_smoothly_within_its_range() {
        let c = shift_only(0.0, 10.0);
        let mut ch = HfChannel::new(7);
        let mut out = [0.0; FRAME_SAMPLES];
        let silence = [0.0; FRAME_SAMPLES];
        let shifts: Vec<f64> = (0..3000)
            .map(|_| {
                ch.process(&c, &silence, &mut out);
                ch.shift_hz(&c)
            })
            .collect();
        assert!(shifts.iter().all(|s| s.abs() <= 10.0));
        let k = 1.0 - (-FRAME_SECONDS / DRIFT_TIME_CONSTANT).exp();
        assert!(shifts.windows(2).all(|w| (w[1] - w[0]).abs() <= 20.0 * k), "moves at most k of the range per frame");
        let (lo, hi) = shifts.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s)));
        assert!(hi - lo > 8.0, "it wanders: {lo:.1}..{hi:.1} Hz");
    }

    #[test]
    fn the_shift_stays_within_offset_plus_drift() {
        let c = shift_only(40.0, 5.0);
        for seed in 0..200 {
            assert!(HfChannel::new(seed).shift_hz(&c).abs() <= 45.0);
        }
    }

    #[test]
    fn neighbouring_callsigns_get_unrelated_channels() {
        let offsets: Vec<f64> = ["SAS120", "SAS121", "SAS122", "SAS123"]
            .iter()
            .map(|c| HfChannel::for_callsign(c).shift_hz(&shift_only(100.0, 0.0)))
            .collect();
        assert!(offsets.windows(2).all(|w| (w[0] - w[1]).abs() > 1.0), "{offsets:?}");
    }

    #[test]
    fn stations_are_deterministic_and_distinct() {
        let c = HfConditions::FAIR;
        let a = render(&mut HfChannel::for_callsign("SAS123"), &c, 1000.0, 20);
        let b = render(&mut HfChannel::for_callsign("SAS123"), &c, 1000.0, 20);
        let other = render(&mut HfChannel::for_callsign("NAX45F"), &c, 1000.0, 20);
        assert_eq!(a, b);
        assert_ne!(a, other);
    }

    #[test]
    fn no_fading_keeps_a_constant_level() {
        // Neither depth without a rate nor a rate without depth fades a single path.
        for c in [fading(0.0, 0.0, 1.0), fading(0.0, 1.0, 0.0)] {
            let mut ch = HfChannel::new(3);
            let out = render(&mut ch, &c, 1000.0, 100);
            let peaks: Vec<f32> =
                out.chunks(FRAME_SAMPLES).skip(5).map(|f| f.iter().fold(0f32, |m, s| m.max(s.abs()))).collect();
            assert!(peaks.iter().all(|p| (p - 0.5).abs() < 0.01), "{c:?}: {peaks:?}");
        }
    }

    #[test]
    fn full_fading_is_rayleigh() {
        // Ten minutes of a single fully scattered path.
        let p = powers(&fading(0.0, 0.5, 1.0), 30_000, 11);
        let mean = p.iter().sum::<f64>() / p.len() as f64;
        assert!((mean - 1.0).abs() < 0.2, "mean power {mean}");
        // Rayleigh: P(10 dB below the mean) = 1 − e^−0.1 ≈ 9.5 %.
        let deep = p.iter().filter(|&&p| p < 0.1).count() as f64 / p.len() as f64;
        assert!((0.05..0.15).contains(&deep), "{:.1} % of the time 10 dB down", deep * 100.0);
    }

    #[test]
    fn two_paths_share_the_power() {
        let p = powers(&fading(2.0, 0.5, 1.0), 30_000, 15);
        let mean = p.iter().sum::<f64>() / p.len() as f64;
        assert!((mean - 1.0).abs() < 0.2, "mean power {mean}");
        // Two independent paths fade less deeply than one.
        let deep = p.iter().filter(|&&p| p < 0.1).count() as f64 / p.len() as f64;
        assert!(deep < 0.05, "{:.1} % of the time 10 dB down", deep * 100.0);
    }

    #[test]
    fn a_steady_component_makes_fades_rarer() {
        // Rician with K = (1 − depth) / depth: about 0.1·(1 + K)·e^−K of the time 10 dB down,
        // so 7.4 % at depth 0.5 and 0.9 % at depth 0.2, against 9.5 % for full Rayleigh.
        let deep = |depth| {
            let p = powers(&fading(0.0, 0.5, depth), 30_000, 12);
            p.iter().filter(|&&p| p < 0.1).count() as f64 / p.len() as f64
        };
        let (half, fifth) = (deep(0.5), deep(0.2));
        assert!((0.04..0.11).contains(&half), "{:.1} % at depth 0.5", half * 100.0);
        assert!(fifth < 0.025, "{:.1} % at depth 0.2", fifth * 100.0);
    }

    #[test]
    fn the_doppler_spread_sets_the_fading_speed() {
        // Count how often the power crosses its mean: proportional to the spread.
        let crossings = |rate| {
            let p = powers(&fading(0.0, rate, 1.0), 30_000, 13);
            p.windows(2).filter(|w| (w[0] < 1.0) != (w[1] < 1.0)).count() as f64
        };
        let (slow, fast) = (crossings(0.2), crossings(2.0));
        assert!((5.0..20.0).contains(&(fast / slow)), "{fast} vs {slow} crossings");
        // Rice's level-crossing rate for Rayleigh fading with a Gaussian Doppler spectrum of
        // standard deviation σ: the mean power is crossed upwards 2√π·σ/e ≈ 1.30·σ times a
        // second. A 2 Hz spread is σ = 1 Hz: about 1565 crossings either way in ten minutes.
        assert!((1300.0..1850.0).contains(&fast), "{fast} crossings in 600 s at a 2 Hz spread");
    }

    #[test]
    fn two_paths_make_notches_one_over_the_delay_apart() {
        // Steady paths 1 ms apart: the response repeats every 1 kHz and has deep notches.
        let response = |freq: f64| {
            let mut ch = HfChannel::new(14);
            tone(&render(&mut ch, &fading(1.0, 0.0, 0.0), freq, 25)[12_000..], freq)
        };
        let sweep: Vec<f32> = (0..50).map(|k| response(300.0 + 20.0 * k as f64)).collect();
        let (lo, hi) = sweep.iter().fold((f32::MAX, 0f32), |(lo, hi), &a| (lo.min(a), hi.max(a)));
        assert!(hi > 10.0 * lo, "notch depth: {lo} vs {hi}");
        for f in [400.0, 650.0, 900.0] {
            assert!((response(f) - response(f + 1000.0)).abs() < 0.01, "periodic in 1 kHz at {f} Hz");
        }
    }

    #[test]
    fn bad_settings_do_not_break_the_channel() {
        let mut ch = HfChannel::new(2);
        let bad = HfConditions {
            offset_hz: f32::NAN,
            drift_hz: f32::INFINITY,
            multipath_delay_ms: f32::NAN,
            fading_rate_hz: f32::NEG_INFINITY,
            fading_depth: f32::NAN,
            ..HfConditions::FAIR
        };
        let out = render(&mut ch, &bad, 1000.0, 10);
        assert!(out.iter().all(|s| s.is_finite()));
        let out = render(&mut ch, &HfConditions::FAIR, 1000.0, 50);
        assert!(out.iter().all(|s| s.is_finite()) && out.iter().any(|s| s.abs() > 0.05));
    }

    #[test]
    fn changing_conditions_mid_stream_does_not_click() {
        // Steps where the conditions change are no larger than the signal's own steps elsewhere.
        let presets =
            [HfConditions::POOR, HfConditions::OFF, HfConditions::FLUTTER, HfConditions::GOOD, HfConditions::FAIR];
        let mut ch = HfChannel::new(9);
        let mut out = Vec::new();
        let mut frame = [0.0; FRAME_SAMPLES];
        for n in 0..presets.len() * 20 {
            ch.process(&presets[n / 20], &sine_frame(n, 1000.0, 0.5), &mut frame);
            out.extend_from_slice(&frame);
        }
        let changes: Vec<usize> = (1..presets.len()).map(|k| k * 20 * FRAME_SAMPLES).collect();
        let near_change = |i: usize| changes.iter().any(|&c| (c - 5..c + FRAME_SAMPLES).contains(&i));
        let step = |i: usize| (out[i] - out[i - 1]).abs();
        let worst = (FRAME_SAMPLES..out.len()).filter(|&i| near_change(i)).map(step).fold(0f32, f32::max);
        let elsewhere = (FRAME_SAMPLES..out.len()).filter(|&i| !near_change(i)).map(step).fold(0f32, f32::max);
        assert!(worst <= elsewhere, "step {worst} at a change, {elsewhere} at most otherwise");
    }

    #[test]
    fn fast_fading_is_smooth_across_frames() {
        // Under flutter the path gains change a lot per frame; frame boundaries must not step.
        let mut ch = HfChannel::new(16);
        let out =
            render(&mut ch, &HfConditions { offset_hz: 0.0, drift_hz: 0.0, ..HfConditions::FLUTTER }, 1000.0, 500);
        let step = |i: usize| (out[i] - out[i - 1]).abs();
        let at_boundaries = (1..500).map(|f| step(f * FRAME_SAMPLES)).fold(0f32, f32::max);
        let within = (1..out.len()).filter(|i| i % FRAME_SAMPLES != 0).map(step).fold(0f32, f32::max);
        assert!(at_boundaries <= within, "step {at_boundaries} at frame boundaries, {within} within frames");
    }

    #[test]
    fn slow_fading_is_log_normal_and_slow() {
        // An hour of the slow fading process alone.
        let c = HfConditions { slow_fading_db: 6.0, ..HfConditions::OFF };
        let mut ch = HfChannel::new(17);
        let (mut db, mut state) = (Vec::new(), Vec::new());
        for _ in 0..50 * 3600 {
            ch.advance_slow();
            db.push(ch.slow_db(&c));
            state.push(ch.slow);
        }
        let mut sorted = db.clone();
        sorted.sort_by(f64::total_cmp);
        let (p5, p50) = (sorted[sorted.len() / 20], sorted[sorted.len() / 2]);
        assert!((p5 + 1.645 * 6.0).abs() < 1.5, "5th percentile {p5:.1} dB, expected −9.9");
        assert!(p50.abs() < 1.0, "median {p50:.1} dB");
        assert!(sorted[sorted.len() - 1] < SLOW_FADING_MAX_DB, "rises at most {SLOW_FADING_MAX_DB} dB");
        // A first-order process with an 8 s time constant: correlation e^(−t/8).
        let corr = |lag: usize| {
            let n = state.len() - lag;
            (0..n).map(|i| state[i] * state[i + lag]).sum::<f64>() / n as f64
        };
        let var = corr(0);
        assert!((corr(50) / var - (-1f64 / 8.0).exp()).abs() < 0.05, "after 1 s: {}", corr(50) / var);
        assert!((corr(1500) / var).abs() < 0.2, "after 30 s: {}", corr(1500) / var);
    }

    #[test]
    fn slow_fading_reaches_the_audio() {
        let c = HfConditions { slow_fading_db: 10.0, ..HfConditions::OFF };
        let mut ch = HfChannel::new(18);
        let mut out = [0.0; FRAME_SAMPLES];
        let mut previous = 0.0;
        for n in 0..500 {
            ch.process(&c, &sine_frame(n, 1000.0, 0.5), &mut out);
            // The gain ramps across each frame, so its level is between the last two values.
            let expected = (previous + ch.slow_db(&c)) / 2.0;
            previous = ch.slow_db(&c);
            if n > 10 {
                let rms = (out.iter().map(|s| s * s).sum::<f32>() / FRAME_SAMPLES as f32).sqrt();
                let level = 20.0 * (rms / (0.5 / std::f32::consts::SQRT_2)).log10() as f64;
                assert!((level - expected).abs() < 0.2, "frame {n}: {level:.2} dB vs {expected:.2}");
            }
        }
        // Without it the level does not move.
        let mut ch = HfChannel::new(18);
        ch.process(&HfConditions::OFF, &sine_frame(0, 1000.0, 0.5), &mut out);
        assert_eq!(ch.slow_db(&HfConditions::OFF), 0.0);
    }
}
