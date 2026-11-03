//! HF propagation for single-sideband voice: receiver mistuning, the Watterson multipath fading
//! model, and atmospheric static. See `docs/HF.md` for a plain description.
//!
//! - **Mistuning.** Aviation HF is upper sideband without a carrier, so a receiver tuned off the
//!   transmitter shifts every tone of the voice by the same number of hertz. Unlike a pitch
//!   change, this pulls the harmonics out of their ratios: voices sound squeaky or growly.
//! - **Multipath fading** (Watterson, Juroshek and Bensema 1970; CCIR 520, ITU-R F.1487). The
//!   signal arrives over two ionospheric paths a few milliseconds apart. Each path's gain is a
//!   complex random process whose spectrum is a Gaussian as wide as the Doppler spread, so it
//!   varies in level and in phase. Two copies a delay τ apart cancel at frequencies 1/τ apart:
//!   notches sweep through the voice as the gains change, and the phase changes make it warble.
//! - **Static.** Lightning anywhere within thousands of kilometres reaches the receiver as short
//!   broadband impulses. The receiver's steep sideband filter ([`SsbFilter`]) rings with each
//!   one, turning it into a brief blip of a few milliseconds.
//!
//! [`HfChannel`] applies mistuning and fading to one station; [`Crackle`] makes the static at the
//! receiver. Both take their strengths from [`HfConditions`].

use std::ops::RangeInclusive;

use super::Biquad;
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
/// Mixed into seeds so that a station's channel and a radio's static, hiss and sounders draw
/// unrelated random numbers from the same seed.
const CHANNEL_STREAM: u64 = 0x6866_5f63_6861_6e6e;
const CRACKLE_STREAM: u64 = 0x7374_6174_6963_2121;
const HISS_STREAM: u64 = 0x6869_7373_6869_7373;
const SOUNDER_STREAM: u64 = 0x736f_756e_6465_7273;
/// Sinusoids summed per path to synthesise its Gaussian fading process.
const SINUSOIDS: usize = 16;
/// Delay line for the second path: up to 10.6 ms.
const HISTORY: usize = 512;

/// How strongly HF propagation colours received voice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HfConditions {
    /// Largest tuning error between a station and the receiver, in Hz. Each station keeps its own
    /// fixed error within ±this.
    pub offset_hz: f32,
    /// Largest slow drift around a station's tuning error, in Hz.
    pub drift_hz: f32,
    /// Delay between the two propagation paths, in ms; 0 for a single path. Watterson's
    /// reference channels use 0.5 (good), 1 (moderate) and 2 ms (poor).
    pub multipath_delay_ms: f32,
    /// Doppler spread of each path in Hz, i.e. how fast it fades: 0.1 (good), 0.5 (moderate),
    /// 1 (poor), 10 (polar flutter). 0 stops fading.
    pub fading_rate_hz: f32,
    /// 0 for steady paths, up to 1 for full Rayleigh fading with deep nulls; in between, a steady
    /// component with fading on top (Rician).
    pub fading_depth: f32,
    /// Slow fading on top, from changing ionospheric absorption: the standard deviation of the
    /// signal level in dB, varying over tens of seconds (log-normal). 0 for none.
    pub slow_fading_db: f32,
    /// Background noise relative to the voice: 1 makes it as loud as an unfaded signal (0 dB
    /// signal-to-noise ratio), 0.25 is 12 dB below.
    pub noise_level: f32,
    /// Static crashes per second from distant lightning, on average; each may set off a run of
    /// crackling.
    pub crackle_rate_hz: f32,
    /// Loudness of the static, 0 to 1.
    pub crackle_level: f32,
    /// Ionospheric sounders sweeping past the frequency, per second on average; each is heard as
    /// a short rising blip.
    pub sounder_rate_hz: f32,
    /// Loudness of the sounders, 0 to 1.
    pub sounder_level: f32,
}

