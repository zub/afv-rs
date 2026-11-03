//! Port of ChunkWare Audio's "Simple Source" compressor and limiter (MIT licence,
//! (c) 2006 ChunkWare Music Software), as used by afv-native's hardware filter.
//!
//! Only the parts afv-native uses are ported; both process a stereo pair because the original
//! links its two inputs.

const DC_OFFSET: f64 = 1.0e-25;
/// What remains of a step after one time constant of the limiter's fast envelope.
const FAST_RESIDUE: f64 = 0.01;

fn lin_to_db(lin: f64) -> f64 {
    lin.ln() * 8.685_889_638_065_037
}

fn db_to_lin(db: f64) -> f64 {
    (db * 0.115_129_254_649_702_28).exp()
}

/// One-pole smoother: `state = input + coef * (state - input)`.
#[derive(Debug, Clone, Copy)]
struct Envelope {
    coef: f64,
}

impl Envelope {
    /// The standard coefficient: `exp(-1000 / (ms * rate))`.
    fn exponential(ms: f64, sample_rate: f64) -> Self {
        Self { coef: (-1000.0 / (ms * sample_rate)).exp() }
    }

    /// The limiter's faster coefficient: `0.01 ^ (1000 / (ms * rate))`, which covers all but
    /// 1 % (−40 dB) of a step in `ms`.
    fn fast(ms: f64, sample_rate: f64) -> Self {
        Self { coef: FAST_RESIDUE.powf(1000.0 / (ms * sample_rate)) }
    }

    fn run(&self, input: f64, state: &mut f64) {
        *state = input + self.coef * (*state - input);
    }
}

/// Peak compressor. A ratio above 1 expands instead of compressing.
#[derive(Debug, Clone)]
pub struct SimpleComp {
    attack: Envelope,
    release: Envelope,
    thresh_db: f64,
    ratio: f64,
    env_db: f64,
}

impl SimpleComp {
    pub fn new(sample_rate: f64, attack_ms: f64, release_ms: f64, thresh_db: f64, ratio: f64) -> Self {
        Self {
            attack: Envelope::exponential(attack_ms, sample_rate),
            release: Envelope::exponential(release_ms, sample_rate),
            thresh_db,
            ratio,
            env_db: DC_OFFSET,
        }
    }

    pub fn reset(&mut self) {
        self.env_db = DC_OFFSET;
    }

    pub fn process(&mut self, in1: &mut f64, in2: &mut f64) {
        let key = in1.abs().max(in2.abs()) + DC_OFFSET;
        let over_db = (lin_to_db(key) - self.thresh_db).max(0.0) + DC_OFFSET;
        if over_db > self.env_db {
            self.attack.run(over_db, &mut self.env_db);
        } else {
            self.release.run(over_db, &mut self.env_db);
        }
        let gain = db_to_lin((self.env_db - DC_OFFSET) * (self.ratio - 1.0));
        *in1 *= gain;
        *in2 *= gain;
    }
}

const LIMIT_BUFFER: usize = 1024;

/// Look-ahead peak limiter.
#[derive(Debug, Clone)]
pub struct SimpleLimit {
    thresh: f64,
    peak_hold: usize,
    peak_timer: usize,
    max_peak: f64,
    attack: Envelope,
    release: Envelope,
    env: f64,
    cursor: usize,
    buffer: Box<[[f64; 2]; LIMIT_BUFFER]>,
}

impl SimpleLimit {
    /// `peak_hold` is the look-ahead in samples. The original derives it from the attack time at
    /// the sample rate in effect when the attack was set, so callers pass it explicitly.
    pub fn new(sample_rate: f64, attack_ms: f64, release_ms: f64, thresh_db: f64, peak_hold: usize) -> Self {
        assert!(peak_hold < LIMIT_BUFFER);
        let thresh = db_to_lin(thresh_db);
        Self {
            thresh,
            peak_hold,
            peak_timer: 0,
            max_peak: thresh,
            attack: Envelope::fast(attack_ms, sample_rate),
            release: Envelope::fast(release_ms, sample_rate),
            env: thresh,
            cursor: 0,
            buffer: Box::new([[0.0; 2]; LIMIT_BUFFER]),
        }
    }

    pub fn reset(&mut self) {
        self.peak_timer = 0;
        self.max_peak = self.thresh;
        self.env = self.thresh;
        self.cursor = 0;
        self.buffer.fill([0.0; 2]);
    }

