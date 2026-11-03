//! Second-order IIR sections ("biquads") and cascades of them: the RBJ "Audio EQ Cookbook"
//! designs, and Butterworth and Chebyshev band-passes built from them.
//!
//! # References
//!
//! - R. Bristow-Johnson, "Cookbook formulae for audio equalizer biquad filter coefficients",
//!   published as the W3C [Audio EQ Cookbook](https://www.w3.org/TR/audio-eq-cookbook/): the
//!   low-pass, high-pass, peaking and shelving sections.
//! - A. B. Williams and F. J. Taylor, *Electronic Filter Design Handbook*, McGraw-Hill: the pole
//!   positions of the Butterworth and Chebyshev (type I) low-pass prototypes, and the low-pass to
//!   high-pass transformation.

use std::f64::consts::PI;

use crate::SAMPLE_RATE;

/// One second-order section, `H(z) = (b0 + b1·z⁻¹ + b2·z⁻²) / (1 + a1·z⁻¹ + a2·z⁻²)`, in transposed
/// direct form II.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    state: [f64; 2],
}

impl Default for Biquad {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The cookbook's `cos ω0` and `α` for a corner or centre frequency and Q (Bristow-Johnson).
fn cookbook(freq: f64, q: f64) -> (f64, f64) {
    let w0 = 2.0 * PI * freq / SAMPLE_RATE as f64;
    (w0.cos(), w0.sin() / (2.0 * q))
}

impl Biquad {
    /// Passes the signal unchanged.
    pub const IDENTITY: Self = Self { b: [1.0, 0.0, 0.0], a: [0.0, 0.0], state: [0.0; 2] };

    /// From numerator coefficients `b` and denominator coefficients `a`, normalised by `a[0]`.
    pub fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self { b: b.map(|b| b / a[0]), a: [a[1] / a[0], a[2] / a[0]], state: [0.0; 2] }
    }