impl HfConditions {
    /// Clean single-sideband audio. The signal still passes the channel's allpass filters, which
    /// change its phase but not its sound, so switching conditions never clicks.
    pub const OFF: Self = Self {
        offset_hz: 0.0,
        drift_hz: 0.0,
        multipath_delay_ms: 0.0,
        fading_rate_hz: 0.0,
        fading_depth: 0.0,
        slow_fading_db: 0.0,
        noise_level: 0.0,
        crackle_rate_hz: 0.0,
        crackle_level: 0.0,
        sounder_rate_hz: 0.0,
        sounder_level: 0.0,
    };
    /// About as good as HF gets: Watterson's "good" channel with a steady component left in.
    pub const GOOD: Self = Self {
        offset_hz: 40.0,
        drift_hz: 5.0,
        multipath_delay_ms: 0.5,
        fading_rate_hz: 0.1,
        fading_depth: 0.8,
        slow_fading_db: 2.0,
        noise_level: 0.08,
        crackle_rate_hz: 0.5,
        crackle_level: 0.3,
        sounder_rate_hz: 0.03,
        sounder_level: 0.5,
    };
    /// Typical conditions, and the default: Watterson's "moderate" channel.
    pub const FAIR: Self = Self {
        offset_hz: 200.0,
        drift_hz: 25.0,
        multipath_delay_ms: 1.0,
        fading_rate_hz: 0.5,
        fading_depth: 1.0,
        slow_fading_db: 9.0,
        noise_level: 0.2,
        crackle_rate_hz: 2.0,
        crackle_level: 0.6,
        sounder_rate_hz: 0.06,
        sounder_level: 0.6,
    };
    /// Watterson's "poor" channel, badly tuned, with frequent static.
    pub const POOR: Self = Self {
        offset_hz: 500.0,
        drift_hz: 60.0,
        multipath_delay_ms: 2.0,
        fading_rate_hz: 1.0,
        fading_depth: 1.0,
        slow_fading_db: 14.0,
        noise_level: 0.45,
        crackle_rate_hz: 6.0,
        crackle_level: 1.0,
        sounder_rate_hz: 0.1,
        sounder_level: 0.8,
    };
    /// Watterson's "flutter" channel: fast fading as on polar routes.
    pub const FLUTTER: Self = Self {
        offset_hz: 200.0,
        drift_hz: 25.0,
        multipath_delay_ms: 0.5,
        fading_rate_hz: 10.0,
        fading_depth: 1.0,
        slow_fading_db: 9.0,
        noise_level: 0.2,
        crackle_rate_hz: 2.0,
        crackle_level: 0.6,
        sounder_rate_hz: 0.06,
        sounder_level: 0.6,
    };

    /// Non-finite values as 0, so a bad setting cannot leave a channel's state broken.
    fn sanitized(&self) -> Self {
        let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
        Self {
            offset_hz: finite(self.offset_hz),
            drift_hz: finite(self.drift_hz),
            multipath_delay_ms: finite(self.multipath_delay_ms),
            fading_rate_hz: finite(self.fading_rate_hz),
            fading_depth: finite(self.fading_depth),
            slow_fading_db: finite(self.slow_fading_db),
            noise_level: finite(self.noise_level),
            crackle_rate_hz: finite(self.crackle_rate_hz),
            crackle_level: finite(self.crackle_level),
            sounder_rate_hz: finite(self.sounder_rate_hz),
            sounder_level: finite(self.sounder_level),
        }
    }
}

impl Default for HfConditions {
    fn default() -> Self {
        Self::FAIR
    }
}

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

/// Splits a signal into two outputs 90° apart, using two chains of allpass filters (Olli
/// Niemitalo's 8th-order polyphase IIR design). Both outputs keep the input's magnitude.
#[derive(Debug, Clone)]
pub struct Hilbert {
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
    pub fn process(&mut self, x: f32) -> (f32, f32) {
        let i = self.a.iter_mut().fold(x as f64, |s, f| f.process(s));
        let q = self.b.iter_mut().fold(x as f64, |s, f| f.process(s));
        let out = (self.delayed as f32, q as f32);
        self.delayed = i;
        out
    }
}

/// xorshift64*: small, fast and deterministic.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    /// Seeds through SplitMix64, so that similar seeds give unrelated sequences.
    fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        Self(z.max(1))
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in [−1, 1).
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.uniform() + 1.0) / 2.0
    }

    /// Approximately standard normal (sum of three uniforms, variance 1).
    fn gaussian(&mut self) -> f64 {
        self.uniform() + self.uniform() + self.uniform()
    }

    fn range(&mut self, range: RangeInclusive<u32>) -> u32 {
        let (lo, hi) = range.into_inner();
        lo + (self.next() % (hi - lo + 1) as u64) as u32
    }
}

type Complex = (f64, f64);

