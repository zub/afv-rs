//! An output bus (e.g. headset or speaker): the remote talkers, the radios routed to this bus,
//! and one-shot sounds, rendered one 20 ms frame at a time.
//!
//! Each bus keeps its own decoders because buses are usually driven by different audio devices
//! with independent clocks, exactly as afv-native keeps separate headset and speaker streams.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use afv_proto::consts::HF_UPPER_LIMIT_HZ;
use afv_proto::voice::RxTransceiver;

use crate::codec::OpusError;
use crate::dsp::{Carrier, HfChannel, HfConditions};
use crate::effects::{EffectSamples, SamplePlayer, Samples};
use crate::radio::{Output, Radio, RadioSettings, RxChange, Signal};
use crate::stream::RemoteStream;
use crate::{FRAME_SAMPLES, Frame};

/// Caller-chosen radio identifier.
pub type RadioId = u32;

/// Talkers silent for this long are forgotten.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

struct Talker {
    stream: RemoteStream,
    carrier: Carrier,
    frame: Box<Frame>,
    has_frame: bool,
    /// Propagation applied when the talker transmits on HF, and its result for this frame.
    hf: HfChannel,
    hf_frame: Box<Frame>,
    on_hf: bool,
    /// Radios on which this talker was heard in the last rendered frame.
    heard_on: Vec<RadioId>,
}

pub struct Bus {
    effects: EffectSamples,
    talkers: HashMap<String, Talker>,
    radios: Vec<(RadioId, Radio)>,
    sounds: Vec<(SamplePlayer, f32)>,
    fetch: Box<Frame>,
    agc_enabled: bool,
    agc_target_db: f64,
    hf_conditions: HfConditions,
}

impl Bus {
    pub fn new(effects: EffectSamples) -> Self {
        Self {
            effects,
            talkers: HashMap::new(),
            radios: Vec::new(),
            sounds: Vec::new(),
            fetch: Box::new([0.0; FRAME_SAMPLES]),
            agc_enabled: true,
            agc_target_db: -18.0,
            hf_conditions: HfConditions::default(),
        }
    }

    /// Adds a radio, or updates it if `id` exists.
    pub fn set_radio(&mut self, id: RadioId, settings: RadioSettings) {
        match self.radios.iter_mut().find(|(i, _)| *i == id) {
            Some((_, radio)) => radio.set_settings(settings),
            None => {
                let mut radio = Radio::new(settings);
                radio.set_hf_conditions(self.hf_conditions);
                self.radios.push((id, radio));
            }
        }
    }

    pub fn remove_radio(&mut self, id: RadioId) {
        self.radios.retain(|(i, _)| *i != id);
        for t in self.talkers.values_mut() {
            t.heard_on.retain(|&r| r != id);
        }
    }

    pub fn radio(&self, id: RadioId) -> Option<&Radio> {
        self.radios.iter().find(|(i, _)| *i == id).map(|(_, r)| r)
    }

    pub fn set_transmitting(&mut self, id: RadioId, transmitting: bool) {
        if let Some((_, radio)) = self.radios.iter_mut().find(|(i, _)| *i == id) {
            radio.set_transmitting(transmitting);
        }
    }

    /// Per-talker AGC; applies to existing and future talkers.
    pub fn set_agc(&mut self, enabled: bool, target_db: f64) {
        self.agc_enabled = enabled;
        self.agc_target_db = target_db;
        for t in self.talkers.values_mut() {
            t.stream.agc_mut().set_target_db(target_db);
            t.stream.agc_mut().set_enabled(enabled);
        }
    }

    /// How HF propagation sounds on radios with effects enabled (default: [`HfConditions::FAIR`]).
    pub fn set_hf_conditions(&mut self, conditions: HfConditions) {
        self.hf_conditions = conditions;
        for (_, radio) in &mut self.radios {
            radio.set_hf_conditions(conditions);
        }
    }

    pub fn hf_conditions(&self) -> HfConditions {
        self.hf_conditions
    }

    /// Plays a sound once on both channels.
    pub fn play_sound(&mut self, samples: Samples, gain: f32) {
        self.sounds.push((SamplePlayer::new(samples, false), gain));
    }

    pub fn stop_sounds(&mut self) {
        self.sounds.clear();
    }

