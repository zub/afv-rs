//! Radio "hardware" voice filter: expander, EQ preset, limiter. Port of afv-native's
//! `VHFFilterSource`.

use super::biquad::Biquad;
use super::chunkware::{SimpleComp, SimpleLimit};
use crate::SAMPLE_RATE;

/// Radio set whose frequency response is simulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub enum Hardware {
    #[default]
    SchmidEd137b,
    RockwellCollins2100,
    Garex220,
}

// afv-native's settings. The expander (a compressor with a ratio above 1) steepens levels above
// its threshold; the limiter catches the result.
const EXPANDER_ATTACK_MS: f64 = 0.1;
const EXPANDER_RELEASE_MS: f64 = 80.0;
const EXPANDER_THRESHOLD_DB: f64 = -8.0;
const EXPANDER_RATIO: f64 = 6.0;
const LIMITER_ATTACK_MS: f64 = 0.1;
const LIMITER_RELEASE_MS: f64 = 80.0;
const LIMITER_THRESHOLD_DB: f64 = 8.0;
const POST_GAIN_DB: f64 = -5.5;
/// ChunkWare's default sample rate, in effect when afv-native sets the limiter's attack.
const ORIGINAL_DEFAULT_RATE: f64 = 44_100.0;

#[derive(Debug, Clone)]
pub struct HardwareFilter {
    hardware: Hardware,
    expander: SimpleComp,
    limiter: SimpleLimit,
    post_gain: f32,
    filters: Vec<Biquad>,
}

impl HardwareFilter {
    pub fn new(hardware: Hardware) -> Self {
        let rate = SAMPLE_RATE as f64;
        // In the original the limiter's look-ahead is computed when the attack is set, before the
        // sample rate is changed from its default: int(0.001 · 0.1 ms · 44100) = 4.
        let peak_hold = (0.001 * LIMITER_ATTACK_MS * ORIGINAL_DEFAULT_RATE) as usize;
        Self {
            hardware,
            expander: SimpleComp::new(
                rate,
                EXPANDER_ATTACK_MS,
                EXPANDER_RELEASE_MS,
                EXPANDER_THRESHOLD_DB,
                EXPANDER_RATIO,
            ),
            limiter: SimpleLimit::new(rate, LIMITER_ATTACK_MS, LIMITER_RELEASE_MS, LIMITER_THRESHOLD_DB, peak_hold),
            post_gain: 10f64.powf(POST_GAIN_DB / 20.0) as f32,
            filters: presets(hardware),
        }
    }

    pub fn hardware(&self) -> Hardware {
        self.hardware
    }

    /// Clears all filter state, as if newly constructed.
    pub fn reset(&mut self) {
        self.expander.reset();
        self.limiter.reset();
        self.filters.iter_mut().for_each(Biquad::reset);
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for sample in samples {
            let mut l = *sample as f64;
            let mut r = l;
            self.expander.process(&mut l, &mut r);
            let mut filtered = l as f32;
            for f in &mut self.filters {
                filtered = f.process(filtered);
            }
            // The limiter is keyed on both the filtered signal and the unfiltered expander output.
            let mut l = filtered as f64;
            self.limiter.process(&mut l, &mut r);
            *sample = (l * self.post_gain as f64) as f32;
        }
    }
}

/// One stage of an EQ preset: frequency in Hz, Q, and gain in dB where it applies.
#[derive(Debug, Clone, Copy)]
enum Stage {
    HighPass(f32, f32),
    LowPass(f32, f32),
    Peak(f32, f32, f32),
    LowShelf(f32, f32, f32),
    HighShelf(f32, f32, f32),
    /// Raw coefficients `[aa0, aa1, aa2, b0, b1, b2]`.
    Raw([f64; 6]),
}

use Stage::*;