fn mul(a: Complex, b: Complex) -> Complex {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn lerp(a: Complex, b: Complex, t: f64) -> Complex {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// The complex gain of one propagation path: a steady component plus a scattered one whose
/// spectrum is Gaussian, synthesised as a sum of sinusoids at normally distributed Doppler
/// frequencies.
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
    fn new(rng: &mut Rng) -> Self {
        let mut doppler: [f64; SINUSOIDS] = std::array::from_fn(|_| rng.gaussian());
        // Scale to exactly σ RMS: the fading speed depends on it, and 16 random draws would
        // otherwise leave it up to ±30 % off.
        let rms = (doppler.iter().map(|f| f * f).sum::<f64>() / SINUSOIDS as f64).sqrt();
        doppler.iter_mut().for_each(|f| *f /= rms);
        let phases = std::array::from_fn(|_| rng.unit());
        let angle = std::f64::consts::TAU * rng.unit();
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
    rng: Rng,
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
        let mut rng = Rng::new(seed);
        let offset = rng.uniform();
        let drift = rng.uniform();
        let drift_target = rng.uniform();
        let drift_hold = rng.range(DRIFT_HOLD_FRAMES);
        let paths = [Path::new(&mut rng), Path::new(&mut rng)];
        let slow = rng.gaussian();
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
        Self::new(super::callsign_hash(callsign) ^ CHANNEL_STREAM)
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
            self.drift_target = self.rng.uniform();
            self.drift_hold = self.rng.range(DRIFT_HOLD_FRAMES);
        }
        let k = 1.0 - (-FRAME_SECONDS / DRIFT_TIME_CONSTANT).exp();
        self.drift += (self.drift_target - self.drift) * k;

        let a = (-FRAME_SECONDS / SLOW_FADING_TIME_CONSTANT).exp();
        self.slow = a * self.slow + (1.0 - a * a).sqrt() * self.rng.gaussian();
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

/// Order of each skirt of the sideband filter, like the 6–10 pole crystal filters of SSB sets.
const SSB_ORDER: usize = 8;
/// The sideband filter's passband, in Hz.
const SSB_LOW_HZ: f32 = 300.0;
const SSB_HIGH_HZ: f32 = 2700.0;

/// The receiver's sideband filter, 300–2700 Hz with 8th-order Butterworth skirts. Its steepness
/// makes an impulse ring for several milliseconds, which turns static crashes into blips.
#[derive(Debug, Clone)]
pub struct SsbFilter {
    stages: [Biquad; SSB_ORDER],
}

impl Default for SsbFilter {
    fn default() -> Self {
        let r = SAMPLE_RATE as f32;
        // A Butterworth filter of order n is n/2 biquads with Q = 1 / (2 cos(π(2k + 1) / 2n)).
        let q = |k: usize| {
            let angle = std::f32::consts::PI * (2 * k + 1) as f32 / (2 * SSB_ORDER) as f32;
            1.0 / (2.0 * angle.cos())
        };
        let half = SSB_ORDER / 2;
        Self {
            stages: std::array::from_fn(|i| {
                if i < half {
                    Biquad::high_pass(r, SSB_LOW_HZ, q(i))
                } else {
                    Biquad::low_pass(r, SSB_HIGH_HZ, q(i - half))
                }
            }),
        }
    }
}

impl SsbFilter {
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.stages.iter_mut().fold(*s, |x, f| f.process(x));
        }
    }

    pub fn reset(&mut self) {
        self.stages.iter_mut().for_each(Biquad::reset);
    }
}

/// Simultaneous impulses; the quietest is replaced when more arrive.
const MAX_BURSTS: usize = 8;
/// Peak amplitude of the loudest impulse at level 1, before the receiver's filter.
const CRACKLE_MAX_AMPLITUDE: f32 = 0.2;
/// Impulse sizes, as a fraction of the loudest, follow a Pareto distribution from this smallest
/// size with this shape: mostly near the smallest, with a long tail (capped at 1).
const CRACKLE_SMALLEST: f64 = 0.1;
const CRACKLE_SIZE_SHAPE: f64 = 1.2;
/// Impulses last from this long (the smallest)…
const CRACKLE_SHORTEST_S: f64 = 0.000_03;
/// …to this much longer (the loudest).
const CRACKLE_LENGTH_RANGE_S: f64 = 0.000_3;
/// An impulse louder than this (as a fraction of the loudest) sets off a run of crackling…
const CRACKLE_RUN_THRESHOLD: f32 = 0.3;
/// …during which impulses come this many times as often, for 50–250 ms.
const CRACKLE_RUN_RATE: f64 = 10.0;
const CRACKLE_RUN_SAMPLES: RangeInclusive<u32> = 2_400..=12_000;

