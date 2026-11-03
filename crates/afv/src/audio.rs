//! The audio engine: two output buses and the transmitter, connected to the session.
//!
//! [`Audio`] is driven by whoever owns the audio devices: call [`Audio::capture`] with microphone
//! samples and [`Audio::render`] for each output, in buffers of any size, at any sample rate.
//! Both lock the engine briefly and do the decoding, mixing and encoding inline, so call them
//! from an ordinary thread that is fed by the device callbacks (the `cpal` backend does this),
//! not from a real-time callback directly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use afv_audio::bus::STREAM_IDLE_TIMEOUT;
use afv_audio::{
    Bus, CaptureConverter, ConvertError, EffectSamples, HfConditions, MicProcessor, RenderConverter, RxChange, Samples,
    Transmitter,
};
use afv_proto::voice::AudioRx;
use tokio::sync::{broadcast, mpsc};

use crate::events::Event;
use crate::radio::{OutputBus, RadioConfig, RadioId};
use crate::session::Shared;

const PURGE_INTERVAL: Duration = Duration::from_secs(5);

/// Handle to the audio engine of a [`crate::Client`]. Cheap to clone.
#[derive(Clone)]
pub struct Audio {
    engine: Arc<Mutex<Engine>>,
}

struct Engine {
    shared: Arc<Shared>,
    events: broadcast::Sender<Event>,
    incoming: Option<mpsc::Receiver<AudioRx>>,
    headset: Bus,
    speaker: Bus,
    transmitter: Transmitter,
    /// Transceivers to transmit on, as last acknowledged by the server.
    tx: Vec<u16>,
    /// Radios with `tx` enabled, which stop playing received audio while transmitting.
    tx_radios: Vec<RadioId>,
    /// Which bus each receiving radio is on.
    rx_radios: HashMap<RadioId, OutputBus>,
    capture: Option<(u32, u16, CaptureConverter)>,
    render_headset: Option<(u32, u16, RenderConverter)>,
    render_speaker: Option<(u32, u16, RenderConverter)>,
    last_purge: Instant,
}

/// Recreates `slot`'s converter when the format changes.
fn converter<C>(
    slot: &mut Option<(u32, u16, C)>,
    rate: u32,
    channels: u16,
    new: impl FnOnce(u32, u16) -> Result<C, ConvertError>,
) -> Result<&mut C, ConvertError> {
    if !matches!(slot, Some((r, c, _)) if *r == rate && *c == channels) {
        *slot = Some((rate, channels, new(rate, channels)?));
    }
    Ok(&mut slot.as_mut().expect("just set").2)
}

impl Audio {
    pub(crate) fn new(
        shared: Arc<Shared>,
        events: broadcast::Sender<Event>,
        effects: EffectSamples,
    ) -> Result<Self, afv_audio::OpusError> {
        let incoming = shared.take_incoming_audio();
        let engine = Engine {
            shared,
            events,
            incoming,
            headset: Bus::new(effects.clone()),
            speaker: Bus::new(effects),
            transmitter: Transmitter::new()?,
            tx: Vec::new(),
            tx_radios: Vec::new(),
            rx_radios: HashMap::new(),
            capture: None,
            render_headset: None,
            render_speaker: None,
            last_purge: Instant::now(),
        };
        Ok(Self { engine: Arc::new(Mutex::new(engine)) })
    }