// afv-native's presets (`VHFFilterSource`).
const SCHMID_ED137B: &[Stage] = &[
    HighPass(310.0, 0.25),
    Peak(450.0, 0.75, 12.0),
    Peak(1450.0, 1.0, 20.0),
    Peak(2000.0, 1.0, 20.0),
    LowPass(2500.0, 0.25),
];
const GAREX_220: &[Stage] = &[
    HighPass(300.0, 0.25),
    HighShelf(400.0, 1.0, 8.0),
    HighShelf(600.0, 1.0, 4.0),
    LowShelf(2000.0, 1.0, 1.0),
    LowShelf(2400.0, 1.0, 3.0),
    LowShelf(3000.0, 1.0, 10.0),
    LowPass(3400.0, 0.25),
];
const ROCKWELL_COLLINS_2100: &[Stage] = &[
    Raw([1.0, 0.0, 0.0, -0.01, 0.0, 0.0]),
    Raw([1.0, -1.7152995098277, 0.761385315196423, 0.0, 1.0, 0.753162969638192]),
    Raw([1.0, -1.71626681678914, 0.762433947105989, 1.0, -2.29278115712509, 1.000336632935775]),
    Raw([1.0, -1.79384214686345, 0.909678364879526, 1.0, -2.05042803669041, 1.05048374237779]),
    Raw([1.0, -1.79409285259567, 0.909822671281377, 1.0, -1.95188929743297, 0.951942325888074]),
    Raw([1.0, -1.9390093095185, 0.9411847259142, 1.0, -1.82547932903698, 1.09157529229851]),
    Raw([1.0, -1.94022767750807, 0.942630574503006, 1.0, -1.67241244173042, 0.916184578658119]),
];

impl Stage {
    fn biquad(self) -> Biquad {
        let r = SAMPLE_RATE as f32;
        match self {
            HighPass(f, q) => Biquad::high_pass(r, f, q),
            LowPass(f, q) => Biquad::low_pass(r, f, q),
            Peak(f, q, db) => Biquad::peaking_eq(r, f, q, db),
            LowShelf(f, q, db) => Biquad::low_shelf(r, f, q, db),
            HighShelf(f, q, db) => Biquad::high_shelf(r, f, q, db),
            Raw([aa0, aa1, aa2, b0, b1, b2]) => Biquad::from_coefficients(aa0, aa1, aa2, b0, b1, b2),
        }
    }
}

