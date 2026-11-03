//! Audio processing for Audio for VATSIM (AFV), with no devices and no networking.
//!
//! Everything works on 20 ms mono frames at 48 kHz ([`Frame`]):
//!
//! - [`Transmitter`]: microphone frames to Opus packets for `AT` messages.
//! - [`Bus`]: received `AR` packets to a mixed output frame, through per-talker jitter buffers
//!   and decoders and the per-radio [`Radio`] simulation (static, noise, hardware EQ,
//!   simultaneous transmissions beating against each other, HF propagation, squelch click).
//! - [`dsp`]: the filters and dynamics processors, ported from afv-native.
//!
//! The radio simulation is a port of afv-native (BSD-3-Clause, (c) Christopher Collins), including
//! its vendored ChunkWare (MIT) and sndfilter (MIT) code.

pub mod bus;
pub mod codec;
pub mod convert;
pub mod dsp;
pub mod effects;
pub mod jitter;
pub mod radio;
pub mod stream;
#[cfg(test)]
mod test_util;
pub mod tx;
#[cfg(feature = "wav")]
pub mod wav;

pub use bus::{Bus, RadioId};
pub use codec::OpusError;
pub use convert::{CaptureConverter, ConvertError, RenderConverter};
pub use dsp::{Carrier, Hardware, HfConditions};
pub use effects::{EffectSamples, SamplePlayer, Samples};
pub use radio::{Output, Pan, Radio, RadioSettings, RxChange, Signal};
pub use tx::{Encoded, LevelMeter, MicProcessor, Transmitter};

pub const SAMPLE_RATE: u32 = afv_proto::consts::SAMPLE_RATE_HZ;
pub const FRAME_SAMPLES: usize = afv_proto::consts::FRAME_SAMPLES;
pub(crate) const FRAME_DURATION_MS: f64 = 20.0;

/// One 20 ms mono frame at 48 kHz.
pub type Frame = [f32; FRAME_SAMPLES];