    fn lock(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Feeds microphone audio: interleaved samples at `sample_rate` with `channels` channels,
    /// in buffers of any size. Every complete 20 ms frame advances the transmit sequence and,
    /// while PTT is open, is encoded and sent.
    pub fn capture(&self, samples: &[f32], sample_rate: u32, channels: u16) -> Result<(), ConvertError> {
        let engine = &mut *self.lock();
        let converter = converter(&mut engine.capture, sample_rate, channels, CaptureConverter::new)?;
        let (transmitter, shared, tx) = (&mut engine.transmitter, &engine.shared, &engine.tx);
        converter.push(samples, |frame| match transmitter.process(frame) {
            Ok(Some(packet)) if !tx.is_empty() => {
                if let Err(e) = shared.send_audio(packet.sequence, packet.opus, packet.last_packet, tx) {
                    tracing::debug!("audio not sent: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("encoding failed: {e}"),
        });
        Ok(())
    }

    /// Fills `out` (interleaved, `channels` wide, at `sample_rate`) with the next audio of `bus`.
    pub fn render(&self, bus: OutputBus, out: &mut [f32], sample_rate: u32, channels: u16) -> Result<(), ConvertError> {
        let engine = &mut *self.lock();
        engine.receive();
        let slot = match bus {
            OutputBus::Headset => &mut engine.render_headset,
            OutputBus::Speaker => &mut engine.render_speaker,
        };
        let converter = converter(slot, sample_rate, channels, RenderConverter::new)?;
        let target = match bus {
            OutputBus::Headset => &mut engine.headset,
            OutputBus::Speaker => &mut engine.speaker,
        };
        let events = &engine.events;
        converter.fill(out, |left, right| {
            target.render(left, right, |radio, callsign, change| {
                let (radio, callsign) = (RadioId(radio), callsign.to_owned());
                let _ = events.send(match change {
                    RxChange::Started => Event::RxStarted { radio, callsign },
                    RxChange::Ended => Event::RxEnded { radio, callsign },
                });
            })
        });
        Ok(())
    }

    /// Microphone gain, linear (default 1). The signal is clipped to ±1 after it.
    pub fn set_mic_gain(&self, gain: f32) {
        self.lock().transmitter.set_gain(gain);
    }

    /// Microphone level over the last 300 ms, as `(rms, peak)` in 0‥1.
    pub fn mic_level(&self) -> (f64, f64) {
        let engine = self.lock();
        let meter = engine.transmitter.meter();
        (meter.level(), meter.peak())
    }

    /// Processing applied to the microphone before the gain, e.g. noise suppression.
    pub fn set_mic_processor(&self, processor: Option<Box<dyn MicProcessor>>) {
        self.lock().transmitter.set_processor(processor);
    }

    /// Automatic gain control on each received station (default: on, −18 dBFS).
    pub fn set_agc(&self, enabled: bool, target_db: f64) {
        let mut engine = self.lock();
        engine.headset.set_agc(enabled, target_db);
        engine.speaker.set_agc(enabled, target_db);
    }

    /// How HF propagation sounds: mistuning, fading, noise, static and sounders (default
    /// [`HfConditions::FAIR`]). Applies to HF radios with `effects` enabled, from the next frame.
    pub fn set_hf_conditions(&self, conditions: HfConditions) {
        let mut engine = self.lock();
        engine.headset.set_hf_conditions(conditions);
        engine.speaker.set_hf_conditions(conditions);
    }

    pub fn hf_conditions(&self) -> HfConditions {
        self.lock().headset.hf_conditions()
    }

    /// Plays a sound once on `bus`.
    pub fn play_sound(&self, bus: OutputBus, samples: Samples, gain: f32) {
        self.lock().bus(bus).play_sound(samples, gain);
    }

    #[cfg(feature = "cpal")]
    pub(crate) fn emit(&self, event: Event) {
        let _ = self.lock().events.send(event);
    }

    /// Applies the radio configurations to the buses.
    pub(crate) fn set_radios(&self, radios: &[(RadioId, RadioConfig)]) {
        let mut engine = self.lock();
        let transmitting = engine.transmitter.ptt();
        let wanted: HashMap<RadioId, OutputBus> =
            radios.iter().filter(|(_, r)| r.rx).map(|(id, r)| (*id, r.output.bus)).collect();
        let stale: Vec<_> = engine
            .rx_radios
            .iter()
            .filter(|(id, bus)| wanted.get(id) != Some(bus))
            .map(|(id, bus)| (*id, *bus))
            .collect();
        for (id, bus) in stale {
            engine.bus(bus).remove_radio(id.0);
        }
        for (id, radio) in radios.iter().filter(|(_, r)| r.rx) {
            let bus = engine.bus(radio.output.bus);
            bus.set_radio(id.0, radio.audio_settings());
            bus.set_transmitting(id.0, transmitting && radio.tx);
        }
        engine.rx_radios = wanted;
        engine.tx_radios = radios.iter().filter(|(_, r)| r.tx).map(|(id, _)| *id).collect();
    }

    /// Opens or closes transmission. Closing sends a final packet with the next captured frame.
    pub(crate) fn set_ptt(&self, open: bool) {
        let mut engine = self.lock();
        engine.transmitter.set_ptt(open);
        for id in engine.tx_radios.clone() {
            if let Some(&bus) = engine.rx_radios.get(&id) {
                engine.bus(bus).set_transmitting(id.0, open);
            }
        }
    }

    /// The transceivers to transmit on.
    pub(crate) fn set_tx_transceivers(&self, ids: &[u16]) {
        let mut engine = self.lock();
        engine.tx.clear();
        engine.tx.extend_from_slice(ids);
    }
}

impl Engine {
    fn bus(&mut self, bus: OutputBus) -> &mut Bus {
        match bus {
            OutputBus::Headset => &mut self.headset,
            OutputBus::Speaker => &mut self.speaker,
        }
    }

    /// Moves received packets into the buses whose radios can hear them.
    fn receive(&mut self) {
        let now = Instant::now();
        if let Some(incoming) = &mut self.incoming {
            while let Ok(packet) = incoming.try_recv() {
                for bus in [&mut self.headset, &mut self.speaker] {
                    if bus.hears(&packet.transceivers)
                        && let Err(e) = bus.push(
                            &packet.callsign,
                            packet.sequence,
                            &packet.audio,
                            packet.last_packet,
                            &packet.transceivers,
                            now,
                        )
                    {
                        tracing::warn!("cannot decode {}: {e}", packet.callsign);
                    }
                }
            }
        }
        if now.duration_since(self.last_purge) > PURGE_INTERVAL {
            self.last_purge = now;
            self.headset.purge_idle(now, STREAM_IDLE_TIMEOUT);
            self.speaker.purge_idle(now, STREAM_IDLE_TIMEOUT);
        }
    }
}