fn presets(hardware: Hardware) -> Vec<Biquad> {
    let stages = match hardware {
        Hardware::SchmidEd137b => SCHMID_ED137B,
        Hardware::RockwellCollins2100 => ROCKWELL_COLLINS_2100,
        Hardware::Garex220 => GAREX_220,
    };
    stages.iter().map(|s| s.biquad()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{db, peak, sine, tone};

    const ALL: [Hardware; 3] = [Hardware::SchmidEd137b, Hardware::RockwellCollins2100, Hardware::Garex220];
    /// Look-ahead of the limiter; see [`HardwareFilter::new`].
    const DELAY: usize = 4;

    fn response_db(hardware: Hardware, freq: f64) -> f32 {
        let mut filter = HardwareFilter::new(hardware);
        let mut x = sine(freq, 0.01, 48_000);
        filter.process(&mut x);
        db(peak(&x[24_000..]) / 0.01)
    }

    #[test]
    fn silence_stays_silent() {
        for hw in ALL {
            let mut x = vec![0.0; 4800];
            HardwareFilter::new(hw).process(&mut x);
            assert!(x.iter().all(|&s| s == 0.0), "{hw:?}");
        }
    }

    #[test]
    fn quiet_signals_go_through_the_eq_chain_only() {
        // Below the expander's −8 dB threshold and the limiter's +8 dB threshold both dynamics
        // stages have exactly unity gain, so the output is the biquad chain, delayed by the
        // limiter's look-ahead and scaled by the post gain.
        let input: Vec<f32> =
            sine(700.0, 0.01, 4800).iter().zip(sine(2300.0, 0.01, 4800)).map(|(a, b)| a + b).collect();
        for hw in ALL {
            let mut out = input.clone();
            HardwareFilter::new(hw).process(&mut out);
            let mut chain = presets(hw);
            let eq: Vec<f32> = input.iter().map(|&x| chain.iter_mut().fold(x, |s, f| f.process(s))).collect();
            let post_gain = 10f64.powf(-5.5 / 20.0) as f32;
            assert!(out[..DELAY].iter().all(|&s| s == 0.0));
            for (i, (&o, &e)) in out[DELAY..].iter().zip(&eq).enumerate() {
                assert_eq!(o, (e as f64 * post_gain as f64) as f32, "{hw:?} sample {i}");
            }
        }
    }

    #[test]
    fn every_preset_is_a_voice_band_filter() {
        for hw in ALL {
            let band = response_db(hw, 1500.0);
            for outside in [100.0, 6000.0] {
                let r = response_db(hw, outside);
                assert!(band - r > 20.0, "{hw:?}: {band:.1} dB at 1.5 kHz, {r:.1} dB at {outside} Hz");
            }
        }
    }

    #[test]
    fn loud_signals_stop_at_the_limiter_ceiling() {
        // Limiter threshold +8 dB, then −5.5 dB post gain.
        let ceiling = 10f32.powf((8.0 - 5.5) / 20.0);
        for hw in ALL {
            for amplitude in [0.5, 0.9, 1.0] {
                let mut x = sine(1000.0, amplitude, 48_000);
                HardwareFilter::new(hw).process(&mut x);
                let p = peak(&x[4800..]);
                assert!(
                    p <= ceiling * 1.001 && p > ceiling * 0.9,
                    "{hw:?} at {amplitude}: peak {p}, ceiling {ceiling}"
                );
            }
        }
    }

    #[test]
    fn the_expander_steepens_levels_above_its_threshold() {
        // Expander: threshold −8 dBFS, ratio 6. A 1 dB step below the threshold stays 1 dB; above
        // it, 6 dB. The limiter is also keyed on the unfiltered expander output, so stay below
        // +8 dBFS there: −6.5 dBFS expanded by 5 × 1.5 dB peaks at +1 dBFS.
        let level = |dbfs: f64| {
            let mut x = sine(150.0, 10f64.powf(dbfs / 20.0), 48_000);
            HardwareFilter::new(Hardware::SchmidEd137b).process(&mut x);
            // The fundamental only: the gain varies within each cycle, and the EQ boosts the
            // resulting harmonics far above 150 Hz.
            db(tone(&x[24_000..], 150.0))
        };
        let below = level(-11.0) - level(-12.0);
        let above = level(-6.5) - level(-7.5);
        assert!((below - 1.0).abs() < 0.01, "below threshold: {below:.2} dB per dB");
        assert!((above - 6.0).abs() < 0.1, "above threshold: {above:.2} dB per dB");
    }

    #[test]
    fn the_limiter_is_keyed_on_the_unfiltered_expander_output_too() {
        // A loud 60 Hz tone is expanded far above the limiter threshold, but the EQ removes most
        // of it. Because the limiter also sees the unfiltered expander output, its gain is
        // threshold / expanded peak, so the output's fundamental is threshold · |H(60 Hz)| ·
        // post gain whatever the input level. Keyed on the filtered signal alone it would not
        // limit at all.
        let quiet_response = 10f32.powf(response_db(Hardware::SchmidEd137b, 60.0) / 20.0);
        let expected = 10f32.powf(8.0 / 20.0) * quiet_response;
        for dbfs in [-4.0, -2.0] {
            let mut x = sine(60.0, 10f64.powf(dbfs / 20.0), 48_000);
            HardwareFilter::new(Hardware::SchmidEd137b).process(&mut x);
            let got = tone(&x[24_000..], 60.0);
            assert!((db(got) - db(expected)).abs() < 1.0, "{dbfs} dBFS: {} dB, expected {} dB", db(got), db(expected));
        }
    }

    #[test]
    fn reset_makes_processing_repeatable() {
        let input = sine(1000.0, 0.9, 4800);
        for hw in ALL {
            let mut filter = HardwareFilter::new(hw);
            let mut first = input.clone();
            filter.process(&mut first);
            filter.reset();
            let mut second = input.clone();
            filter.process(&mut second);
            assert_eq!(first, second, "{hw:?}");
        }
    }
}
