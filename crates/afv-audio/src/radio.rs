//! Receive-side simulation of one radio. Port of afv-native's `ATCRadioSimulation::_process_radio`
//! (the pilot `RadioSimulation` behaves the same for a single matching transceiver), except that
//! simultaneous transmissions on VHF are rendered by [`AmChannel`] instead of a fixed tone.

use afv_proto::consts::HF_UPPER_LIMIT_HZ;
use afv_proto::voice::RxTransceiver;

use crate::dsp::{AmChannel, Atmospherics, Carrier, Compressor, Hardware, HardwareFilter, HfConditions, SsbFilter};
use crate::effects::{EffectSamples, SamplePlayer};
use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

const CLICK_GAIN: f32 = 1.3;
const AC_BUS_GAIN: f32 = 0.005;
const HF_AC_BUS_GAIN: f32 = 0.001;
const VHF_NOISE_GAIN: f32 = 0.17;
const HF_NOISE_GAIN: f32 = 0.16;
/// afv-native's weak-signal crackle on VHF: `e^d · d^POWER / DIVISOR − OFFSET` for a
/// `DistanceRatio` d, clamped to 0‥MAX. OFFSET is e/350, so a full-strength signal has none.
const CRACKLE_POWER: f64 = -4.0;
const CRACKLE_DIVISOR: f64 = 350.0;
const CRACKLE_OFFSET: f64 = 0.00776652;
const CRACKLE_MAX: f32 = 0.20;
/// The crackle recording plays at this multiple of the crackle amount…
const CRACKLE_PLAYBACK_GAIN: f32 = 2.0;
/// …and the voice is turned down by this multiple of it.
const CRACKLE_VOICE_DUCKING: f64 = 3.7;
/// Weakest carrier for the AM model, so that even a signal at the edge of range takes part.
const CARRIER_LEVEL_FLOOR: f32 = 0.01;

/// afv-native plays HF voice at this gain; the HF background levels are set relative to it.
pub(crate) const HF_VOICE_GAIN: f32 = 0.20;

/// Which side(s) of a stereo output a radio plays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Pan {
    #[default]
    Both,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RadioSettings {
    /// Hz; 0 receives nothing.
    pub frequency: u32,
    /// Output gain, linear.
    pub gain: f32,
    /// Simulate radio effects; `false` plays voice clean.
    pub effects: bool,
    /// HF radios have no squelch, so their background (noise, static, sounders) plays all the
    /// time; this silences it, and adds the squelch click at the end of a transmission.
    pub hf_squelch: bool,
    pub hardware: Hardware,
    pub muted: bool,
    pub pan: Pan,
}

impl Default for RadioSettings {
    fn default() -> Self {
        Self {
            frequency: 0,
            gain: 1.0,
            effects: true,
            hf_squelch: false,
            hardware: Hardware::default(),
            muted: false,
            pan: Pan::Both,
        }
    }
}

/// Reception started or ended on a radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxChange {
    Started,
    Ended,
}

/// One talker's audio for the current frame.
#[derive(Debug, Clone, Copy)]
pub struct Signal<'a> {
    pub audio: &'a Frame,
    /// The receiving transceivers from the talker's latest packet.
    pub transceivers: &'a [RxTransceiver],
    pub carrier: Carrier,
}

/// Carrier amplitude for a `DistanceRatio` (1.0 close, about 0.1 at the edge of range).
fn carrier_level(distance_ratio: f32) -> f32 {
    // `max`/`min` rather than `clamp` so that NaN maps to the weakest signal.
    #[allow(clippy::manual_clamp)]
    distance_ratio.max(CARRIER_LEVEL_FLOOR).min(1.0)
}

/// How much crackle a signal of this `DistanceRatio` gets (0 at full strength, up to [`CRACKLE_MAX`]).
fn weak_signal_crackle(distance_ratio: f32) -> f32 {
    let d = distance_ratio as f64;
    let crackle = ((d.exp() * d.powf(CRACKLE_POWER) / CRACKLE_DIVISOR) - CRACKLE_OFFSET) as f32;
    // fmax/fmin semantics as in the original: NaN maps to the bound, unlike `clamp`.
    #[allow(clippy::manual_clamp)]
    crackle.max(0.0).min(CRACKLE_MAX)
}

/// Where a radio's processed audio goes.
pub enum Output<'a> {
    Stereo { left: &'a mut Frame, right: &'a mut Frame },
    Mono(&'a mut Frame),
}

