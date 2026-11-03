//! Signal-processing building blocks, ported from afv-native and its vendored libraries.

mod agc;
mod am;
mod biquad;
mod chunkware;
mod compressor;
mod hardware;
mod hf;

pub use agc::RmsAgc;
pub use am::{AmChannel, Carrier};
pub use biquad::Biquad;
pub use chunkware::{SimpleComp, SimpleLimit};
pub use compressor::Compressor;
pub use hardware::{Hardware, HardwareFilter};
pub use hf::{Atmospherics, Crackle, HfChannel, HfConditions, Hilbert, Hiss, Sounders, SsbFilter};

/// A stable hash of a callsign, for per-station randomness that is the same on every run and
/// every machine. FNV-1a: unlike `DefaultHasher`, it does not change between Rust versions.
pub(crate) fn callsign_hash(callsign: &str) -> u64 {
    callsign.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}
