//! A look-ahead peak limiter that keeps a signal within a ceiling without clipping it.

use crate::SAMPLE_RATE;

/// Look-ahead and attack, in samples (1 ms); the output is delayed by one less.
const WINDOW: usize = 48;
/// Time constant of the gain recovering after a peak, in seconds.
const RELEASE_S: f64 = 0.05;

/// Turns the gain down smoothly ahead of any peak above the ceiling and lets it recover over
/// [`RELEASE_S`]. Below the ceiling the signal passes untouched, only delayed.
///
/// The gain for each sample is the lower of the gain it needs and the recovering previous gain;
/// its minimum over the window, averaged over the window again, ramps down in time for the peak
/// to leave the delay line and never exceeds the gain any sample in the window needs.
#[derive(Debug, Clone)]
pub(crate) struct PeakLimiter {
    ceiling: f32,
    delay: [f32; WINDOW],
    needed: [f32; WINDOW],
    held: [f32; WINDOW],
    held_sum: f64,
    gain: f32,
    cursor: usize,
}

impl PeakLimiter {
    pub(crate) fn new(ceiling: f32) -> Self {
        Self {
            ceiling,
            delay: [0.0; WINDOW],
            needed: [1.0; WINDOW],
            held: [1.0; WINDOW],
            held_sum: WINDOW as f64,
            gain: 1.0,
            cursor: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.ceiling);
    }

    pub(crate) fn process(&mut self, samples: &mut [f32]) {
        let release = (1.0 - (-1.0 / (RELEASE_S * SAMPLE_RATE as f64)).exp()) as f32;
        for s in samples {
            let needed = if s.abs() > self.ceiling { self.ceiling / s.abs() } else { 1.0 };
            self.gain = needed.min(self.gain + (1.0 - self.gain) * release);
            self.needed[self.cursor] = self.gain;
            let held = self.needed.iter().fold(1f32, |m, &g| m.min(g));
            self.held_sum += held as f64 - self.held[self.cursor] as f64;
            self.held[self.cursor] = held;
            self.delay[self.cursor] = *s;
            self.cursor = (self.cursor + 1) % WINDOW;
            // The oldest sample, WINDOW − 1 behind the newest.
            *s = self.delay[self.cursor] * (self.held_sum / WINDOW as f64).min(1.0) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{peak, sine};

    #[test]
    fn below_the_ceiling_it_is_an_exact_delay() {
        let input = sine(1000.0, 0.69, 4800);
        let mut out = input.clone();
        PeakLimiter::new(0.7).process(&mut out);
        assert!(out[..WINDOW - 1].iter().all(|&s| s == 0.0));
        assert_eq!(&out[WINDOW - 1..], &input[..input.len() - (WINDOW - 1)]);
    }

    #[test]
    fn nothing_gets_past_the_ceiling() {
        let mut x = sine(700.0, 0.3, 24_000);
        x[5000] = 40.0;
        x[5001] = -40.0;
        for (i, s) in x[12_000..14_000].iter_mut().enumerate() {
            *s += 3.0 * (i as f32 * 0.2).sin();
        }
        PeakLimiter::new(0.7).process(&mut x);
        assert!(peak(&x) <= 0.7 * 1.000_01, "peak {}", peak(&x));
    }

    #[test]
    fn the_gain_changes_smoothly_and_recovers() {
        // A loud burst in a steady tone: no gain step from one sample to the next is large.
        let mut x = sine(1000.0, 0.5, 24_000);
        let input = x.clone();
        for s in &mut x[10_000..10_480] {
            *s *= 4.0;
        }
        let burst = x.clone();
        PeakLimiter::new(0.7).process(&mut x);
        let gain = |i: usize| x[i] / burst[i - (WINDOW - 1)];
        let audible = |i: usize| burst[i - (WINDOW - 1)].abs() > 0.1;
        let worst = (WINDOW..x.len() - 1)
            .filter(|&i| audible(i) && audible(i + 1))
            .map(|i| (gain(i + 1) - gain(i)).abs())
            .fold(0f32, f32::max);
        assert!(worst < 0.02, "gain steps by up to {worst} per sample");
        // Recovered to unity half a second after the burst.
        let late = 10_480 + 24_000 / 2;
        let i = (late..x.len()).find(|&i| input[i - (WINDOW - 1)].abs() > 0.4).unwrap();
        assert!((x[i] / input[i - (WINDOW - 1)] - 1.0).abs() < 0.01);
    }
}
