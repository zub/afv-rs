//! Opus encoder and decoder configured for AFV: 48 kHz mono, VoIP mode, 16 kbit/s.

use afv_proto::consts::OPUS_BITRATE;

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

pub use opus::Error as OpusError;

/// Largest Opus packet (RFC 6716 §3.4).
pub const MAX_PACKET: usize = 1275;

pub struct Encoder {
    inner: opus::Encoder,
    buffer: Box<[u8; MAX_PACKET]>,
}

impl Encoder {
    pub fn new() -> Result<Self, OpusError> {
        let mut inner = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        inner.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))?;
        Ok(Self { inner, buffer: Box::new([0; MAX_PACKET]) })
    }

    /// Encodes one frame. The returned slice is valid until the next call.
    pub fn encode(&mut self, frame: &Frame) -> Result<&[u8], OpusError> {
        let len = self.inner.encode_float(frame, &mut self.buffer[..])?;
        Ok(&self.buffer[..len])
    }

    pub fn reset(&mut self) -> Result<(), OpusError> {
        self.inner.reset_state()
    }
}

pub struct Decoder {
    inner: opus::Decoder,
}

impl Decoder {
    pub fn new() -> Result<Self, OpusError> {
        Ok(Self { inner: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)? })
    }

    /// Decodes one packet into `frame`.
    pub fn decode(&mut self, packet: &[u8], frame: &mut Frame) -> Result<(), OpusError> {
        let n = self.inner.decode_float(packet, frame, false)?;
        frame[n.min(FRAME_SAMPLES)..].fill(0.0);
        Ok(())
    }

    /// Fills `frame` with packet-loss concealment.
    pub fn conceal(&mut self, frame: &mut Frame) -> Result<(), OpusError> {
        let n = self.inner.decode_float(&[], frame, false)?;
        frame[n.min(FRAME_SAMPLES)..].fill(0.0);
        Ok(())
    }

    pub fn reset(&mut self) -> Result<(), OpusError> {
        self.inner.reset_state()
    }
}
