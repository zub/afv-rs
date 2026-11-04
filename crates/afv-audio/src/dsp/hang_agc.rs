//! The automatic gain control of an SSB receiver.
//!
//! Each set supplies its own timing ([`HangTiming`]); see the HFS-900D's in
//! [`hardware`](super::hardware), which follows the Marconi 915 airborne HF transceiver's data
//! sheet (at most 12 ms attack and 1.5 s recovery).

use super::{db_to_gain, pole, voice_rms};
use crate::SAMPLE_RATE;

/// The gain is set this far ahead of the audio (2 ms), so that it can start down before a crash.
const LOOKAHEAD: usize = (SAMPLE_RATE * 2 / 1000) as usize;
/// Peaks of an unfaded voice, relative to its RMS level.
const VOICE_CREST: f32 = 4.0;

/// How a set's hang AGC behaves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HangTiming {
    /// Time constant of the gain falling to a louder signal, in seconds…
    pub attack_s: f32,
    /// …how long it then holds, so that it stays put between words…
    pub hang_s: f32,
    /// …and how fast it rises again after that, in dB per second.
    pub decay_db_per_s: f32,
    /// The level the gain holds peaks at.
    pub target_peak: f32,
    /// How far the gain can rise above what an unfaded voice needs, in dB: it makes up for fades
    /// this deep, and between transmissions the hiss comes up this much.
    pub range_db: f32,
}

/// A hang AGC: the gain falls quickly to the loudest peaks, holds through the pauses between words,
/// then rises slowly. Unlike a compressor it does not lift the noise in every pause, so speech
/// stands above the noise by the channel's signal-to-noise ratio. A static crash turns the gain
/// down for a moment, as on a real set.
#[derive(Debug, Clone)]
pub(crate) struct HangAgc {
    timing: HangTiming,
    /// Per sample: the share of the way to a lower gain the attack moves…
    attack: f32,
    /// …the samples it then holds…
    hang_samples: u32,
    /// …and the factor it rises by after that, up to `max_gain`.
    decay: f32,
    max_gain: f32,
    gain: f32,
    hang: u32,
    delay: [f32; LOOKAHEAD],
    cursor: usize,
}

impl HangAgc {
    pub(crate) fn new(timing: HangTiming) -> Self {
        let HangTiming { attack_s, hang_s, decay_db_per_s, target_peak, range_db } = timing;
        let rate = SAMPLE_RATE as f32;
        let max_gain = target_peak / (VOICE_CREST * voice_rms()) * db_to_gain(range_db);
        Self {
            timing,
            attack: 1.0 - pole(1.0, attack_s * rate),
            hang_samples: (hang_s * rate) as u32,
            decay: db_to_gain(decay_db_per_s / rate),
            max_gain,
            gain: max_gain,
            hang: 0,
            delay: [0.0; LOOKAHEAD],
            cursor: 0,
        }
    }

    /// Back to full gain with nothing in flight, as after transmitting.
    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.timing);
    }

    pub(crate) fn process(&mut self, samples: &mut [f32]) {
        let target_peak = self.timing.target_peak;
        for s in samples {
            let ahead = s.abs();
            if ahead * self.gain > target_peak {
                self.gain += (target_peak / ahead - self.gain) * self.attack;
                self.hang = self.hang_samples;
            } else if self.hang > 0 {
                self.hang -= 1;
            } else {
                self.gain = (self.gain * self.decay).min(self.max_gain);
            }
            // The ring's oldest sample goes out as the newest comes in.
            let delayed = self.delay[self.cursor];
            self.delay[self.cursor] = *s;
            self.cursor = (self.cursor + 1) % LOOKAHEAD;
            *s = delayed * self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{db, peak, sine};

    const TIMING: HangTiming =
        HangTiming { attack_s: 0.000_2, hang_s: 0.5, decay_db_per_s: 10.0, target_peak: 0.2, range_db: 6.0 };

    fn run(agc: &mut HangAgc, mut x: Vec<f32>) -> Vec<f32> {
        agc.process(&mut x);
        x
    }

    #[test]
    fn holds_peaks_at_the_target() {
        for amplitude in [0.5, 1.0, 5.0] {
            let out = run(&mut HangAgc::new(TIMING), sine(1000.0, amplitude, 48_000));
            let p = peak(&out[4800..]);
            assert!(db(p / TIMING.target_peak).abs() < 0.5, "{amplitude}: peak {p}");
        }
    }

    #[test]
    fn the_look_ahead_catches_a_sudden_crash() {
        let mut x = sine(1000.0, 0.05, 24_000);
        for (i, s) in x[12_000..12_048].iter_mut().enumerate() {
            *s += 5.0 * (i as f32 * 0.7).sin();
        }
        // A crash 28 dB above the target comes through at most 6 dB above it.
        let out = run(&mut HangAgc::new(TIMING), x);
        assert!(peak(&out) < 2.0 * TIMING.target_peak, "peak {}", peak(&out));
    }

    #[test]
    fn holds_its_gain_through_a_pause_then_rises_slowly() {
        let mut agc = HangAgc::new(TIMING);
        run(&mut agc, sine(1000.0, 1.0, 24_000));
        let loud = agc.gain;
        // A pause shorter than the hang: no change.
        run(&mut agc, vec![0.0; (0.4 * 48_000.0) as usize]);
        assert_eq!(agc.gain, loud);
        // Past the hang, it rises at the decay rate.
        run(&mut agc, vec![0.0; (0.1 * 48_000.0) as usize + 48_000]);
        let rise = db(agc.gain / loud);
        assert!((rise - TIMING.decay_db_per_s).abs() < 0.3, "rose {rise} dB in a second");
    }

    #[test]
    fn the_gain_is_limited_to_its_range_above_an_unfaded_voice() {
        let mut agc = HangAgc::new(TIMING);
        run(&mut agc, vec![0.0; 48_000 * 5]);
        let unfaded = TIMING.target_peak / (VOICE_CREST * voice_rms());
        assert!((db(agc.gain / unfaded) - TIMING.range_db).abs() < 0.01);
        // So a quiet noise floor stays quiet.
        let out = run(&mut agc, sine(1000.0, 0.001, 4800));
        assert!(peak(&out) < 0.01);
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let input = sine(700.0, 0.8, 9600);
        let mut agc = HangAgc::new(TIMING);
        let first = run(&mut agc, input.clone());
        agc.reset();
        assert_eq!(run(&mut agc, input), first);
    }
}