pub struct Radio {
    settings: RadioSettings,
    transmitting: bool,
    filter: HardwareFilter,
    filter_needs_reset: bool,
    compressor: Compressor,
    click: Option<SamplePlayer>,
    crackle: Option<SamplePlayer>,
    vhf_noise: Option<SamplePlayer>,
    hf_noise: Option<SamplePlayer>,
    ac_bus: Option<SamplePlayer>,
    am: AmChannel,
    hf_conditions: HfConditions,
    /// HF noise floor, static and sounders (distinct from `crackle`, afv-native's weak-signal
    /// VHF recording).
    atmospherics: Atmospherics,
    /// The HF receiver's sideband filter, applied to voice and static alike.
    ssb: SsbFilter,
    rx_count: u32,
    channel: Box<Frame>,
    fetch: Box<Frame>,
}

fn mix(dst: &mut Frame, src: &Frame, gain: f32) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += gain * s;
    }
}

impl Radio {
    pub fn new(settings: RadioSettings) -> Self {
        let seed = settings.frequency as u64;
        Self {
            filter: HardwareFilter::new(settings.hardware),
            settings,
            transmitting: false,
            filter_needs_reset: false,
            compressor: Compressor::default_for_rate(SAMPLE_RATE),
            click: None,
            crackle: None,
            vhf_noise: None,
            hf_noise: None,
            ac_bus: None,
            am: AmChannel::new(),
            hf_conditions: HfConditions::default(),
            atmospherics: Atmospherics::new(seed),
            ssb: SsbFilter::default(),
            rx_count: 0,
            channel: Box::new([0.0; FRAME_SAMPLES]),
            fetch: Box::new([0.0; FRAME_SAMPLES]),
        }
    }

    pub fn settings(&self) -> &RadioSettings {
        &self.settings
    }

    /// Applies new settings. A frequency change stops all effects except a pending click.
    /// A hardware change allocates a new filter.
    pub fn set_settings(&mut self, settings: RadioSettings) {
        if settings.frequency != self.settings.frequency {
            self.reset_effects(true);
        }
        if settings.hardware != self.settings.hardware {
            self.filter = HardwareFilter::new(settings.hardware);
        }
        self.settings = settings;
    }

    /// HF propagation; the radio adds the static from it while receiving on HF.
    pub fn set_hf_conditions(&mut self, conditions: HfConditions) {
        self.hf_conditions = conditions;
    }

    /// While transmitting, the radio outputs nothing it receives.
    pub fn set_transmitting(&mut self, transmitting: bool) {
        self.transmitting = transmitting;
    }

    pub fn transmitting(&self) -> bool {
        self.transmitting
    }

    /// Number of streams received in the last processed frame.
    pub fn receiving(&self) -> u32 {
        self.rx_count
    }

    fn reset_effects(&mut self, keep_click: bool) {
        if !keep_click {
            self.click = None;
            self.rx_count = 0;
        }
        self.crackle = None;
        self.atmospherics.reset();
        self.ssb.reset();
        self.vhf_noise = None;
        self.hf_noise = None;
        self.ac_bus = None;
        self.filter_needs_reset = true;
    }

    fn mix_effect(player: &mut Option<SamplePlayer>, gain: f32, channel: &mut Frame, fetch: &mut Frame) {
        if gain <= 0.0 {
            return;
        }
        if let Some(p) = player {
            if p.fill(fetch) {
                mix(channel, fetch, gain);
            } else {
                *player = None;
            }
        }
    }