#[derive(Debug, Clone, Copy, Default)]
struct Burst {
    amplitude: f32,
    decay: f32,
}

/// Atmospheric static on an HF receiver: impulses from distant lightning, mostly small with the
/// occasional loud crash, and often in runs. Each is a broadband click well under a millisecond
/// long; pass the result through [`SsbFilter`] to hear it as a receiver would. Deterministic for
/// a given seed.
#[derive(Debug, Clone)]
pub struct Crackle {
    rng: Rng,
    /// Samples until the next impulse; `None` until a rate is known.
    next: Option<f64>,
    /// The rate `next` was drawn for.
    rate: f64,
    /// Samples left in the current run of crackling.
    run: u32,
    bursts: [Burst; MAX_BURSTS],
    /// Impulses so far, for tests.
    crashes: u64,
}

impl Crackle {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed ^ CRACKLE_STREAM),
            next: None,
            rate: 0.0,
            run: 0,
            bursts: [Burst::default(); MAX_BURSTS],
            crashes: 0,
        }
    }

    /// Silences any impulse in progress.
    pub fn reset(&mut self) {
        self.bursts = [Burst::default(); MAX_BURSTS];
        self.run = 0;
    }

    /// Samples until the next impulse at the current rate (exponentially distributed).
    fn draw(&mut self) -> f64 {
        let rate = if self.run > 0 { self.rate * CRACKLE_RUN_RATE } else { self.rate };
        -(1.0 - self.rng.unit()).ln() / rate * SAMPLE_RATE as f64
    }

    fn crash(&mut self, level: f32) {
        self.crashes += 1;
        // Heavy-tailed size: mostly about a tenth of the loudest, rarely the loudest itself.
        let u = self.rng.unit().max(f64::MIN_POSITIVE);
        let size = (CRACKLE_SMALLEST * u.powf(-1.0 / CRACKLE_SIZE_SHAPE)).min(1.0) as f32;
        // Larger crashes last a little longer.
        let tau_samples = (CRACKLE_SHORTEST_S + CRACKLE_LENGTH_RANGE_S * size as f64) * SAMPLE_RATE as f64;
        let burst = Burst { amplitude: level * CRACKLE_MAX_AMPLITUDE * size, decay: (-1.0 / tau_samples).exp() as f32 };
        let slot = self.bursts.iter_mut().min_by(|a, b| a.amplitude.total_cmp(&b.amplitude)).expect("MAX_BURSTS > 0");
        *slot = burst;
        if size > CRACKLE_RUN_THRESHOLD {
            self.run = self.run.max(self.rng.range(CRACKLE_RUN_SAMPLES));
        }
    }

    /// Adds one frame of static under `conditions` to `out`.
    pub fn add(&mut self, conditions: &HfConditions, out: &mut Frame) {
        let conditions = conditions.sanitized();
        let (rate, level) = (conditions.crackle_rate_hz.max(0.0) as f64, conditions.crackle_level.clamp(0.0, 1.0));
        if rate == 0.0 || level == 0.0 {
            self.next = None;
            self.reset();
            return;
        }
        if rate != self.rate || self.next.is_none() {
            self.rate = rate;
            self.next = Some(self.draw());
        }
        for o in out.iter_mut() {
            let mut next = self.next.expect("drawn above");
            while next <= 0.0 {
                self.crash(level);
                next += self.draw();
            }
            self.next = Some(next - 1.0);
            self.run = self.run.saturating_sub(1);

            let mut envelope = 0.0;
            for b in &mut self.bursts {
                envelope += b.amplitude;
                b.amplitude *= b.decay;
            }
            *o += self.rng.gaussian() as f32 * envelope;
        }
    }
}

/// The unfaded voice level ahead of the sideband filter on an HF radio: afv-native's HF voice
/// gain of 0.2 applied to a stream at the −18 dBFS AGC target. Background levels are relative to it.
fn voice_rms() -> f32 {
    0.2 * 0.125_9
}

/// Share of white noise's power inside the sideband: its width over the whole band.
const SSB_SHARE: f32 = (SSB_HIGH_HZ - SSB_LOW_HZ) / (SAMPLE_RATE as f32 / 2.0);

/// The radio's EQ and compressor treat flat noise and speech differently: the Schmid preset's
/// peaks at 1.45 and 2 kHz lift noise more than voice, and the compressor gives a quiet noise floor
/// more make-up gain. Measured at the output of the default radio so that `noise_level` 1, 0.45,
/// 0.2 and 0.08 give 0, 7, 14 and 22 dB speech-to-noise as heard (within 0.5 dB). The Rockwell and
/// Garex presets come out 4–6 dB cleaner.
const HISS_CALIBRATION: f32 = 0.21;

