//! RBJ "Audio EQ Cookbook" biquad, ported from afv-native's `BiQuadFilter`.
//!
//! Coefficients are `f64`; the sample path and filter state are `f32`, as in the original.

use std::f64::consts::PI;

#[derive(Debug, Clone, Default)]
pub struct Biquad {
    a0: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    a4: f64,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Builds a filter from raw coefficients (`aa*` feedback, `b*` feedforward), normalised by `aa0`.
    pub fn from_coefficients(aa0: f64, aa1: f64, aa2: f64, b0: f64, b1: f64, b2: f64) -> Self {
        Self { a0: b0 / aa0, a1: b1 / aa0, a2: b2 / aa0, a3: aa1 / aa0, a4: aa2 / aa0, ..Self::default() }
    }

    pub fn low_pass(sample_rate: f32, cutoff: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * cutoff as f64 / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        Self::from_coefficients(1.0 + alpha, -2.0 * cos, 1.0 - alpha, (1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0)
    }

    pub fn high_pass(sample_rate: f32, cutoff: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * cutoff as f64 / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        Self::from_coefficients(
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
        )
    }

    pub fn peaking_eq(sample_rate: f32, centre: f32, q: f32, db_gain: f32) -> Self {
        let w0 = 2.0 * PI * centre as f64 / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        // `dbGain / 40` is evaluated in float in the original.
        let a = 10f64.powf((db_gain / 40.0) as f64);
        Self::from_coefficients(
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
        )
    }

    pub fn low_shelf(sample_rate: f32, centre: f32, q: f32, db_gain: f32) -> Self {
        let w0 = 2.0 * PI * centre as f64 / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        // `dbGain / 40` is evaluated in float in the original.
        let a = 10f64.powf((db_gain / 40.0) as f64);
        let s = 2.0 * a.sqrt() * alpha;
        Self::from_coefficients(
            (a + 1.0) + (a - 1.0) * cos + s,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - s,
            a * ((a + 1.0) - (a - 1.0) * cos + s),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - s),
        )
    }

    pub fn high_shelf(sample_rate: f32, centre: f32, q: f32, db_gain: f32) -> Self {
        let w0 = 2.0 * PI * centre as f64 / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        // `dbGain / 40` is evaluated in float in the original.
        let a = 10f64.powf((db_gain / 40.0) as f64);
        let s = 2.0 * a.sqrt() * alpha;
        Self::from_coefficients(
            (a + 1.0) - (a - 1.0) * cos + s,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - s,
            a * ((a + 1.0) + (a - 1.0) * cos + s),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - s),
        )
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.a0 * x as f64 + self.a1 * self.x1 as f64 + self.a2 * self.x2 as f64
            - self.a3 * self.y1 as f64
            - self.a4 * self.y2 as f64;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y as f32;
        self.y1
    }

    pub fn reset(&mut self) {
        (self.x1, self.x2, self.y1, self.y2) = (0.0, 0.0, 0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state amplitude of a unit sine at `freq` after the filter.
    fn gain_at(mut f: Biquad, freq: f32) -> f32 {
        let mut peak = 0f32;
        for i in 0..48_000 {
            let y = f.process((2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin());
            if i > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn low_and_high_pass() {
        assert!((gain_at(Biquad::low_pass(48_000.0, 1000.0, 0.707), 100.0) - 1.0).abs() < 0.01);
        assert!(gain_at(Biquad::low_pass(48_000.0, 1000.0, 0.707), 10_000.0) < 0.02);
        assert!((gain_at(Biquad::high_pass(48_000.0, 1000.0, 0.707), 10_000.0) - 1.0).abs() < 0.01);
        assert!(gain_at(Biquad::high_pass(48_000.0, 1000.0, 0.707), 100.0) < 0.02);
    }

    fn db(g: f32) -> f32 {
        20.0 * g.log10()
    }

    #[test]
    fn peaking_boosts_and_cuts_at_centre_only() {
        let g = gain_at(Biquad::peaking_eq(48_000.0, 1000.0, 1.0, 12.0), 1000.0);
        assert!((db(g) - 12.0).abs() < 0.1, "{g}");
        let g = gain_at(Biquad::peaking_eq(48_000.0, 1000.0, 1.0, -12.0), 1000.0);
        assert!((db(g) + 12.0).abs() < 0.1, "{g}");
        let g = gain_at(Biquad::peaking_eq(48_000.0, 1000.0, 1.0, 12.0), 50.0);
        assert!(db(g).abs() < 0.2, "{g}");
    }

    #[test]
    fn butterworth_cutoff_is_minus_3_db() {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        assert!((db(gain_at(Biquad::low_pass(48_000.0, 1000.0, q), 1000.0)) + 3.01).abs() < 0.05);
        assert!((db(gain_at(Biquad::high_pass(48_000.0, 1000.0, q), 1000.0)) + 3.01).abs() < 0.05);
    }

    #[test]
    fn shelves_change_one_side_only() {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let high = || Biquad::high_shelf(48_000.0, 1000.0, q, 6.0);
        assert!((db(gain_at(high(), 15_000.0)) - 6.0).abs() < 0.2);
        assert!(db(gain_at(high(), 50.0)).abs() < 0.2);
        // Half the shelf gain at the centre frequency.
        assert!((db(gain_at(high(), 1000.0)) - 3.0).abs() < 0.1);

        let low = || Biquad::low_shelf(48_000.0, 1000.0, q, 6.0);
        assert!((db(gain_at(low(), 50.0)) - 6.0).abs() < 0.2);
        assert!(db(gain_at(low(), 15_000.0)).abs() < 0.2);
        assert!((db(gain_at(low(), 1000.0)) - 3.0).abs() < 0.1);
    }

    #[test]
    fn raw_coefficients_are_normalised_by_aa0() {
        let input: Vec<f32> = (0..64).map(|i| (i as f32 * 0.3).sin()).collect();
        let mut identity = Biquad::from_coefficients(2.0, 0.0, 0.0, 2.0, 0.0, 0.0);
        assert!(input.iter().all(|&x| identity.process(x) == x));
        // y[n] = 0.5·x[n−1] + 0.25·y[n−1], given scaled by aa0 = 4.
        let mut f = Biquad::from_coefficients(4.0, -1.0, 0.0, 0.0, 2.0, 0.0);
        let (mut x1, mut y1) = (0f32, 0f32);
        for &x in &input {
            let expected = 0.5 * x1 + 0.25 * y1;
            assert!((f.process(x) - expected).abs() < 1e-6);
            (x1, y1) = (x, expected);
        }
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut f = Biquad::peaking_eq(48_000.0, 2000.0, 1.0, 20.0);
        let first: Vec<f32> = input.iter().map(|&x| f.process(x)).collect();
        f.reset();
        let second: Vec<f32> = input.iter().map(|&x| f.process(x)).collect();
        assert_eq!(first, second);
    }
}
