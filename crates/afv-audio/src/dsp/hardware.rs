//! Radio sets: what each does to what it receives, from its band filter to its speaker.
//!
//! Every set is described the same way, by a [`Spec`], and simulated by one [`RadioSet`]:
//!
//! 1. **Selectivity**: the set's band filter, a high-pass and a low-pass skirt.
//! 2. **Gain control**: a compressor (the VHF sets) or a hang AGC (SSB sets).
//! 3. **Audio response**: the colouring of its audio path and speaker, scaled so that every set
//!    plays speech equally loud.
//! 4. **Output limiter**: keeps static crashes from clipping.
//!
//! VHF sets ([`VhfHardware`]) and HF sets ([`HfHardware`]) are chosen separately, so that a radio
//! always hears HF through an HF set.

use super::biquad::{self, Biquad, Cascade};
use super::compressor::Compressor;
use super::hang_agc::{HangAgc, HangTiming};
use super::limiter::PeakLimiter;
use crate::SAMPLE_RATE;

/// The VHF set whose sound is simulated. The audio responses are afv-native's three presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub enum VhfHardware {
    #[default]
    SchmidEd137b,
    RockwellCollins2100,
    Garex220,
}

/// The HF set whose sound is simulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub enum HfHardware {
    /// Collins HFS-900D, an ARINC 719 HF voice and data radio common in airliners.
    #[default]
    CollinsHfs900d,
}

/// Peaks are limited to this level. Speech peaks at about 0.5, so mostly only static crashes
/// reach it.
const CEILING: f32 = 0.7;
/// Gain of every set's audio response for speech, so that they play it equally loud.
const SPEECH_GAIN: f64 = 1.5;
/// The long-term spectrum of speech, for weighing a response: flat up to this frequency and
/// falling 6 dB per octave above it, close to the international average (Byrne et al. 1994).
const SPEECH_CORNER_HZ: f64 = 500.0;
/// Most sections in a set's selectivity and in its audio response.
const MAX_SELECTIVITY_SECTIONS: usize = 8;
const MAX_RESPONSE_SECTIONS: usize = 7;

/// One skirt of a band filter: its even order, its edge in Hz, and its passband ripple in dB (0
/// for Butterworth, otherwise Chebyshev).
#[derive(Debug, Clone, Copy)]
struct Skirt {
    order: usize,
    hz: f64,
    ripple_db: f64,
}

/// A set's gain control.
#[derive(Debug, Clone, Copy)]
enum GainControlSpec {
    /// The sndfilter compressor with its default settings.
    Compressor,
    Hang(HangTiming),
}

/// One section of an audio response: frequency in Hz, Q, and gain in dB where it applies.
#[derive(Debug, Clone, Copy)]
enum Stage {
    HighPass(f64, f64),
    LowPass(f64, f64),
    Peak(f64, f64, f64),
    LowShelf(f64, f64, f64),
    HighShelf(f64, f64, f64),
    /// Coefficients `[b0, b1, b2]` and `[a1, a2]`.
    Raw([f64; 3], [f64; 2]),
}

use Stage::*;

/// Everything that sets one radio set apart.
#[derive(Debug, Clone, Copy)]
struct Spec {
    high_pass: Skirt,
    low_pass: Skirt,
    gain_control: GainControlSpec,
    response: &'static [Stage],
}

/// A VHF AM receiver's audio passband: 300–3000 Hz with 4th-order Butterworth skirts.
const VHF_HIGH_PASS: Skirt = Skirt { order: 4, hz: 300.0, ripple_db: 0.0 };
const VHF_LOW_PASS: Skirt = Skirt { order: 4, hz: 3000.0, ripple_db: 0.0 };

