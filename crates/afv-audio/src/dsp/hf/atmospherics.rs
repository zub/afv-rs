//! What an HF receiver hears besides the signal: the noise floor, static and sounders.
//!
//! The strengths are set by ear, except the noise floor, whose presets sit within ITU-R F.339's
//! grades (see the [`hf`](super) module).
//!
//! # References
//!
//! - Motorola, [US patent 4,466,129](https://patents.google.com/patent/US4466129), "Noise
//!   reducing circuitry for single sideband receivers": an SSB receiver's filters stretch a
//!   sub-microsecond static impulse into a burst of several milliseconds.
//! - G3PLX, ["Chirp sounders"](https://jcoppens.com/radio/prop/g3plx/chirps.en.php), and
//!   Wikipedia, ["Ionosonde"](https://en.wikipedia.org/wiki/Ionosonde): most sounders sweep
//!   upwards at 100 kHz per second.

use std::ops::RangeInclusive;

use rand::RngExt;
use rand::rngs::SmallRng;
use rand_distr::{Exp1, Pareto, StandardNormal};

use super::HfConditions;
use crate::dsp::{voice_rms, with_rng};
use crate::{Frame, SAMPLE_RATE};

/// Random events at a rate that may change at any time (a Poisson process): each is due when the
/// rate, summed over time, reaches an exponentially distributed draw. A change of rate therefore
/// applies at once.
#[derive(Debug, Clone)]
struct Arrivals {
    /// Expected events until the next one.
    until_next: f64,
}

impl Arrivals {
    fn new(rng: &mut SmallRng) -> Self {
        Self { until_next: rng.sample(Exp1) }
    }

    /// Moves on by one sample at `rate` events per second; returns the number of events due.
    fn tick(&mut self, rng: &mut SmallRng, rate: f64) -> u32 {
        self.until_next -= rate / SAMPLE_RATE as f64;
        let mut due = 0;
        while self.until_next <= 0.0 {
            due += 1;
            self.until_next += rng.sample::<f64, _>(Exp1);
        }
        due
    }
}

/// Simultaneous impulses; the quietest is replaced when more arrive.
const MAX_BURSTS: usize = 8;
/// Peak amplitude of the loudest impulse at level 1 relative to the voice level, before the
/// receiver's filter (which spreads it into a far quieter blip).
const CRACKLE_MAX_RELATIVE: f32 = 8.0;
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

/// Atmospheric static: impulses from distant lightning, mostly small with the occasional loud
/// crash, and often in runs. Each is a broadband click well under a millisecond long; the
/// receiver's sideband filter turns it into a blip (Motorola US 4,466,129).
#[derive(Debug, Clone)]
struct Crackle {
    arrivals: Arrivals,
    /// Samples left in the current run of crackling.
    run: u32,
    bursts: [Burst; MAX_BURSTS],
    /// Impulses so far, for tests.
    crashes: u64,
}

impl Crackle {
    fn new(rng: &mut SmallRng) -> Self {
        Self { arrivals: Arrivals::new(rng), run: 0, bursts: [Burst::default(); MAX_BURSTS], crashes: 0 }
    }

    /// Silences any impulse in progress.
    fn reset(&mut self) {
        self.bursts = [Burst::default(); MAX_BURSTS];
        self.run = 0;
    }

    fn crash(&mut self, rng: &mut SmallRng, level: f32) {
        self.crashes += 1;
        // Heavy-tailed size: mostly about a tenth of the loudest, rarely the loudest itself.
        let sizes = Pareto::new(CRACKLE_SMALLEST, CRACKLE_SIZE_SHAPE).expect("valid parameters");
        let size = rng.sample(sizes).min(1.0) as f32;
        // Larger crashes last a little longer.
        let tau_samples = (CRACKLE_SHORTEST_S + CRACKLE_LENGTH_RANGE_S * size as f64) * SAMPLE_RATE as f64;
        let amplitude = level * CRACKLE_MAX_RELATIVE * voice_rms() * size;
        let burst = Burst { amplitude, decay: (-1.0 / tau_samples).exp() as f32 };
        let slot = self.bursts.iter_mut().min_by(|a, b| a.amplitude.total_cmp(&b.amplitude)).expect("MAX_BURSTS > 0");
        *slot = burst;
        if size > CRACKLE_RUN_THRESHOLD {
            self.run = self.run.max(rng.random_range(CRACKLE_RUN_SAMPLES));
        }
    }

