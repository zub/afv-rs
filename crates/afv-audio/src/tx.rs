//! Transmit side: microphone frames to Opus packets. Port of the transmit half of afv-native's
//! radio simulations.

use crate::codec::{Encoder, OpusError};
use crate::{FRAME_SAMPLES, Frame};

/// Optional microphone processing (noise suppression, AGC, ...) run before gain and encoding.
pub trait MicProcessor: Send {
    fn process(&mut self, frame: &mut Frame);
}

/// One encoded frame ready to be sent as an `AT` packet.
#[derive(Debug, PartialEq, Eq)]
pub struct Encoded<'a> {
    pub sequence: u32,
    pub opus: &'a [u8],
    /// Set on the frame after PTT was released.
    pub last_packet: bool,
}

pub struct Transmitter {
    encoder: Encoder,
    processor: Option<Box<dyn MicProcessor>>,
    gain: f32,
    ptt: bool,
    /// The previous frame was sent with PTT pressed, so one final frame is still due.
    sending: bool,
    sequence: u32,
    frame: Box<Frame>,
    meter: LevelMeter,
}

impl Transmitter {
    pub fn new() -> Result<Self, OpusError> {
        Ok(Self {
            encoder: Encoder::new()?,
            processor: None,
            gain: 1.0,
            ptt: false,
            sending: false,
            sequence: 0,
            frame: Box::new([0.0; FRAME_SAMPLES]),
            meter: LevelMeter::new(),
        })
    }

    pub fn set_processor(&mut self, processor: Option<Box<dyn MicProcessor>>) {
        self.processor = processor;
    }

    /// Linear microphone gain.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain;
    }

    pub fn set_ptt(&mut self, ptt: bool) {
        self.ptt = ptt;
    }

    pub fn ptt(&self) -> bool {
        self.ptt
    }

    pub fn meter(&self) -> &LevelMeter {
        &self.meter
    }

    /// The most recent microphone frame after processing and gain (for sidetone).
    pub fn last_frame(&self) -> &Frame {
        &self.frame
    }

    /// Feeds one 20 ms microphone frame. Every frame advances the sequence counter, whether
    /// or not anything is sent; returns the packet to send while PTT is pressed and for one
    /// frame after it is released.
    pub fn process(&mut self, mic: &Frame) -> Result<Option<Encoded<'_>>, OpusError> {
        self.frame.copy_from_slice(mic);
        if let Some(p) = &mut self.processor {
            p.process(&mut self.frame);
        }
        for s in self.frame.iter_mut() {
            *s = (*s * self.gain).clamp(-1.0, 1.0);
        }
        self.meter.add(&self.frame);

        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        if !self.ptt && !self.sending {
            return Ok(None);
        }
        let last_packet = !self.ptt;
        self.sending = self.ptt;
        let opus = self.encoder.encode(&self.frame)?;
        Ok(Some(Encoded { sequence, opus, last_packet }))
    }
}

/// Microphone level for a VU meter, on a 0–1 scale covering −40 to 0 dBFS peak, averaged over
/// the last 300 ms. Readings below 0.3 (−28 dBFS) show as 0.
#[derive(Debug, Clone)]
pub struct LevelMeter {
    window: [f32; Self::WINDOW],
    next: usize,
}

impl LevelMeter {
    /// Frames averaged: 300 ms.
    const WINDOW: usize = 15;
    /// The bottom of the scale, in dBFS peak…
    const FLOOR_DB: f32 = -40.0;
    /// …and readings below this fraction of it show as 0, so that background noise does not move
    /// the meter.
    const CUTOFF: f32 = 0.30;

    fn new() -> Self {
        Self { window: [0.0; Self::WINDOW], next: 0 }
    }

    fn add(&mut self, frame: &Frame) {
        let peak = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
        let db = (20.0 * peak.log10()).clamp(Self::FLOOR_DB, 0.0);
        let ratio = (db - Self::FLOOR_DB) / -Self::FLOOR_DB;
        self.window[self.next] = if ratio < Self::CUTOFF { 0.0 } else { ratio };
        self.next = (self.next + 1) % Self::WINDOW;
    }

    /// Average level.
    pub fn level(&self) -> f32 {
        self.window.iter().sum::<f32>() / Self::WINDOW as f32
    }

    /// Highest level in the window.
    pub fn peak(&self) -> f32 {
        self.window.iter().copied().fold(0.0, f32::max)
    }
}
