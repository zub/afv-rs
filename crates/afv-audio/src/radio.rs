//! Receive-side simulation of one radio. Port of afv-native's `ATCRadioSimulation::_process_radio`
//! (the pilot `RadioSimulation` behaves the same for a single matching transceiver), except that
//! simultaneous transmissions on VHF are rendered by [`AmChannel`] instead of a fixed tone.

use afv_proto::consts::HF_UPPER_LIMIT_HZ;
use afv_proto::voice::RxTransceiver;

use crate::dsp::{AmChannel, Carrier, Compressor, Hardware, HardwareFilter};
use crate::effects::{EffectSamples, SamplePlayer};
use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

const CLICK_GAIN: f32 = 1.3;
const AC_BUS_GAIN: f32 = 0.005;
const HF_AC_BUS_GAIN: f32 = 0.001;
const VHF_NOISE_GAIN: f32 = 0.17;
const HF_NOISE_GAIN: f32 = 0.16;
const HF_VOICE_GAIN: f32 = 0.20;

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
    /// Suppress the HF background noise.
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
    distance_ratio.max(0.01).min(1.0)
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

    /// While transmitting, the radio outputs nothing it receives.
    pub fn set_transmitting(&mut self, transmitting: bool) {
        self.transmitting = transmitting;
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
                let d = ratio as f64;
                let crackle = ((d.exp() * d.powf(-4.0) / 350.0) - 0.00776652) as f32;
                // fmax/fmin semantics as in the original: NaN maps to the bound, unlike `clamp`.
                #[allow(clippy::manual_clamp)]
                let crackle = crackle.max(0.0).min(0.20);
                if hf {
                    hf_gain = if hf_squelch { 0.0 } else { HF_NOISE_GAIN };
                    vhf_gain = 0.0;
                    ac_gain = HF_AC_BUS_GAIN;
                    voice_gain = HF_VOICE_GAIN;
                } else {
                    hf_gain = 0.0;
                    vhf_gain = VHF_NOISE_GAIN;
                    ac_gain = AC_BUS_GAIN;
                    crackle_gain += crackle * 2.0;
                    voice_gain = (1.0 - crackle as f64 * 3.7) as f32;
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

        let mut change = None;
        if count > 0 {
            if self.rx_count == 0 && !ignore {
                change = Some(RxChange::Started);
            }
            if fx_on {
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
            if self.rx_count > 0 {
                self.click = effects.click.clone().map(|s| SamplePlayer::new(s, false));
                change = Some(RxChange::Ended);
            }
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