    /// Adds one frame of static under `conditions` to `out`.
    fn add(&mut self, rng: &mut SmallRng, conditions: &HfConditions, out: &mut Frame) {
        let (rate, level) = (conditions.crackle_rate_hz.max(0.0) as f64, conditions.crackle_level.clamp(0.0, 1.0));
        if rate == 0.0 || level == 0.0 {
            self.reset();
            return;
        }
        for o in out.iter_mut() {
            let rate = if self.run > 0 { rate * CRACKLE_RUN_RATE } else { rate };
            for _ in 0..self.arrivals.tick(rng, rate) {
                self.crash(rng, level);
            }
            self.run = self.run.saturating_sub(1);

            let mut envelope = 0.0;
            for b in &mut self.bursts {
                envelope += b.amplitude;
                b.amplitude *= b.decay;
            }
            *o += rng.sample::<f32, _>(StandardNormal) * envelope;
        }
    }
}

/// The voice channel [`HfConditions::noise_level`] is measured in, in Hz. A set with a narrower
/// filter hears less of the noise, as a real one does.
const CHANNEL_LOW_HZ: f64 = 300.0;
const CHANNEL_HIGH_HZ: f64 = 3000.0;
/// Share of white noise's power inside that channel: its width over the whole band.
const CHANNEL_SHARE: f64 = (CHANNEL_HIGH_HZ - CHANNEL_LOW_HZ) / (SAMPLE_RATE as f64 / 2.0);

