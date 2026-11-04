//! Envelope detection of several AM transmitters on one channel, for simultaneous transmissions.
//!
//! Replaces afv-native's fixed 180 Hz "blocking" tone. Real transmitters are a few ppm off their
//! nominal frequency (1 ppm is 125 Hz at 125 MHz), so two stations keying at once produce two
//! carriers some hundreds of Hz apart. Each is modelled at complex baseband as
//! `C·(1 + m(t))·e^{j(2πΔf·t + φ)}`, and the receiver hears the magnitude of their sum. With one
//! transmitter that is just its audio. With a strong and a weak one it is the strong voice, a
//! heterodyne whistle at the carrier difference, and the weak voice ring-modulated by that
//! difference. Two equally strong signals distort each other heavily.

use rand::RngExt;

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

/// Pole of the DC blocker that stands in for the receiver's AC coupling (about 40 Hz).
const DC_BLOCK_POLE: f32 = 0.995;

/// A transmitter's carrier relative to the channel's nominal frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Carrier {
    /// Offset from the nominal frequency in Hz.
    pub offset_hz: i16,
    /// Phase at the start of each second, in 1/48000ths of a turn.
    pub phase: u16,
}

impl Carrier {
    /// Largest offset [`Carrier::random`] picks.
    pub const MAX_OFFSET_HZ: i16 = 600;

    /// A transmitter's carrier, anywhere within ±[`MAX_OFFSET_HZ`](Self::MAX_OFFSET_HZ). Keep it
    /// for as long as the station is heard, so that a given pair of stations keeps beating at the
    /// same frequency, as real ones do.
    pub fn random() -> Self {
        super::with_rng(|rng| Self {
            offset_hz: rng.random_range(-Self::MAX_OFFSET_HZ..=Self::MAX_OFFSET_HZ),
            phase: rng.random_range(0..SAMPLE_RATE as u16),
        })
    }
}

/// Sums the transmitters heard on one channel during a frame and detects the envelope.
///
/// Call [`begin`](Self::begin) once per frame, [`add`](Self::add) for each transmitter, then
/// [`detect`](Self::detect) or [`idle`](Self::idle).
pub struct AmChannel {
    re: Box<Frame>,
    im: Box<Frame>,
    /// Sum of the squared carrier amplitudes added this frame.
    power: f32,
    /// Sample index of the frame start within the current second.
    clock: u32,
    dc_in: f32,
    dc_out: f32,
}

impl Default for AmChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl AmChannel {
    pub fn new() -> Self {
        Self {
            re: Box::new([0.0; FRAME_SAMPLES]),
            im: Box::new([0.0; FRAME_SAMPLES]),
            power: 0.0,
            clock: 0,
            dc_in: 0.0,
            dc_out: 0.0,
        }
    }

    /// Starts the next frame.
    pub fn begin(&mut self) {
        self.re.fill(0.0);
        self.im.fill(0.0);
        self.power = 0.0;
        self.clock = (self.clock + FRAME_SAMPLES as u32) % SAMPLE_RATE;
    }

    /// Adds a transmitter with carrier amplitude `level`, modulated by `audio · modulation`.
    /// Modulation below −100 % cuts the carrier, as on air.
    pub fn add(&mut self, audio: &Frame, modulation: f32, level: f32, carrier: Carrier) {
        self.power += level * level;
        let rate = SAMPLE_RATE as i64;
        for (i, (s, (re, im))) in audio.iter().zip(self.re.iter_mut().zip(self.im.iter_mut())).enumerate() {
            // Integer offsets make the phase periodic over one second, so it is exact forever.
            let turn =
                (carrier.offset_hz as i64 * (self.clock as i64 + i as i64) + carrier.phase as i64).rem_euclid(rate);
            let (sin, cos) = (std::f32::consts::TAU * turn as f32 / rate as f32).sin_cos();
            let amplitude = level * (1.0 + modulation * s).max(0.0);
            *re += amplitude * cos;
            *im += amplitude * sin;
        }
    }

