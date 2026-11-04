//! Transmit side: microphone frames to Opus packets. Port of the transmit half of afv-native's
//! radio simulations.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::codec::{Encoder, OpusError};
use crate::{FRAME_SAMPLES, Frame};

/// Optional microphone processing (noise suppression, AGC, ...) run before gain and encoding.
///
/// A processor that panics is removed, and the microphone goes out unprocessed from that frame
/// on (see [`Transmitter::take_processor_failure`]). This needs `panic = "unwind"`, the default.
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
    processor_failed: bool,
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
            processor_failed: false,
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

    /// Whether the processor panicked and was removed since this was last asked.
    pub fn take_processor_failure(&mut self) -> bool {
        std::mem::take(&mut self.processor_failed)
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
            // Caller code: a panic must not take the transmitter, or the audio thread running
            // it, down. The processor's state is unknown afterwards, so it goes, and this frame
            // is sent as captured.
            let frame = &mut *self.frame;
            if catch_unwind(AssertUnwindSafe(|| p.process(frame))).is_err() {
                let processor = self.processor.take();
                let _ = catch_unwind(AssertUnwindSafe(|| drop(processor)));
                self.processor_failed = true;
                self.frame.copy_from_slice(mic);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Halves the signal, and panics on its third frame.
    struct Flaky(u32);

    impl MicProcessor for Flaky {
        fn process(&mut self, frame: &mut Frame) {
            self.0 += 1;
            frame.iter_mut().for_each(|s| *s *= 0.5);
            assert!(self.0 != 3, "processor bug");
        }
    }

    #[test]
    fn a_panicking_processor_is_removed_and_the_microphone_goes_out_unprocessed() {
        let mut tx = Transmitter::new().unwrap();
        tx.set_processor(Some(Box::new(Flaky(0))));
        tx.set_ptt(true);
        let mic = [0.4; FRAME_SAMPLES];
        for frame in 1..=5 {
            assert!(tx.process(&mic).unwrap().is_some(), "frame {frame} is sent");
            let expected = if frame < 3 { 0.2 } else { 0.4 };
            assert!(tx.last_frame().iter().all(|&s| s == expected), "frame {frame}");
            assert_eq!(tx.take_processor_failure(), frame == 3, "frame {frame}");
        }
    }
}