/// The atmospheric noise floor: white noise standing [`HfConditions::noise_level`] relative to the
/// voice as heard on the default radio (other hardware presets shift the balance a few dB).
#[derive(Debug, Clone)]
pub struct Hiss {
    rng: Rng,
}

impl Hiss {
    pub fn new(seed: u64) -> Self {
        Self { rng: Rng::new(seed ^ HISS_STREAM) }
    }

    /// Adds one frame of hiss under `conditions` to `out`.
    pub fn add(&mut self, conditions: &HfConditions, out: &mut Frame) {
        let level = conditions.sanitized().noise_level.max(0.0);
        if level == 0.0 {
            return;
        }
        let sigma = HISS_CALIBRATION * level * voice_rms() / SSB_SHARE.sqrt();
        for o in out.iter_mut() {
            *o += sigma * self.rng.gaussian() as f32;
        }
    }
}

/// Sounders in flight at once.
const MAX_CHIRPS: usize = 4;
/// Sweep rates of chirp sounders in Hz per second; most sweep at 100 kHz/s.
const SOUNDER_SWEEP_RATES: [f64; 5] = [50_000.0, 100_000.0, 100_000.0, 100_000.0, 125_000.0];
/// The audio range a sweep crosses; the sideband filter cuts it to the passband.
const SOUNDER_SPAN_HZ: f64 = 3500.0;
/// Share of sounders sweeping upwards; in the upper sideband they rise in pitch.
const SOUNDER_UPWARD: f64 = 0.8;
/// Sounder strengths spread evenly (in dB) over this range below the strongest.
const SOUNDER_STRENGTH_SPREAD_DB: f32 = 20.0;
/// Share of sounders heard with an echo over a second path…
const SOUNDER_ECHO_CHANCE: f64 = 0.5;
/// …arriving this much later…
const SOUNDER_ECHO_DELAY_S: RangeInclusive<f64> = 0.001..=0.004;
/// …at this fraction of the sounder's strength.
const SOUNDER_ECHO_STRENGTH: RangeInclusive<f32> = 0.3..=1.0;
/// Strongest sounder at level 1 relative to the voice level: 9 dB above it (6 dB plus the 3 dB
/// between a sine's peak and its RMS).
const SOUNDER_MAX_RELATIVE: f32 = 2.0 * std::f32::consts::SQRT_2;

/// Peak amplitude of the strongest sounder at level 1.
fn sounder_max_amplitude() -> f32 {
    SOUNDER_MAX_RELATIVE * voice_rms()
}

#[derive(Debug, Clone, Copy, Default)]
struct Chirp {
    /// Samples until it starts (an echo starts late); negative once running.
    wait: i64,
    /// Current audio frequency and its change per sample, in Hz.
    frequency: f64,
    sweep: f64,
    /// Phase in turns.
    phase: f64,
    amplitude: f32,
    active: bool,
}

/// Ionospheric chirp sounders. Each sweeps the whole HF band at about 100 kHz per second, so it
/// crosses a receiver's 2.4 kHz passband in a few tens of milliseconds: a short tone sliding up
/// (or, for the few that sweep downwards, down). Some arrive with an echo over a second path.
/// Deterministic for a given seed.
#[derive(Debug, Clone)]
pub struct Sounders {
    rng: Rng,
    next: Option<f64>,
    rate: f64,
    chirps: [Chirp; MAX_CHIRPS],
    /// Sounders and echoes so far, for tests.
    passes: u64,
    echoes: u64,
}