    /// Writes the detected audio, normalised to the total carrier power the way a receiver's AGC
    /// would. A lone transmitter yields its `audio · modulation` unchanged apart from AC coupling.
    pub fn detect(&mut self, out: &mut Frame) {
        if self.power <= 0.0 {
            out.fill(0.0);
            return;
        }
        let norm = self.power.sqrt();
        for (o, (re, im)) in out.iter_mut().zip(self.re.iter().zip(self.im.iter())) {
            let x = (re.hypot(*im) - norm) / norm;
            self.dc_out = x - self.dc_in + DC_BLOCK_POLE * self.dc_out;
            self.dc_in = x;
            *o = self.dc_out;
        }
    }

    /// Ends a frame without detection, so the next detection starts from fresh filter state.
    pub fn idle(&mut self) {
        (self.dc_in, self.dc_out) = (0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{rms, sine_frame, tone};

    /// Renders `frames` frames of the given transmitters `(audio freq, level, carrier offset)`.
    fn render(frames: usize, txs: &[(f64, f32, i16)]) -> Vec<f32> {
        let mut am = AmChannel::new();
        let mut out = Vec::new();
        let mut frame = [0.0; FRAME_SAMPLES];
        for n in 0..frames {
            am.begin();
            for &(freq, level, offset_hz) in txs {
                am.add(&sine_frame(n, freq, 0.3), 1.0, level, Carrier { offset_hz, phase: 0 });
            }
            am.detect(&mut frame);
            out.extend_from_slice(&frame);
        }
        out
    }

    /// Renders 11 frames and drops the first while the DC blocker settles; the remaining 10
    /// frames (200 ms) put every multiple of 5 Hz on a DFT bin.
    fn render_settled(txs: &[(f64, f32, i16)]) -> Vec<f32> {
        render(11, txs).split_off(FRAME_SAMPLES)
    }

    #[test]
    fn random_carriers_are_bounded() {
        for _ in 0..1000 {
            let c = Carrier::random();
            assert!(c.offset_hz.abs() <= Carrier::MAX_OFFSET_HZ && (c.phase as u32) < SAMPLE_RATE, "{c:?}");
        }
    }

    #[test]
    fn carrier_offsets_cover_the_range() {
        crate::dsp::seed_rng(1);
        let offsets: Vec<i16> = (0..2000).map(|_| Carrier::random().offset_hz).collect();
        let max = Carrier::MAX_OFFSET_HZ;
        assert!(offsets.iter().any(|&o| o < -max / 2) && offsets.iter().any(|&o| o > max / 2));
        let mean = offsets.iter().map(|&o| o as f64).sum::<f64>() / offsets.len() as f64;
        assert!(mean.abs() < 30.0, "mean offset {mean}");
    }

    #[test]
    fn no_transmitters_is_silence() {
        let mut am = AmChannel::new();
        let mut out = [1.0; FRAME_SAMPLES];
        am.begin();
        am.detect(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_lone_transmitter_is_demodulated_unchanged() {
        let out = render_settled(&[(440.0, 0.4, 250)]);
        assert!((tone(&out, 440.0) - 0.3).abs() < 0.01, "{}", tone(&out, 440.0));
        // The carrier offset of a lone transmitter is inaudible.
        assert!(tone(&out, 250.0) < 1e-3);
    }

    #[test]
    fn modulation_scales_the_audio() {
        let mut am = AmChannel::new();
        let audio = sine_frame(0, 1000.0, 0.3);
        let mut out = [0.0; FRAME_SAMPLES];
        am.begin();
        am.add(&audio, 0.5, 1.0, Carrier::default());
        am.detect(&mut out);
        // With fresh DC-blocker state the first sample passes unfiltered.
        assert!((out[1] - 0.5 * audio[1]).abs() < 1e-6);
    }

    #[test]
    fn overmodulation_cuts_the_carrier() {
        // Audio below −100 % modulation cannot make the envelope negative: it clips at zero,
        // which after normalisation is −1.
        let mut am = AmChannel::new();
        let mut out = [0.0; FRAME_SAMPLES];
        am.begin();
        am.add(&[-2.0; FRAME_SAMPLES], 1.0, 1.0, Carrier::default());
        am.detect(&mut out);
        assert_eq!(out[0], -1.0);
    }

    #[test]
    fn a_weak_second_carrier_adds_a_heterodyne_and_ring_modulates_its_voice() {
        // Strong 440 Hz voice at +100 Hz, weak 1000 Hz voice at +400 Hz: beat at 300 Hz, and the
        // weak voice reappears around the beat at 1000 ± 300 Hz.
        let out = render_settled(&[(440.0, 1.0, 100), (1000.0, 0.1, 400)]);
        let strong = tone(&out, 440.0);
        let beat = tone(&out, 300.0);
        let sidebands = tone(&out, 700.0) + tone(&out, 1300.0);
        assert!((strong - 0.3).abs() < 0.02, "strong voice {strong}");
        assert!((beat - 0.1).abs() < 0.01, "beat {beat}");
        assert!(sidebands > 0.02, "sidebands {sidebands}");
        assert!(tone(&out, 1000.0) < 0.005, "weak voice not at baseband");
    }

    #[test]
    fn the_stronger_carrier_dominates() {
        // Swapping which voice is strong swaps which one is heard at baseband.
        let a = render_settled(&[(440.0, 1.0, 100), (1000.0, 0.1, 400)]);
        let b = render_settled(&[(440.0, 0.1, 100), (1000.0, 1.0, 400)]);
        assert!(tone(&a, 440.0) > 10.0 * tone(&a, 1000.0));
        assert!(tone(&b, 1000.0) > 10.0 * tone(&b, 440.0));
    }

    #[test]
    fn equal_carriers_beat_loudly() {
        let out = render_settled(&[(440.0, 0.5, -150), (440.0, 0.5, 150)]);
        assert!(rms(&out) > 0.3, "rms {}", rms(&out));
        assert!(tone(&out, 300.0) > 0.4, "beat {}", tone(&out, 300.0));
    }

    #[test]
    fn the_beat_is_continuous_across_frames_and_seconds() {
        // Two unmodulated carriers 150 Hz apart repeat every 320 samples, including across
        // frame boundaries and the wrap of the one-second phase clock.
        let mut am = AmChannel::new();
        let silence = [0.0; FRAME_SAMPLES];
        let mut out = Vec::new();
        let mut frame = [0.0; FRAME_SAMPLES];
        for _ in 0..55 {
            am.begin();
            am.add(&silence, 1.0, 1.0, Carrier { offset_hz: -50, phase: 1234 });
            am.add(&silence, 1.0, 0.5, Carrier { offset_hz: 100, phase: 0 });
            am.detect(&mut frame);
            out.extend_from_slice(&frame);
        }
        for n in 5000..out.len() {
            assert!((out[n] - out[n - 320]).abs() < 1e-3, "sample {n}");
        }
    }

    #[test]
    fn idle_resets_the_dc_blocker() {
        let mut am = AmChannel::new();
        let mut out = [0.0; FRAME_SAMPLES];
        let audio = [0.2; FRAME_SAMPLES];
        for _ in 0..3 {
            am.begin();
            am.add(&audio, 1.0, 1.0, Carrier::default());
            am.detect(&mut out);
        }
        // The DC blocker has removed most of the constant.
        assert!(out[FRAME_SAMPLES - 1].abs() < 0.01);
        am.idle();
        am.begin();
        am.add(&audio, 1.0, 1.0, Carrier::default());
        am.detect(&mut out);
        assert!((out[0] - 0.2).abs() < 1e-6);
    }
}