    pub fn process(&mut self, in1: &mut f64, in2: &mut f64) {
        let key = in1.abs().max(in2.abs()).max(self.thresh);
        self.peak_timer += 1;
        if self.peak_timer >= self.peak_hold || key > self.max_peak {
            self.peak_timer = 0;
            self.max_peak = key;
        }
        if self.max_peak > self.env {
            self.attack.run(self.max_peak, &mut self.env);
        } else {
            self.release.run(self.max_peak, &mut self.env);
        }
        let gain = self.thresh / self.env;
        let delayed = self.buffer[self.cursor.wrapping_sub(self.peak_hold) & (LIMIT_BUFFER - 1)];
        self.buffer[self.cursor] = [*in1, *in2];
        self.cursor = (self.cursor + 1) & (LIMIT_BUFFER - 1);
        *in1 = delayed[0] * gain;
        *in2 = delayed[1] * gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    /// Runs a mono signal through `f` (both inputs fed the same) and returns the outputs.
    fn run(input: impl IntoIterator<Item = f64>, mut f: impl FnMut(&mut f64, &mut f64)) -> Vec<f64> {
        input
            .into_iter()
            .map(|x| {
                let (mut l, mut r) = (x, x);
                f(&mut l, &mut r);
                assert_eq!(l, r);
                l
            })
            .collect()
    }

    fn gain_db(out: f64, input: f64) -> f64 {
        20.0 * (out / input).abs().log10()
    }

    #[test]
    fn db_conversions_are_inverse() {
        for db in [-60.0, -6.0, 0.0, 8.0] {
            assert!((lin_to_db(db_to_lin(db)) - db).abs() < 1e-12);
        }
        assert!((db_to_lin(-20.0) - 0.1).abs() < 1e-15);
    }

    #[test]
    fn comp_is_transparent_below_threshold() {
        let mut comp = SimpleComp::new(RATE, 1.0, 10.0, -20.0, 0.25);
        let out = run(std::iter::repeat_n(0.05, 4800), |l, r| comp.process(l, r));
        assert!(out.iter().all(|&o| (o - 0.05).abs() < 1e-15));
    }

    #[test]
    fn comp_steady_state_follows_the_ratio() {
        // 0 dBFS, 20 dB over a −20 dB threshold: (ratio − 1)·20 dB of gain.
        for (ratio, expected_db) in [(0.5, -10.0), (0.25, -15.0), (6.0, 100.0)] {
            let mut comp = SimpleComp::new(RATE, 1.0, 10.0, -20.0, ratio);
            let out = run(std::iter::repeat_n(1.0, 48_000), |l, r| comp.process(l, r));
            let g = gain_db(*out.last().unwrap(), 1.0);
            assert!((g - expected_db).abs() < 1e-6, "ratio {ratio}: {g} dB");
        }
        // The hardware filter's expander: −2 dBFS is 6 dB over −8 dB, ratio 6 gives +30 dB.
        let mut comp = SimpleComp::new(RATE, 0.1, 80.0, -8.0, 6.0);
        let x = db_to_lin(-2.0);
        let out = run(std::iter::repeat_n(x, 48_000), |l, r| comp.process(l, r));
        assert!((gain_db(*out.last().unwrap(), x) - 30.0).abs() < 1e-6);
    }

    #[test]
    fn comp_attack_and_release_are_one_pole_time_constants() {
        // Ratio 0 makes the gain in dB exactly the negated envelope.
        let (attack_ms, release_ms) = (10.0, 100.0);
        let mut comp = SimpleComp::new(RATE, attack_ms, release_ms, -20.0, 0.0);
        let attack_n = (attack_ms * RATE / 1000.0) as usize;
        let out = run(std::iter::repeat_n(1.0, attack_n), |l, r| comp.process(l, r));
        let expected = -20.0 * (1.0 - (-1f64).exp());
        assert!((gain_db(out[attack_n - 1], 1.0) - expected).abs() < 1e-9, "after one attack time constant");

        run(std::iter::repeat_n(1.0, 48_000), |l, r| comp.process(l, r));
        // Release from 20 dB with the key below threshold.
        let release_n = (release_ms * RATE / 1000.0) as usize;
        let quiet = 0.01;
        let out = run(std::iter::repeat_n(quiet, release_n), |l, r| comp.process(l, r));
        let expected = -20.0 * (-1f64).exp();
        assert!((gain_db(out[release_n - 1], quiet) - expected).abs() < 1e-6, "after one release time constant");
    }

    #[test]
    fn comp_links_both_channels_to_the_louder() {
        let mut comp = SimpleComp::new(RATE, 1.0, 10.0, -20.0, 0.5);
        let (mut l, mut r) = (0.0, 0.0);
        for _ in 0..48_000 {
            (l, r) = (1.0, 0.01);
            comp.process(&mut l, &mut r);
        }
        assert!((l - r / 0.01).abs() < 1e-12);
        assert!((gain_db(r, 0.01) + 10.0).abs() < 1e-6);
    }

    #[test]
    fn comp_reset_forgets_the_envelope() {
        let mut comp = SimpleComp::new(RATE, 1.0, 1000.0, -20.0, 0.5);
        run(std::iter::repeat_n(1.0, 4800), |l, r| comp.process(l, r));
        comp.reset();
        let out = run([0.05], |l, r| comp.process(l, r));
        assert!((out[0] - 0.05).abs() < 1e-15);
    }

    #[test]
    fn limit_below_threshold_is_an_exact_delay() {
        let hold = 16;
        let mut lim = SimpleLimit::new(RATE, 1.0, 10.0, 0.0, hold);
        let input: Vec<f64> = (0..1000).map(|i| 0.9 * (i as f64 * 0.05).sin()).collect();
        let out = run(input.iter().copied(), |l, r| lim.process(l, r));
        assert!(out[..hold].iter().all(|&o| o == 0.0));
        assert!(out[hold..].iter().zip(&input).all(|(o, i)| o == i));
    }

    #[test]
    fn limit_attack_follows_the_fast_envelope() {
        // Step to 2× threshold. The envelope rises as 2 − c^n with c = 0.01^(1000 / (ms·rate)),
        // i.e. 99 % of the way in one attack time; output is the delayed input over the envelope.
        let (hold, attack_ms) = (16, 1.0);
        let mut lim = SimpleLimit::new(RATE, attack_ms, 10.0, 0.0, hold);
        let out = run(std::iter::repeat_n(2.0, 2000), |l, r| lim.process(l, r));
        let c = 0.01f64.powf(1000.0 / (attack_ms * RATE));
        for (k, &o) in out.iter().enumerate().skip(hold) {
            let expected = 2.0 / (2.0 - c.powi(k as i32 + 1));
            assert!((o - expected).abs() < 1e-12, "sample {k}: {o} vs {expected}");
        }
        assert!(out[47] < 1.006, "within 1 % after one attack time");
        assert!((out[1999] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn limit_look_ahead_catches_a_spike() {
        // With the attack shorter than the look-ahead, the envelope has reached a single-sample
        // spike by the time it leaves the delay line. The peak hold expires on that very sample,
        // so the envelope has taken one release step: 1 + c_release·(4 − 1).
        let (hold, release_ms) = (16, 10.0);
        let mut lim = SimpleLimit::new(RATE, 0.1, release_ms, 0.0, hold);
        let mut input = vec![0.5; 100];
        input[50] = 4.0;
        let out = run(input, |l, r| lim.process(l, r));
        let c_release = 0.01f64.powf(1000.0 / (release_ms * RATE));
        let expected = 4.0 / (1.0 + c_release * 3.0);
        assert!((out[50 + hold] - expected).abs() < 1e-6, "{} vs {expected}", out[50 + hold]);
        assert!(expected < 1.01);
        assert!(out[50 + hold - 1] < 0.5, "samples before the spike are attenuated too");
    }

    #[test]
    fn limit_releases_back_to_unity() {
        let (hold, release_ms) = (16, 10.0);
        let mut lim = SimpleLimit::new(RATE, 0.1, release_ms, 0.0, hold);
        run(std::iter::repeat_n(2.0, 4800), |l, r| lim.process(l, r));
        let out = run(std::iter::repeat_n(0.5, 2000), |l, r| lim.process(l, r));
        let release_n = (release_ms * RATE / 1000.0) as usize;
        // Still clearly limiting just after the change, within 1 % after one release time plus
        // the hold and delay.
        assert!(out[2 * hold + 20] < 0.4);
        assert!(out[2 * hold + release_n] > 0.5 / 1.01);
        assert!((out[1999] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn limit_reset_clears_the_delay_line() {
        let mut lim = SimpleLimit::new(RATE, 1.0, 10.0, 0.0, 16);
        run(std::iter::repeat_n(2.0, 100), |l, r| lim.process(l, r));
        lim.reset();
        let out = run(std::iter::repeat_n(0.5, 17), |l, r| lim.process(l, r));
        assert!(out[..16].iter().all(|&o| o == 0.0));
        assert_eq!(out[16], 0.5);
    }
}