const SCHMID_ED137B: Spec = Spec {
    high_pass: VHF_HIGH_PASS,
    low_pass: VHF_LOW_PASS,
    gain_control: GainControlSpec::Compressor,
    response: &[
        HighPass(310.0, 0.25),
        Peak(450.0, 0.75, 12.0),
        Peak(1450.0, 1.0, 20.0),
        Peak(2000.0, 1.0, 20.0),
        LowPass(2500.0, 0.25),
    ],
};
const GAREX_220: Spec = Spec {
    high_pass: VHF_HIGH_PASS,
    low_pass: VHF_LOW_PASS,
    gain_control: GainControlSpec::Compressor,
    response: &[
        HighPass(300.0, 0.25),
        HighShelf(400.0, 1.0, 8.0),
        HighShelf(600.0, 1.0, 4.0),
        LowShelf(2000.0, 1.0, 1.0),
        LowShelf(2400.0, 1.0, 3.0),
        LowShelf(3000.0, 1.0, 10.0),
        LowPass(3400.0, 0.25),
    ],
};
/// Its audio response is an elliptic band-pass of about 300–2700 Hz.
const ROCKWELL_COLLINS_2100: Spec = Spec {
    high_pass: VHF_HIGH_PASS,
    low_pass: VHF_LOW_PASS,
    gain_control: GainControlSpec::Compressor,
    response: &[
        Raw([0.0, 1.0, 0.753162969638192], [-1.7152995098277, 0.761385315196423]),
        Raw([1.0, -2.29278115712509, 1.000336632935775], [-1.71626681678914, 0.762433947105989]),
        Raw([1.0, -2.05042803669041, 1.05048374237779], [-1.79384214686345, 0.909678364879526]),
        Raw([1.0, -1.95188929743297, 0.951942325888074], [-1.79409285259567, 0.909822671281377]),
        Raw([1.0, -1.82547932903698, 1.09157529229851], [-1.9390093095185, 0.9411847259142]),
        Raw([1.0, -1.67241244173042, 0.916184578658119], [-1.94022767750807, 0.942630574503006]),
    ],
};

/// The Collins HFS-900D.
///
/// Selectivity, from Collins' data sheet (147-0795-001, 2015): the SSB passband varies less than
/// 4 dB from 350 to 2500 Hz, is 35 dB down from 2900 Hz and on the carrier side within 300 Hz,
/// and 60 dB down beyond 3300 Hz. A 6th-order Butterworth high-pass at 300 Hz and a 10th-order
/// Chebyshev low-pass with 2 dB ripple ending at 2500 Hz meet that mask. The upper skirt falls
/// 35 dB in 400 Hz, so the set sounds narrow and its filter rings a little on static.
///
/// The AGC is not published. Its timing follows airborne HF sets of the same class (the Marconi
/// 915 allows 12 ms attack and 1.5 s recovery): 3 ms attack, a quarter of a second's hang, then
/// 20 dB per second, with 8 dB of gain in reserve, like the RF sensitivity control where crews
/// usually leave it. Its audio response is flat.
const COLLINS_HFS900D: Spec = Spec {
    high_pass: Skirt { order: 6, hz: 300.0, ripple_db: 0.0 },
    low_pass: Skirt { order: 10, hz: 2500.0, ripple_db: 2.0 },
    gain_control: GainControlSpec::Hang(HangTiming {
        attack_s: 0.003,
        hang_s: 0.25,
        decay_db_per_s: 20.0,
        // Typical speech plays about 7 dB below the VHF sets.
        target_peak: 0.13,
        range_db: 8.0,
    }),
    response: &[],
};

impl VhfHardware {
    fn spec(self) -> &'static Spec {
        match self {
            Self::SchmidEd137b => &SCHMID_ED137B,
            Self::RockwellCollins2100 => &ROCKWELL_COLLINS_2100,
            Self::Garex220 => &GAREX_220,
        }
    }
}

impl HfHardware {
    fn spec(self) -> &'static Spec {
        match self {
            Self::CollinsHfs900d => &COLLINS_HFS900D,
        }
    }
}