    /// Processes one frame of the active talkers' `signals`. The result is added to `output`.
    ///
    /// On VHF, two or more talkers heard at once are combined as AM carriers that beat against
    /// each other (see [`AmChannel`]); HF is single sideband, so there they simply add up.
    pub fn process<'a>(
        &mut self,
        signals: impl Iterator<Item = Signal<'a>>,
        effects: &EffectSamples,
        output: Output<'_>,
    ) -> Option<RxChange> {
        let RadioSettings { frequency, gain, effects: fx_on, hf_squelch, muted, .. } = self.settings;
        let ignore = self.transmitting;
        if ignore {
            self.reset_effects(true);
        }
        self.channel.fill(0.0);

        let hf = frequency < HF_UPPER_LIMIT_HZ;
        let audible = !ignore && !muted;
        let (mut crackle_gain, mut hf_gain, mut vhf_gain, mut ac_gain) = (0f32, 0f32, 0f32, 0f32);
        let mut count = 0;
        self.am.begin();
        for signal in signals {
            // The strongest of our transceivers that hears this talker on this frequency.
            let Some(ratio) = signal
                .transceivers
                .iter()
                .filter(|t| t.frequency == frequency)
                .map(|t| t.distance_ratio)
                .reduce(f32::max)
            else {
                continue;
            };
            let mut voice_gain = 1.0f32;
            if fx_on {
                let crackle = weak_signal_crackle(ratio);
                if hf {
                    hf_gain = if hf_squelch { 0.0 } else { HF_NOISE_GAIN };
                    vhf_gain = 0.0;
                    ac_gain = HF_AC_BUS_GAIN;
                    voice_gain = HF_VOICE_GAIN;
                } else {
                    hf_gain = 0.0;
                    vhf_gain = VHF_NOISE_GAIN;
                    ac_gain = AC_BUS_GAIN;
                    crackle_gain += crackle * CRACKLE_PLAYBACK_GAIN;
                    voice_gain = (1.0 - crackle as f64 * CRACKLE_VOICE_DUCKING) as f32;
                }
            }
            if audible {
                mix(&mut self.channel, signal.audio, voice_gain * gain);
                if !hf {
                    self.am.add(signal.audio, voice_gain, carrier_level(ratio), signal.carrier);
                }
            }
            count += 1;
        }
        if audible && !hf && count > 1 {
            self.am.detect(&mut self.channel);
            self.channel.iter_mut().for_each(|x| *x *= gain);
        } else {
            self.am.idle();
        }
        // HF sets have no squelch: unless `hf_squelch` asks for one, the receiver's background
        // plays whether or not anyone is transmitting (skipped while it cannot be heard anyway).
        let unsquelched = hf && fx_on && !hf_squelch;
        let open_hf = unsquelched && audible;
        if open_hf {
            (hf_gain, vhf_gain, ac_gain) = (HF_NOISE_GAIN, 0.0, HF_AC_BUS_GAIN);
        }

        let mut change = None;
        if count > 0 && self.rx_count == 0 && !ignore {
            change = Some(RxChange::Started);
        }
        if count > 0 || open_hf {
            if fx_on {
                if hf {
                    // What an SSB receiver hears: noise, static and sounders added to the signal,
                    // all through the sideband filter, ahead of the rest of the audio chain.
                    if hf_gain > 0.0 {
                        self.fetch.fill(0.0);
                        self.atmospherics.add(&self.hf_conditions, &mut self.fetch);
                        mix(&mut self.channel, &self.fetch, gain);
                    }
                    self.ssb.process(&mut self.channel[..]);
                }
                self.channel.iter_mut().for_each(|x| *x = x.clamp(-1.0, 1.0));
                let start = |p: &mut Option<SamplePlayer>, samples: &Option<crate::effects::Samples>| {
                    if p.is_none() {
                        *p = samples.clone().map(|s| SamplePlayer::new(s, true));
                    }
                };
                start(&mut self.vhf_noise, &effects.vhf_noise);
                start(&mut self.hf_noise, &effects.hf_noise);
                start(&mut self.crackle, &effects.crackle);
                start(&mut self.ac_bus, &effects.ac_bus);
                if std::mem::take(&mut self.filter_needs_reset) {
                    self.filter.reset();
                }
                self.filter.process(&mut self.channel[..]);
                self.compressor.process(&mut self.channel[..]);
                Self::mix_effect(&mut self.crackle, crackle_gain * gain, &mut self.channel, &mut self.fetch);
                Self::mix_effect(&mut self.hf_noise, hf_gain * gain, &mut self.channel, &mut self.fetch);
                Self::mix_effect(&mut self.vhf_noise, vhf_gain * gain, &mut self.channel, &mut self.fetch);
                Self::mix_effect(&mut self.ac_bus, ac_gain * gain, &mut self.channel, &mut self.fetch);
            }
        } else {
            self.reset_effects(true);
        }
        if count == 0 && self.rx_count > 0 {
            // The squelch closing; an HF receiver without one has nothing to click.
            if !unsquelched {
                self.click = effects.click.clone().map(|s| SamplePlayer::new(s, false));
            }
            change = Some(RxChange::Ended);
        }
        self.rx_count = count;
        Self::mix_effect(&mut self.click, CLICK_GAIN * gain, &mut self.channel, &mut self.fetch);

        if !ignore && !muted {
            match output {
                Output::Mono(out) => mix(out, &self.channel, 1.0),
                Output::Stereo { left, right } => {
                    if self.settings.pan != Pan::Right {
                        mix(left, &self.channel, 1.0);
                    }
                    if self.settings.pan != Pan::Left {
                        mix(right, &self.channel, 1.0);
                    }
                }
            }
        }
        change
    }
}
