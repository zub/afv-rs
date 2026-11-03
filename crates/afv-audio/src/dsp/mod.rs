//! Signal-processing building blocks.

mod agc;
mod am;
mod biquad;
mod compressor;
mod hang_agc;
mod hardware;
mod hf;
mod limiter;

pub use agc::RmsAgc;
pub use am::{AmChannel, Carrier};
pub use biquad::{Biquad, Cascade};
pub use compressor::Compressor;
pub use hardware::{HfHardware, RadioSet, VhfHardware};
pub(crate) use hf::Atmospherics;
pub use hf::{HfChannel, HfConditions};

/// The unfaded voice level of a received stream (at the stream AGC's target), which background
/// levels and the receivers' gain controls are set against.
pub(crate) fn voice_rms() -> f32 {
    10f32.powf(RmsAgc::DEFAULT_TARGET_DB as f32 / 20.0)
}

/// A stable hash of a callsign, for per-station randomness that is the same on every run and
/// every machine. FNV-1a: unlike `DefaultHasher`, it does not change between Rust versions.
pub(crate) fn callsign_hash(callsign: &str) -> u64 {
    callsign.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}