/// Adds one frame of the atmospheric noise floor under `conditions` to `out`: white noise,
/// [`HfConditions::noise_level`] relative to the voice within a 300–3000 Hz channel.
fn add_hiss(rng: &mut SmallRng, conditions: &HfConditions, out: &mut Frame) {
    let level = conditions.noise_level.max(0.0);
    if level == 0.0 {
        return;
    }
    let sigma = level * voice_rms() / CHANNEL_SHARE.sqrt() as f32;
    for o in out.iter_mut() {
        *o += sigma * rng.sample::<f32, _>(StandardNormal);
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

/// Ionospheric chirp sounders. Each sweeps the whole HF band, most at 100 kHz per second (G3PLX),
/// so it crosses a receiver's 2.2 kHz passband in a few tens of milliseconds: a short tone sliding
/// up (or, for the few that sweep downwards, down). Some arrive with an echo over a second path.
#[derive(Debug, Clone)]
struct Sounders {
    arrivals: Arrivals,
    chirps: [Chirp; MAX_CHIRPS],
    /// Sounders and echoes so far, for tests.
    passes: u64,
    echoes: u64,
}

impl Sounders {
    fn new(rng: &mut SmallRng) -> Self {
        Self { arrivals: Arrivals::new(rng), chirps: [Chirp::default(); MAX_CHIRPS], passes: 0, echoes: 0 }
    }

    fn reset(&mut self) {
        self.chirps = [Chirp::default(); MAX_CHIRPS];
    }

    fn start(&mut self, chirp: Chirp) {
        if let Some(slot) = self.chirps.iter_mut().find(|c| !c.active) {
            *slot = chirp;
        }
    }

    fn pass(&mut self, rng: &mut SmallRng, level: f32) {
        self.passes += 1;
        let rate = SOUNDER_SWEEP_RATES[rng.random_range(0..SOUNDER_SWEEP_RATES.len())];
        let up = rng.random_bool(SOUNDER_UPWARD);
        let sweep = if up { rate } else { -rate } / SAMPLE_RATE as f64;
        let amplitude =
            level * sounder_max_amplitude() * 10f32.powf(-SOUNDER_STRENGTH_SPREAD_DB / 20.0 * rng.random::<f32>());
        let chirp = Chirp {
            wait: 0,
            frequency: if up { 0.0 } else { SOUNDER_SPAN_HZ },
            sweep,
            phase: 0.0,
            amplitude,
            active: true,
        };
        self.start(chirp);
        if rng.random_bool(SOUNDER_ECHO_CHANCE) {
            let delay = rng.random_range(SOUNDER_ECHO_DELAY_S) * SAMPLE_RATE as f64;
            let strength = rng.random_range(SOUNDER_ECHO_STRENGTH);
            self.start(Chirp { wait: delay as i64, amplitude: amplitude * strength, ..chirp });
            self.echoes += 1;
        }
    }

    /// Adds one frame of sounder blips under `conditions` to `out`.
    fn add(&mut self, rng: &mut SmallRng, conditions: &HfConditions, out: &mut Frame) {
        let (rate, level) = (conditions.sounder_rate_hz.max(0.0) as f64, conditions.sounder_level.clamp(0.0, 1.0));
        if rate == 0.0 || level == 0.0 {
            self.reset();
            return;
        }
        for o in out.iter_mut() {
            for _ in 0..self.arrivals.tick(rng, rate) {
                self.pass(rng, level);
            }
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
/// Add it ahead of the sideband filter.
#[derive(Debug, Clone)]
pub(crate) struct Atmospherics {
    crackle: Crackle,
    sounders: Sounders,
}

impl Atmospherics {
    pub(crate) fn new() -> Self {
        with_rng(|rng| Self { crackle: Crackle::new(rng), sounders: Sounders::new(rng) })
    }

    pub(crate) fn reset(&mut self) {
        self.crackle.reset();
        self.sounders.reset();
    }

    /// Adds one frame of background under `conditions` to `out`.
    pub(crate) fn add(&mut self, conditions: &HfConditions, out: &mut Frame) {
        let c = conditions.sanitized();
        with_rng(|rng| {
            add_hiss(rng, &c, out);
            self.crackle.add(rng, &c, out);
            self.sounders.add(rng, &c, out);
        });
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;
    use crate::FRAME_SAMPLES;
    use crate::test_util::rms;

    fn seeded(seed: u64) -> SmallRng {
        SmallRng::seed_from_u64(seed)
    }

    #[test]
    fn arrivals_follow_the_rate_and_change_with_it() {
        let mut rng = seeded(1);
        let mut a = Arrivals::new(&mut rng);
        let count = |a: &mut Arrivals, rng: &mut SmallRng, rate, seconds: u32| {
            (0..seconds * SAMPLE_RATE).map(|_| a.tick(rng, rate)).sum::<u32>()
        };
        let n = count(&mut a, &mut rng, 10.0, 100);
        assert!((900..1100).contains(&n), "{n} in 100 s at 10/s");
        assert_eq!(count(&mut a, &mut rng, 0.0, 10), 0);
        let n = count(&mut a, &mut rng, 1000.0, 1);
        assert!((900..1100).contains(&n), "{n} in 1 s at 1000/s");
    }

    fn crackle(rate: f32, level: f32) -> HfConditions {
        HfConditions { crackle_rate_hz: rate, crackle_level: level, ..HfConditions::OFF }
    }

    /// `seconds` of static from a fresh generator.
    fn static_for(c: &HfConditions, seconds: usize, seed: u64) -> (Vec<f32>, Crackle) {
        let mut rng = seeded(seed);
        let mut statics = Crackle::new(&mut rng);
        let mut frame = [0.0; FRAME_SAMPLES];
        let out = (0..seconds * 50)
            .flat_map(|_| {
                frame.fill(0.0);
                statics.add(&mut rng, c, &mut frame);
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
        let mut rng = seeded(10);
        let mut statics = Crackle::new(&mut rng);
        let mut frame = [0.0; FRAME_SAMPLES];
        let c = crackle(1.0, 1.0);
        let mut last = None;
        let mut gaps = Vec::new();
        for n in 0..50 * 600 {
            let before = statics.crashes;
            statics.add(&mut rng, &c, &mut frame);
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
        let loudest = CRACKLE_MAX_RELATIVE * voice_rms();
        // Every burst at full size at once, with the noise beyond 7σ: about 1 in 10⁵ over a minute.
        assert!(peak <= MAX_BURSTS as f32 * 7.0 * loudest, "peak {peak}");
        // Silent nearly all the time: each impulse lasts well under a millisecond.
        let active = out.iter().filter(|s| s.abs() > 0.005 * loudest).count() as f64 / out.len() as f64;
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
        let mut rng = seeded(8);
        let mut statics = Crackle::new(&mut rng);
        let mut frame = [0.0; FRAME_SAMPLES];
        // A very slow rate first, so the first crash would be far away if never redrawn.
        for _ in 0..50 {
            statics.add(&mut rng, &crackle(0.001, 1.0), &mut frame);
        }
        let before = statics.crashes;
        for _ in 0..50 {
            statics.add(&mut rng, &crackle(20.0, 1.0), &mut frame);
        }
        assert!(statics.crashes - before >= 5, "{} crashes in the first second at 20/s", statics.crashes - before);
        // Turning it off silences the very next frame, bursts in progress included.
        frame.fill(0.0);
        statics.add(&mut rng, &crackle(20.0, 0.0), &mut frame);
        assert!(frame.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn reset_cuts_the_static() {
        let mut rng = seeded(9);
        let mut statics = Crackle::new(&mut rng);
        let mut frame = [0.0; FRAME_SAMPLES];
        let c = crackle(50.0, 1.0);
        for _ in 0..10 {
            statics.add(&mut rng, &c, &mut frame);
        }
        statics.reset();
        assert!(statics.bursts.iter().all(|b| b.amplitude == 0.0));
    }

    #[test]
    fn hiss_sits_at_its_level_relative_to_the_voice() {
        for level in [0.1f32, 1.0] {
            let c = HfConditions { noise_level: level, ..HfConditions::OFF };
            let (mut rng, mut channel) =
                (seeded(1), crate::dsp::Cascade::<8>::band_pass(CHANNEL_LOW_HZ, CHANNEL_HIGH_HZ, 0.0));
            let mut frame = [0.0; FRAME_SAMPLES];
            let out: Vec<f32> = (0..250)
                .flat_map(|_| {
                    frame.fill(0.0);
                    add_hiss(&mut rng, &c, &mut frame);
                    channel.process(&mut frame);
                    frame
                })
                .collect();
            let expected = level * voice_rms();
            assert!(
                (rms(&out[4800..]) / expected - 1.0).abs() < 0.1,
                "level {level}: {}, expected {expected}",
                rms(&out[4800..])
            );
        }
        let mut frame = [0.0; FRAME_SAMPLES];
        add_hiss(&mut seeded(1), &HfConditions::OFF, &mut frame);
        assert!(frame.iter().all(|&s| s == 0.0));
    }

    fn sounders(rate: f32, level: f32) -> HfConditions {
        HfConditions { sounder_rate_hz: rate, sounder_level: level, ..HfConditions::OFF }
    }

    fn run_sounders(s: &mut Sounders, rng: &mut SmallRng, c: &HfConditions, frames: usize) -> Vec<f32> {
        let mut frame = [0.0; FRAME_SAMPLES];
        (0..frames)
            .flat_map(|_| {
                frame.fill(0.0);
                s.add(rng, c, &mut frame);
                frame
            })
            .collect()
    }

    #[test]
    fn sounders_pass_at_their_rate_half_with_echoes() {
        let mut rng = seeded(1);
        let mut s = Sounders::new(&mut rng);
        run_sounders(&mut s, &mut rng, &sounders(0.5, 1.0), 50 * 600);
        assert!((240..=360).contains(&s.passes), "{} passes in 600 s at 0.5/s", s.passes);
        let echoes = s.echoes as f64 / s.passes as f64;
        assert!((0.4..0.6).contains(&echoes), "{:.0} % with an echo", echoes * 100.0);
        for c in [sounders(0.0, 1.0), sounders(1.0, 0.0)] {
            let mut rng = seeded(2);
            let mut s = Sounders::new(&mut rng);
            assert!(run_sounders(&mut s, &mut rng, &c, 500).iter().all(|&x| x == 0.0));
            assert_eq!(s.passes, 0);
        }
    }

    #[test]
    fn a_sounder_is_a_short_sweep() {
        // One pass at a time: start it by hand, with the rate so low that no other follows.
        for seed in 0..20 {
            let mut rng = seeded(seed);
            let mut s = Sounders::new(&mut rng);
            let c = sounders(1e-9, 1.0);
            s.pass(&mut rng, 1.0);
            let out = run_sounders(&mut s, &mut rng, &c, 10);
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
        let (mut rng_a, mut rng_b) = (seeded(3), seeded(3));
        let (mut a, mut b) = (Sounders::new(&mut rng_a), Sounders::new(&mut rng_b));
        let full = run_sounders(&mut a, &mut rng_a, &sounders(2.0, 1.0), 500);
        let half = run_sounders(&mut b, &mut rng_b, &sounders(2.0, 0.5), 500);
        assert!(full.iter().any(|&x| x != 0.0));
        assert!(full.iter().zip(&half).all(|(f, h)| (0.5 * f - h).abs() < 1e-6));
        assert!(full.iter().all(|x| x.abs() <= 2.0 * sounder_max_amplitude()), "an echo can overlap its sounder");
    }

    #[test]
    fn bad_settings_do_not_break_the_background() {
        crate::dsp::seed_rng(1);
        let mut a = Atmospherics::new();
        let mut frame = [0.0; FRAME_SAMPLES];
        let bad = HfConditions {
            noise_level: f32::NAN,
            crackle_rate_hz: f32::INFINITY,
            sounder_rate_hz: f32::NAN,
            ..HfConditions::POOR
        };
        a.add(&bad, &mut frame);
        assert!(frame.iter().all(|s| s.is_finite()));
        for _ in 0..50 {
            a.add(&HfConditions::POOR, &mut frame);
        }
        assert!(frame.iter().all(|s| s.is_finite()) && frame.iter().any(|&s| s != 0.0));
    }
}