impl Stage {
    fn biquad(self) -> Biquad {
        match self {
            HighPass(f, q) => Biquad::high_pass(f, q),
            LowPass(f, q) => Biquad::low_pass(f, q),
            Peak(f, q, db) => Biquad::peaking(f, q, db),
            LowShelf(f, q, db) => Biquad::low_shelf(f, q, db),
            HighShelf(f, q, db) => Biquad::high_shelf(f, q, db),
            Raw(b, [a1, a2]) => Biquad::new(b, [1.0, a1, a2]),
        }
    }
}

/// The long-term power spectral density of speech at `freq`, relative to its low-frequency level.
/// Falling 9 dB per octave in density is 6 dB per octave in octave bands.
fn speech_spectrum(freq: f64) -> f64 {
    1.0 / (1.0 + (freq / SPEECH_CORNER_HZ).powi(3))
}

/// The ear's sensitivity at `freq` as a power ratio: the A-weighting curve (IEC 61672), 1 at 1 kHz.
fn a_weighting(freq: f64) -> f64 {
    let f2 = freq * freq;
    let (a, b, c, d) = (20.6f64.powi(2), 107.7f64.powi(2), 737.9f64.powi(2), 12_194f64.powi(2));
    let r = d * f2 * f2 / ((f2 + a) * ((f2 + b) * (f2 + c)).sqrt() * (f2 + d));
    (r * 10f64.powf(2.0 / 20.0)).powi(2)
}

/// A response as heard by speech: its power gain averaged over the speech spectrum, each
/// frequency counted as loud as the ear hears it.
fn speech_gain(response: &Cascade<MAX_RESPONSE_SECTIONS>) -> f64 {
    // On a log-spaced grid from 50 Hz to 10 kHz, where each point stands for a band as wide as
    // its frequency.
    let (mut power, mut weight) = (0.0, 0.0);
    for k in 0..=200 {
        let f = 50.0 * 200f64.powf(k as f64 / 200.0);
        let w = f * speech_spectrum(f) * a_weighting(f);
        power += w * response.response(f).powi(2);
        weight += w;
    }
    (power / weight).sqrt()
}

impl Spec {
    fn selectivity(&self) -> Cascade<MAX_SELECTIVITY_SECTIONS> {
        let (h, l) = (self.high_pass, self.low_pass);
        Cascade::new(biquad::high_pass(h.order, h.hz, h.ripple_db).chain(biquad::low_pass(l.order, l.hz, l.ripple_db)))
    }

    /// The audio response and the gain that makes it play speech at [`SPEECH_GAIN`].
    fn response(&self) -> (Cascade<MAX_RESPONSE_SECTIONS>, f32) {
        let response = Cascade::new(self.response.iter().map(|s| s.biquad()));
        let gain = (SPEECH_GAIN / speech_gain(&response)) as f32;
        (response, gain)
    }
}

#[derive(Debug, Clone)]
enum GainControl {
    Compressor(Compressor),
    Hang(HangAgc),
}

/// One radio set's receiver, applied to everything it hears: band filter, gain control, audio
/// response, output limiter.
#[derive(Debug, Clone)]
pub struct RadioSet {
    selectivity: Cascade<MAX_SELECTIVITY_SECTIONS>,
    gain_control: GainControl,
    response: Cascade<MAX_RESPONSE_SECTIONS>,
    response_gain: f32,
    limiter: PeakLimiter,
}

impl RadioSet {
    pub fn vhf(hardware: VhfHardware) -> Self {
        Self::new(hardware.spec())
    }

    pub fn hf(hardware: HfHardware) -> Self {
        Self::new(hardware.spec())
    }

    fn new(spec: &Spec) -> Self {
        let (response, response_gain) = spec.response();
        let gain_control = match spec.gain_control {
            GainControlSpec::Compressor => GainControl::Compressor(Compressor::default_for_rate(SAMPLE_RATE)),
            GainControlSpec::Hang(timing) => GainControl::Hang(HangAgc::new(timing)),
        };
        Self {
            selectivity: spec.selectivity(),
            gain_control,
            response,
            response_gain,
            limiter: PeakLimiter::new(CEILING),
        }
    }