    /// Adds a received `AR` frame. Creating a decoder for a new talker can fail.
    pub fn push(
        &mut self,
        callsign: &str,
        sequence: u32,
        opus: &[u8],
        last: bool,
        transceivers: &[RxTransceiver],
        now: Instant,
    ) -> Result<(), OpusError> {
        let talker = match self.talkers.get_mut(callsign) {
            Some(t) => t,
            None => {
                let mut stream = RemoteStream::new()?;
                stream.agc_mut().set_target_db(self.agc_target_db);
                stream.agc_mut().set_enabled(self.agc_enabled);
                let carrier = Carrier::for_callsign(callsign);
                let talker = Talker {
                    stream,
                    carrier,
                    frame: Box::new([0.0; FRAME_SAMPLES]),
                    has_frame: false,
                    hf: HfChannel::for_callsign(callsign),
                    hf_frame: Box::new([0.0; FRAME_SAMPLES]),
                    on_hf: false,
                    heard_on: Vec::with_capacity(4),
                };
                self.talkers.entry(callsign.to_owned()).or_insert(talker)
            }
        };
        talker.stream.push(sequence, opus, last, transceivers, now);
        Ok(())
    }

    /// Whether any radio on this bus is tuned to one of `transceivers`' frequencies.
    pub fn hears(&self, transceivers: &[RxTransceiver]) -> bool {
        self.radios.iter().any(|(_, r)| transceivers.iter().any(|t| t.frequency == r.settings().frequency))
    }

    /// Forgets talkers that have been silent longer than `timeout`.
    pub fn purge_idle(&mut self, now: Instant, timeout: Duration) {
        self.talkers
            .retain(|_, t| t.stream.last_packet_at().is_some_and(|at| now.saturating_duration_since(at) <= timeout));
    }

    /// Renders one stereo frame. `on_rx` is told when a talker starts or stops being heard on
    /// a radio.
    pub fn render_stereo(&mut self, left: &mut Frame, right: &mut Frame, on_rx: impl FnMut(RadioId, &str, RxChange)) {
        self.render(left, Some(right), on_rx);
    }

    /// Renders one mono frame, ignoring radio panning.
    pub fn render_mono(&mut self, out: &mut Frame, on_rx: impl FnMut(RadioId, &str, RxChange)) {
        self.render(out, None, on_rx);
    }

    /// Renders one frame, mono if `right` is `None`.
    pub fn render(
        &mut self,
        left: &mut Frame,
        mut right: Option<&mut Frame>,
        mut on_rx: impl FnMut(RadioId, &str, RxChange),
    ) {
        left.fill(0.0);
        if let Some(r) = right.as_deref_mut() {
            r.fill(0.0);
        }
        for t in self.talkers.values_mut() {
            t.has_frame = t.stream.is_active() && t.stream.next_frame(&mut t.frame);
            t.on_hf = t.has_frame && t.stream.transceivers().iter().any(|x| x.frequency < HF_UPPER_LIMIT_HZ);
            if t.on_hf {
                t.hf.process(&self.hf_conditions, &t.frame, &mut t.hf_frame);
            }
        }
        let talkers = &self.talkers;
        for (_, radio) in &mut self.radios {
            let settings = radio.settings();
            let hf_effects = settings.effects && settings.frequency < HF_UPPER_LIMIT_HZ;
            let signals = talkers.values().filter(|t| t.has_frame).map(move |t| Signal {
                audio: if hf_effects && t.on_hf { &t.hf_frame } else { &t.frame },
                transceivers: t.stream.transceivers(),
                carrier: t.carrier,
            });
            let output = match right.as_deref_mut() {
                Some(r) => Output::Stereo { left: &mut *left, right: r },
                None => Output::Mono(&mut *left),
            };
            radio.process(signals, &self.effects, output);
        }
        self.report_rx(&mut on_rx);
        self.mix_sounds(left, right);
    }

    /// Compares who is heard on which radio with the previous frame. A talker is heard on a radio
    /// while it is producing audio on the radio's frequency and the radio is not transmitting.
    fn report_rx(&mut self, on_rx: &mut impl FnMut(RadioId, &str, RxChange)) {
        for (callsign, t) in &mut self.talkers {
            for (id, radio) in &self.radios {
                let frequency = radio.settings().frequency;
                let heard = t.has_frame
                    && !radio.transmitting()
                    && t.stream.transceivers().iter().any(|x| x.frequency == frequency);
                match (heard, t.heard_on.iter().position(|r| r == id)) {
                    (true, None) => {
                        t.heard_on.push(*id);
                        on_rx(*id, callsign, RxChange::Started);
                    }
                    (false, Some(i)) => {
                        t.heard_on.swap_remove(i);
                        on_rx(*id, callsign, RxChange::Ended);
                    }
                    _ => {}
                }
            }
        }
    }

    fn mix_sounds(&mut self, left: &mut Frame, right: Option<&mut Frame>) {
        let fetch = &mut self.fetch;
        let mut right = right;
        self.sounds.retain_mut(|(player, gain)| {
            if !player.fill(fetch) {
                return false;
            }
            for (i, s) in fetch.iter().enumerate() {
                left[i] += *gain * s;
                if let Some(r) = right.as_deref_mut() {
                    r[i] += *gain * s;
                }
            }
            true
        });
    }
}