impl Sounders {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed ^ SOUNDER_STREAM),
            next: None,
            rate: 0.0,
            chirps: [Chirp::default(); MAX_CHIRPS],
            passes: 0,
            echoes: 0,
        }
    }

    pub fn reset(&mut self) {
        self.chirps = [Chirp::default(); MAX_CHIRPS];
    }

    fn draw(&mut self) -> f64 {
        -(1.0 - self.rng.unit()).ln() / self.rate * SAMPLE_RATE as f64
    }

    fn start(&mut self, chirp: Chirp) {
        if let Some(slot) = self.chirps.iter_mut().find(|c| !c.active) {
            *slot = chirp;
        }
    }

    fn pass(&mut self, level: f32) {
        self.passes += 1;
        let rate = SOUNDER_SWEEP_RATES[self.rng.range(0..=SOUNDER_SWEEP_RATES.len() as u32 - 1) as usize];
        let up = self.rng.unit() < SOUNDER_UPWARD;
        let sweep = if up { rate } else { -rate } / SAMPLE_RATE as f64;
        let amplitude =
            level * sounder_max_amplitude() * 10f32.powf(-SOUNDER_STRENGTH_SPREAD_DB / 20.0 * self.rng.unit() as f32);
        let chirp = Chirp {
            wait: 0,
            frequency: if up { 0.0 } else { SOUNDER_SPAN_HZ },
            sweep,
            phase: 0.0,
            amplitude,
            active: true,
        };
        self.start(chirp);
        if self.rng.unit() < SOUNDER_ECHO_CHANCE {
            let (lo, hi) = SOUNDER_ECHO_DELAY_S.into_inner();
            let delay = (lo + (hi - lo) * self.rng.unit()) * SAMPLE_RATE as f64;
            let (lo, hi) = SOUNDER_ECHO_STRENGTH.into_inner();
            let strength = lo + (hi - lo) * self.rng.unit() as f32;
            self.start(Chirp { wait: delay as i64, amplitude: amplitude * strength, ..chirp });
            self.echoes += 1;
        }
    }

    /// Adds one frame of sounder blips under `conditions` to `out`.
    pub fn add(&mut self, conditions: &HfConditions, out: &mut Frame) {
        let c = conditions.sanitized();
        let (rate, level) = (c.sounder_rate_hz.max(0.0) as f64, c.sounder_level.clamp(0.0, 1.0));
        if rate == 0.0 || level == 0.0 {
            self.next = None;
            self.reset();
            return;
        }
        if rate != self.rate || self.next.is_none() {
            self.rate = rate;
            self.next = Some(self.draw());
        }
        for o in out.iter_mut() {
            let mut next = self.next.expect("drawn above");
            while next <= 0.0 {
                self.pass(level);
                next += self.draw();
            }
            self.next = Some(next - 1.0);
            for chirp in self.chirps.iter_mut().filter(|c| c.active) {
                if chirp.wait > 0 {
                    chirp.wait -= 1;
                    continue;
                }
                *o += chirp.amplitude * (std::f64::consts::TAU * chirp.phase).sin() as f32;
                chirp.phase = (chirp.phase + chirp.frequency / SAMPLE_RATE as f64).rem_euclid(1.0);
                chirp.frequency += chirp.sweep;
                chirp.active = (0.0..=SOUNDER_SPAN_HZ).contains(&chirp.frequency);
            }
        }
    }
}

/// Everything an HF receiver hears besides the signal: the noise floor, static and sounders.
/// Add it ahead of [`SsbFilter`].
#[derive(Debug, Clone)]
pub struct Atmospherics {
    hiss: Hiss,
    crackle: Crackle,
    sounders: Sounders,
}

impl Atmospherics {
    pub fn new(seed: u64) -> Self {
        Self { hiss: Hiss::new(seed), crackle: Crackle::new(seed), sounders: Sounders::new(seed) }
    }

    pub fn reset(&mut self) {
        self.crackle.reset();
        self.sounders.reset();
    }