    pub fn low_pass(freq: f64, q: f64) -> Self {
        let (cos, alpha) = cookbook(freq, q);
        Self::new([(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0], [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    }

    pub fn high_pass(freq: f64, q: f64) -> Self {
        let (cos, alpha) = cookbook(freq, q);
        Self::new([(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0], [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    }

    pub fn peaking(freq: f64, q: f64, db: f64) -> Self {
        let (cos, alpha) = cookbook(freq, q);
        let a = 10f64.powf(db / 40.0);
        Self::new([1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a])
    }

    pub fn low_shelf(freq: f64, q: f64, db: f64) -> Self {
        let (cos, alpha) = cookbook(freq, q);
        let a = 10f64.powf(db / 40.0);
        let s = 2.0 * a.sqrt() * alpha;
        Self::new(
            [
                a * ((a + 1.0) - (a - 1.0) * cos + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - s),
            ],
            [(a + 1.0) + (a - 1.0) * cos + s, -2.0 * ((a - 1.0) + (a + 1.0) * cos), (a + 1.0) + (a - 1.0) * cos - s],
        )
    }

    pub fn high_shelf(freq: f64, q: f64, db: f64) -> Self {
        let (cos, alpha) = cookbook(freq, q);
        let a = 10f64.powf(db / 40.0);
        let s = 2.0 * a.sqrt() * alpha;
        Self::new(
            [
                a * ((a + 1.0) + (a - 1.0) * cos + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - s),
            ],
            [(a + 1.0) - (a - 1.0) * cos + s, 2.0 * ((a - 1.0) - (a + 1.0) * cos), (a + 1.0) - (a - 1.0) * cos - s],
        )
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b[0] * x + self.state[0];
        self.state = [self.b[1] * x - self.a[0] * y + self.state[1], self.b[2] * x - self.a[1] * y];
        y as f32
    }

    pub fn reset(&mut self) {
        self.state = [0.0; 2];
    }

    /// The gain at `freq` Hz.
    pub fn response(&self, freq: f64) -> f64 {
        // H(e^jω) with z⁻¹ = e^−jω.
        let w = 2.0 * PI * freq / SAMPLE_RATE as f64;
        let eval = |c: [f64; 3]| {
            let re = c[0] + c[1] * w.cos() + c[2] * (2.0 * w).cos();
            let im = -c[1] * w.sin() - c[2] * (2.0 * w).sin();
            re.hypot(im)
        };
        eval(self.b) / eval([1.0, self.a[0], self.a[1]])
    }
}

/// The second-order sections of an all-pole low-pass prototype of even `order` with its passband
/// edge at 1, each as (natural frequency, Q): Butterworth for `ripple_db` 0, otherwise Chebyshev
/// (type I) with that much passband ripple.
fn prototype(order: usize, ripple_db: f64) -> impl Iterator<Item = (f64, f64)> {
    // The Chebyshev poles lie on an ellipse with these semi-axes; Butterworth's on the unit circle.
    let (sinh, cosh) = if ripple_db > 0.0 {
        let epsilon = (10f64.powf(ripple_db / 10.0) - 1.0).sqrt();
        let v = (1.0 / epsilon).asinh() / order as f64;
        (v.sinh(), v.cosh())
    } else {
        (1.0, 1.0)
    };
    (0..order / 2).map(move |k| {
        let theta = PI * (2 * k + 1) as f64 / (2 * order) as f64;
        let (re, im) = (sinh * theta.sin(), cosh * theta.cos());
        let w = re.hypot(im);
        (w, w / (2.0 * re))
    })
}

/// The sections of a high-pass of even `order` with its edge at `freq` Hz: Butterworth for
/// `ripple_db` 0 (−3 dB at the edge), otherwise Chebyshev with that much passband ripple, rippling
/// between 0 dB and +`ripple_db` and ending at the edge.
pub fn high_pass(order: usize, freq: f64, ripple_db: f64) -> impl Iterator<Item = Biquad> {
    assert!(order.is_multiple_of(2), "even order");
    // A high-pass has the low-pass prototype's poles inverted: same Q, reciprocal frequency.
    prototype(order, ripple_db).map(move |(w, q)| Biquad::high_pass(freq / w, q))
}

/// The sections of a low-pass of even `order` with its edge at `freq` Hz; see [`high_pass`].
pub fn low_pass(order: usize, freq: f64, ripple_db: f64) -> impl Iterator<Item = Biquad> {
    assert!(order.is_multiple_of(2), "even order");
    prototype(order, ripple_db).map(move |(w, q)| Biquad::low_pass(freq * w, q))
}

/// `N` biquads in series.
#[derive(Debug, Clone, PartialEq)]
pub struct Cascade<const N: usize>([Biquad; N]);

impl<const N: usize> Default for Cascade<N> {
    fn default() -> Self {
        Self([Biquad::IDENTITY; N])
    }
}

impl<const N: usize> Cascade<N> {
    /// These sections, the rest passing the signal unchanged. Panics if there are more than `N`.
    pub fn new(sections: impl IntoIterator<Item = Biquad>) -> Self {
        let mut cascade = Self::default();
        let mut slots = cascade.0.iter_mut();
        for s in sections {
            *slots.next().expect("no more sections than N") = s;
        }
        cascade
    }

    /// A `low`–`high` Hz band-pass whose skirts are each of order `N` (`N` even): Butterworth for
    /// `ripple_db` 0, where the edges are at −3 dB, otherwise Chebyshev with that much passband
    /// ripple, the edges being where the ripple band ends. Each Chebyshev skirt ripples between
    /// 0 dB and +`ripple_db`.
    pub fn band_pass(low: f64, high: f64, ripple_db: f64) -> Self {
        Self::new(high_pass(N, low, ripple_db).chain(low_pass(N, high, ripple_db)))
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.0.iter_mut().fold(*s, |x, f| f.process(x));
        }
    }

    pub fn reset(&mut self) {
        self.0.iter_mut().for_each(Biquad::reset);
    }

    /// The gain at `freq` Hz.
    pub fn response(&self, freq: f64) -> f64 {
        self.0.iter().map(|f| f.response(freq)).product()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{db, sine, tone};

    /// Steady-state amplitude of a unit sine at `freq` after the filter.
    fn gain_at(mut f: Biquad, freq: f64) -> f32 {
        let x: Vec<f32> = sine(freq, 1.0, 48_000).into_iter().map(|x| f.process(x)).collect();
        tone(&x[24_000..], freq)
    }

    #[test]
    fn low_and_high_pass() {
        assert!((gain_at(Biquad::low_pass(1000.0, 0.707), 100.0) - 1.0).abs() < 0.01);
        assert!(gain_at(Biquad::low_pass(1000.0, 0.707), 10_000.0) < 0.02);
        assert!((gain_at(Biquad::high_pass(1000.0, 0.707), 10_000.0) - 1.0).abs() < 0.01);
        assert!(gain_at(Biquad::high_pass(1000.0, 0.707), 100.0) < 0.02);
    }

    #[test]
    fn peaking_boosts_and_cuts_at_centre_only() {
        assert!((db(gain_at(Biquad::peaking(1000.0, 1.0, 12.0), 1000.0)) - 12.0).abs() < 0.1);
        assert!((db(gain_at(Biquad::peaking(1000.0, 1.0, -12.0), 1000.0)) + 12.0).abs() < 0.1);
        assert!(db(gain_at(Biquad::peaking(1000.0, 1.0, 12.0), 50.0)).abs() < 0.2);
    }

    #[test]
    fn butterworth_cutoff_is_minus_3_db() {
        let q = std::f64::consts::FRAC_1_SQRT_2;
        assert!((db(gain_at(Biquad::low_pass(1000.0, q), 1000.0)) + 3.01).abs() < 0.05);
        assert!((db(gain_at(Biquad::high_pass(1000.0, q), 1000.0)) + 3.01).abs() < 0.05);
    }

    #[test]
    fn shelves_change_one_side_only() {
        let q = std::f64::consts::FRAC_1_SQRT_2;
        let high = || Biquad::high_shelf(1000.0, q, 6.0);
        assert!((db(gain_at(high(), 15_000.0)) - 6.0).abs() < 0.2);
        assert!(db(gain_at(high(), 50.0)).abs() < 0.2);
        // Half the shelf gain at the centre frequency.
        assert!((db(gain_at(high(), 1000.0)) - 3.0).abs() < 0.1);

        let low = || Biquad::low_shelf(1000.0, q, 6.0);
        assert!((db(gain_at(low(), 50.0)) - 6.0).abs() < 0.2);
        assert!(db(gain_at(low(), 15_000.0)).abs() < 0.2);
        assert!((db(gain_at(low(), 1000.0)) - 3.0).abs() < 0.1);
    }

    #[test]
    fn coefficients_are_normalised_by_a0() {
        let input: Vec<f32> = (0..64).map(|i| (i as f32 * 0.3).sin()).collect();
        let mut identity = Biquad::new([2.0, 0.0, 0.0], [2.0, 0.0, 0.0]);
        assert!(input.iter().all(|&x| identity.process(x) == x));
        // y[n] = 0.5·x[n−1] + 0.25·y[n−1], given scaled by a0 = 4.
        let mut f = Biquad::new([0.0, 2.0, 0.0], [4.0, -1.0, 0.0]);
        let (mut x1, mut y1) = (0f32, 0f32);
        for &x in &input {
            let expected = 0.5 * x1 + 0.25 * y1;
            assert!((f.process(x) - expected).abs() < 1e-6);
            (x1, y1) = (x, expected);
        }
    }

    #[test]
    fn response_matches_the_measured_gain() {
        for f in
            [Biquad::peaking(1500.0, 1.0, 20.0), Biquad::high_pass(300.0, 0.25), Biquad::low_shelf(2000.0, 1.0, 3.0)]
        {
            for freq in [100.0, 1000.0, 1500.0, 5000.0] {
                assert!((gain_at(f, freq) as f64 / f.response(freq) - 1.0).abs() < 0.01, "{f:?} at {freq} Hz");
            }
        }
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut f = Biquad::peaking(2000.0, 1.0, 20.0);
        let first: Vec<f32> = input.iter().map(|&x| f.process(x)).collect();
        f.reset();
        let second: Vec<f32> = input.iter().map(|&x| f.process(x)).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn butterworth_band_pass() {
        let bp = Cascade::<4>::band_pass(300.0, 3000.0, 0.0);
        let db_at = |f: f64| 20.0 * bp.response(f).log10();
        assert!((db_at(300.0) + 3.01).abs() < 0.1);
        assert!((db_at(3000.0) + 3.01).abs() < 0.1);
        assert!(db_at(1000.0).abs() < 0.1);
        // Fourth order: 24 dB per octave.
        assert!((db_at(75.0) + 48.0).abs() < 1.0);
    }

    #[test]
    fn chebyshev_band_pass_ripples_then_falls_steeply() {
        let ripple = 0.5;
        let bp = Cascade::<8>::band_pass(300.0, 2700.0, ripple);
        let db_at = |f: f64| 20.0 * bp.response(f).log10();
        // Each skirt ripples between 0 and +ripple from edge to edge; below 0 just outside.
        for k in 0..=100 {
            let f = 300.0 * (9f64).powf(k as f64 / 100.0);
            assert!((-0.15..=2.0 * ripple + 0.1).contains(&db_at(f)), "{f:.0} Hz: {:.2} dB", db_at(f));
        }
        assert!(db_at(290.0) < 0.0 && db_at(2800.0) < 0.0);
        // Far steeper than a Butterworth of the same order, whose 2.7 kHz skirt reaches −60 dB at
        // 6.4 kHz.
        assert!(db_at(4500.0) < -60.0, "{:.1} dB at 4.5 kHz", db_at(4500.0));
        assert!(db_at(180.0) < -60.0, "{:.1} dB at 180 Hz", db_at(180.0));
    }

    #[test]
    fn cascade_runs_its_sections_in_series() {
        let sections = [Biquad::peaking(1000.0, 1.0, 6.0), Biquad::low_pass(3000.0, 0.7)];
        let mut cascade = Cascade::<4>::new(sections);
        let input = sine(1200.0, 0.3, 960);
        let mut out = input.clone();
        cascade.process(&mut out);
        let mut by_hand = sections;
        for (&x, &o) in input.iter().zip(&out) {
            assert_eq!(by_hand.iter_mut().fold(x, |s, f| f.process(s)), o);
        }
        cascade.reset();
        let mut again = input.clone();
        cascade.process(&mut again);
        assert_eq!(out, again);
    }
}