    /// Back to the state it starts in, as after transmitting: a hang AGC at full gain.
    pub fn reset(&mut self) {
        self.selectivity.reset();
        match &mut self.gain_control {
            GainControl::Compressor(c) => c.reset(),
            GainControl::Hang(agc) => agc.reset(),
        }
        self.response.reset();
        self.limiter.reset();
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        self.selectivity.process(samples);
        match &mut self.gain_control {
            GainControl::Compressor(c) => c.process(samples),
            GainControl::Hang(agc) => agc.process(samples),
        }
        self.response.process(samples);
        samples.iter_mut().for_each(|s| *s *= self.response_gain);
        self.limiter.process(samples);
    }
}

#[cfg(test)]
mod tests {
    use rand::{RngExt, SeedableRng};

    use super::*;
    use crate::test_util::{db, peak, rms, sine, tone};

    const VHF: [VhfHardware; 3] = [VhfHardware::SchmidEd137b, VhfHardware::RockwellCollins2100, VhfHardware::Garex220];

    fn every_spec() -> Vec<&'static Spec> {
        VHF.iter().map(|h| h.spec()).chain([HfHardware::CollinsHfs900d.spec()]).collect()
    }

    fn every_set() -> Vec<RadioSet> {
        every_spec().into_iter().map(RadioSet::new).collect()
    }

    #[test]
    fn silence_stays_silent() {
        for mut set in every_set() {
            let mut x = vec![0.0; 4800];
            set.process(&mut x);
            assert!(x.iter().all(|&s| s == 0.0));
        }
    }

    #[test]
    fn every_set_is_a_voice_band_filter() {
        for spec in every_spec() {
            let (response, gain) = spec.response();
            let at = |f| spec.selectivity().response(f) * response.response(f) * gain as f64;
            for outside in [100.0, 6000.0] {
                assert!(at(1500.0) / at(outside) > 10.0, "{spec:?} at {outside} Hz");
            }
        }
    }

    /// Gain of the HFS-900D's selectivity at `freq`, in dB, measured on a tone.
    fn hfs900d_selectivity_db(freq: f64) -> f32 {
        let mut f = COLLINS_HFS900D.selectivity();
        // One second: the low-pass rings for tens of milliseconds near its edge.
        let mut x = sine(freq, 1.0, 48_000);
        f.process(&mut x);
        db(tone(&x[24_000..], freq))
    }

    #[test]
    fn the_hfs900d_selectivity_meets_the_published_mask() {
        let passband: Vec<f32> = (350..=2500).step_by(25).map(|f| hfs900d_selectivity_db(f as f64)).collect();
        let (lo, hi) = passband.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &g| (lo.min(g), hi.max(g)));
        assert!(hi - lo < 4.0, "passband variation {:.2} dB ({lo:.2} to {hi:.2})", hi - lo);
        for f in [2900.0, 3100.0, 3300.0] {
            assert!(hfs900d_selectivity_db(f) < -35.0, "{f} Hz: {:.1} dB", hfs900d_selectivity_db(f));
        }
        for f in [3400.0, 4000.0, 8000.0, 50.0] {
            assert!(hfs900d_selectivity_db(f) < -60.0, "{f} Hz: {:.1} dB", hfs900d_selectivity_db(f));
        }
        assert!(hfs900d_selectivity_db(150.0) < -35.0, "carrier side: {:.1} dB", hfs900d_selectivity_db(150.0));
    }

    #[test]
    fn the_hfs900d_rings_on_an_impulse() {
        let mut f = COLLINS_HFS900D.selectivity();
        let mut x = vec![0.0; 4800];
        x[0] = 1.0;
        f.process(&mut x);
        let energy = |r: std::ops::Range<usize>| x[r].iter().map(|s| s * s).sum::<f32>();
        let total = energy(0..4800);
        assert!(energy(96..4800) > 0.03 * total, "rings past 2 ms");
        assert!(energy(2400..4800) < 0.002 * total, "gone by 50 ms");
    }

    #[test]
    fn the_hfs900d_holds_strong_and_weak_signals_at_one_level() {
        let out = |amplitude: f64| {
            let mut x = sine(1000.0, amplitude, 48_000);
            RadioSet::hf(HfHardware::CollinsHfs900d).process(&mut x);
            peak(&x[24_000..])
        };
        let reference = out(0.3);
        for amplitude in [1.0, 10.0] {
            assert!((db(out(amplitude)) - db(reference)).abs() < 0.5, "{amplitude}: {} vs {reference}", out(amplitude));
        }
    }

    #[test]
    fn after_a_crash_the_hfs900d_hangs_then_recovers() {
        // A tone, a 20 dB louder burst for 100 ms, then the tone again.
        let GainControlSpec::Hang(timing) = COLLINS_HFS900D.gain_control else { unreachable!() };
        let rate = SAMPLE_RATE as usize;
        let mut x = sine(1000.0, 0.3, 2 * rate);
        for s in &mut x[rate / 2..rate / 2 + rate / 10] {
            *s *= 10.0;
        }
        RadioSet::hf(HfHardware::CollinsHfs900d).process(&mut x);
        let at = |t: f64| {
            let i = (t * rate as f64) as usize;
            peak(&x[i..i + 480])
        };
        let (before, hang) = (at(0.4), timing.hang_s as f64);
        let hanging = at(0.6 + hang * 0.5);
        assert!((db(before) - db(hanging) - 20.0).abs() < 1.0, "before {before}, during the hang {hanging}");
        let recovering = at(0.6 + hang + 0.5);
        assert!((db(before) - db(recovering) - 10.0).abs() < 1.5, "recovering {recovering}");
        let recovered = at(0.6 + hang + 1.05);
        assert!((db(before) - db(recovered)).abs() < 0.5, "recovered {recovered}");
    }

    #[test]
    fn every_set_plays_speech_equally_loud() {
        // Noise with the spectrum of speech as the ear weighs it (so that its plain RMS is its
        // loudness): random-phase sines 10 Hz apart, through each set's audio response.
        let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
        let sines: Vec<(f64, f64, f64)> = (5..1000)
            .map(|k| {
                let f = 10.0 * k as f64;
                let amplitude = 0.002 * (speech_spectrum(f) * a_weighting(f)).sqrt();
                (f, amplitude, rng.random::<f64>() * std::f64::consts::TAU)
            })
            .collect();
        let speech: Vec<f32> = (0..48_000)
            .map(|n| {
                let t = n as f64 / 48_000.0;
                sines.iter().map(|(f, a, p)| a * (std::f64::consts::TAU * f * t + p).sin()).sum::<f64>() as f32
            })
            .collect();
        for spec in every_spec() {
            let (mut response, gain) = spec.response();
            let mut x = speech.clone();
            response.process(&mut x);
            let level = db(gain * rms(&x[4800..]) / rms(&speech[4800..]));
            assert!((level - db(SPEECH_GAIN as f32)).abs() < 0.3, "{spec:?}: {level:.2} dB");
        }
    }

    #[test]
    fn a_weighting_matches_its_table() {
        for (freq, expected) in [(100.0, -19.1), (500.0, -3.2), (1000.0, 0.0), (2000.0, 1.2), (8000.0, -1.1)] {
            let got = 10.0 * a_weighting(freq).log10();
            assert!((got - expected).abs() < 0.1, "{freq} Hz: {got:.2} dB");
        }
    }

    #[test]
    fn loud_peaks_are_limited() {
        for mut set in every_set() {
            let mut x = vec![0.0; 4800];
            x[100] = 50.0;
            x.extend(sine(1500.0, 10.0, 4800));
            set.process(&mut x);
            assert!(x.iter().all(|s| s.abs() <= CEILING * 1.000_01));
        }
    }

    #[test]
    fn reset_makes_processing_repeatable() {
        let input = sine(1000.0, 0.9, 4800);
        for mut set in every_set() {
            let mut first = input.clone();
            set.process(&mut first);
            set.reset();
            let mut second = input.clone();
            set.process(&mut second);
            assert_eq!(first, second);
        }
    }
}