    /// Adds one frame of background under `conditions` to `out`.
    pub fn add(&mut self, conditions: &HfConditions, out: &mut Frame) {
        self.hiss.add(conditions, out);
        self.crackle.add(conditions, out);
        self.sounders.add(conditions, out);
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
    fn ssb_filter_passes_voice_and_rings_on_impulses() {
        let gain = |freq: f64| {
            let mut f = SsbFilter::default();
            let mut x = sine(freq, 0.5, 24_000);
            f.process(&mut x);
            tone(&x[12_000..], freq) / 0.5
        };
        assert!((gain(1000.0) - 1.0).abs() < 0.02);
        // 8th order: (300/100)^8 and (6000/2700)^8 below the passband, i.e. −76 and −55 dB.
        assert!(gain(100.0) < 0.001 && gain(6000.0) < 0.004, "{} {}", gain(100.0), gain(6000.0));
        // An impulse comes out as a burst lasting a few milliseconds.
        let mut f = SsbFilter::default();
        let mut x = vec![0.0; 2400];
        x[0] = 1.0;
        f.process(&mut x);
        let energy = |r: std::ops::Range<usize>| x[r].iter().map(|s| s * s).sum::<f32>();
        let total = energy(0..2400);
        assert!(energy(96..2400) > 0.03 * total, "rings past 2 ms");
        assert!(energy(480..2400) < 0.002 * total, "gone by 10 ms");
    }

    fn crackle(rate: f32, level: f32) -> HfConditions {
        HfConditions { crackle_rate_hz: rate, crackle_level: level, ..HfConditions::OFF }
    }

    /// `seconds` of static from a fresh generator.
    fn static_for(c: &HfConditions, seconds: usize, seed: u64) -> (Vec<f32>, Crackle) {
        let mut statics = Crackle::new(seed);
        let mut frame = [0.0; FRAME_SAMPLES];
        let out = (0..seconds * 50)
            .flat_map(|_| {
                frame.fill(0.0);
                statics.add(c, &mut frame);
                frame
            })
            .collect();
        (out, statics)
    }

    #[test]
    fn no_static_without_rate_or_level() {
        for c in [crackle(0.0, 1.0), crackle(5.0, 0.0), HfConditions::OFF] {
            let (out, statics) = static_for(&c, 5, 1);
            assert!(out.iter().all(|&s| s == 0.0));
            assert_eq!(statics.crashes, 0);
        }
    }

    #[test]
    fn crashes_follow_the_rate_with_runs_on_top() {
        let count = |rate| static_for(&crackle(rate, 1.0), 120, 3).1.crashes as f64;
        let (slow, fast) = (count(1.0), count(4.0));
        // At least the base rate (allowing for chance), at most a few times it with the runs.
        assert!((100.0..=480.0).contains(&slow), "{slow} crashes in 120 s at 1/s");
        assert!(fast > 3.0 * slow, "{fast} at 4/s vs {slow} at 1/s");
    }

    #[test]
    fn crashes_come_in_runs() {
        // Without runs, crashes at 1/s would follow each other within 40 ms about 4 % of the time.
        let mut statics = Crackle::new(10);
        let mut frame = [0.0; FRAME_SAMPLES];
        let c = crackle(1.0, 1.0);
        let mut last = None;
        let mut gaps = Vec::new();
        for n in 0..50 * 600 {
            let before = statics.crashes;
            statics.add(&c, &mut frame);
            for _ in before..statics.crashes {
                if let Some(last) = last {
                    gaps.push(n - last);
                }
                last = Some(n);
            }
        }
        let close = gaps.iter().filter(|&&g| g <= 2).count() as f64 / gaps.len() as f64;
        assert!(close > 0.15, "{:.0} % of crashes within 40 ms of the previous", close * 100.0);
    }

    #[test]
    fn static_is_short_impulses() {
        let (out, _) = static_for(&crackle(2.0, 1.0), 60, 4);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        // Every burst at full size at once, with the noise beyond 7σ: about 1 in 10⁵ over a minute.
        assert!(peak <= MAX_BURSTS as f32 * 7.0 * CRACKLE_MAX_AMPLITUDE, "peak {peak}");
        // Silent nearly all the time: each impulse lasts well under a millisecond.
        let active = out.iter().filter(|s| s.abs() > 0.001).count() as f64 / out.len() as f64;
        assert!(active < 0.02, "{:.1} % of samples active", active * 100.0);
    }

    #[test]
    fn level_scales_the_static() {
        let (full, _) = static_for(&crackle(3.0, 1.0), 10, 6);
        let (half, _) = static_for(&crackle(3.0, 0.5), 10, 6);
        assert!(full.iter().zip(&half).all(|(f, h)| (0.5 * f - h).abs() < 1e-6));
    }

    #[test]
    fn rate_changes_apply_at_once() {
        let mut statics = Crackle::new(8);
        let mut frame = [0.0; FRAME_SAMPLES];
        // A very slow rate first, so the first crash would be far away if never redrawn.
        for _ in 0..50 {
            statics.add(&crackle(0.001, 1.0), &mut frame);
        }
        let before = statics.crashes;
        for _ in 0..50 {
            statics.add(&crackle(20.0, 1.0), &mut frame);
        }
        assert!(statics.crashes - before >= 5, "{} crashes in the first second at 20/s", statics.crashes - before);
        // Turning it off silences the very next frame, bursts in progress included.
        frame.fill(0.0);
        statics.add(&crackle(20.0, 0.0), &mut frame);
        assert!(frame.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn reset_cuts_the_static() {
        let mut statics = Crackle::new(9);
        let mut frame = [0.0; FRAME_SAMPLES];
        let c = crackle(50.0, 1.0);
        for _ in 0..10 {
            statics.add(&c, &mut frame);
        }
        statics.reset();
        assert!(statics.bursts.iter().all(|b| b.amplitude == 0.0));
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

    #[test]
    fn hiss_sits_at_its_level_relative_to_the_voice() {
        for level in [0.25f32, 1.0] {
            let c = HfConditions { noise_level: level, ..HfConditions::OFF };
            let (mut hiss, mut ssb) = (Hiss::new(1), SsbFilter::default());
            let mut frame = [0.0; FRAME_SAMPLES];
            let out: Vec<f32> = (0..250)
                .flat_map(|_| {
                    frame.fill(0.0);
                    hiss.add(&c, &mut frame);
                    ssb.process(&mut frame);
                    frame
                })
                .collect();
            let rms = (out[4800..].iter().map(|s| s * s).sum::<f32>() / (out.len() - 4800) as f32).sqrt();
            let expected = HISS_CALIBRATION * level * voice_rms();
            assert!((rms / expected - 1.0).abs() < 0.1, "level {level}: {rms}, expected {expected}");
        }
        let mut frame = [0.0; FRAME_SAMPLES];
        Hiss::new(1).add(&HfConditions::OFF, &mut frame);
        assert!(frame.iter().all(|&s| s == 0.0));
    }

    fn sounders(rate: f32, level: f32) -> HfConditions {
        HfConditions { sounder_rate_hz: rate, sounder_level: level, ..HfConditions::OFF }
    }

    fn run_sounders(s: &mut Sounders, c: &HfConditions, frames: usize) -> Vec<f32> {
        let mut frame = [0.0; FRAME_SAMPLES];
        (0..frames)
            .flat_map(|_| {
                frame.fill(0.0);
                s.add(c, &mut frame);
                frame
            })
            .collect()
    }

    #[test]
    fn sounders_pass_at_their_rate_half_with_echoes() {
        let mut s = Sounders::new(1);
        run_sounders(&mut s, &sounders(0.5, 1.0), 50 * 600);
        assert!((240..=360).contains(&s.passes), "{} passes in 600 s at 0.5/s", s.passes);
        let echoes = s.echoes as f64 / s.passes as f64;
        assert!((0.4..0.6).contains(&echoes), "{:.0} % with an echo", echoes * 100.0);
        for c in [sounders(0.0, 1.0), sounders(1.0, 0.0)] {
            let mut s = Sounders::new(2);
            assert!(run_sounders(&mut s, &c, 500).iter().all(|&x| x == 0.0));
            assert_eq!(s.passes, 0);
        }
    }

    #[test]
    fn a_sounder_is_a_short_sweep() {
        // One pass at a time: start it by hand, with the rate so low that no other follows.
        for seed in 0..20 {
            let mut s = Sounders::new(seed);
            let c = sounders(1e-9, 1.0);
            run_sounders(&mut s, &c, 1);
            s.pass(1.0);
            let out = run_sounders(&mut s, &c, 10);
            let active: Vec<usize> = (0..out.len()).filter(|&i| out[i] != 0.0).collect();
            let (first, last) = (active[0], *active.last().unwrap());
            let ms = (last - first) as f64 / 48.0;
            // 3.5 kHz at 50–125 kHz/s is 28–70 ms, plus up to 4 ms of echo.
            assert!((27.0..75.0).contains(&ms), "seed {seed}: {ms:.1} ms");
            // The pitch moves one way: zero crossings per half differ clearly.
            let crossings =
                |r: std::ops::Range<usize>| out[r].windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f64;
            let mid = (first + last) / 2;
            let (a, b) = (crossings(first..mid), crossings(mid..last));
            assert!((a - b).abs() > 0.3 * (a + b) / 2.0, "seed {seed}: {a} vs {b} crossings");
        }
    }

    #[test]
    fn sounder_level_scales_them() {
        let (mut a, mut b) = (Sounders::new(3), Sounders::new(3));
        let full = run_sounders(&mut a, &sounders(2.0, 1.0), 500);
        let half = run_sounders(&mut b, &sounders(2.0, 0.5), 500);
        assert!(full.iter().any(|&x| x != 0.0));
        assert!(full.iter().zip(&half).all(|(f, h)| (0.5 * f - h).abs() < 1e-6));
        assert!(full.iter().all(|x| x.abs() <= 2.0 * sounder_max_amplitude()), "an echo can overlap its sounder");
    }
}
