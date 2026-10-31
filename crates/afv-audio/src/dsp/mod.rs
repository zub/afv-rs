//! Signal-processing building blocks, ported from afv-native and its vendored libraries.

mod agc;
mod am;
mod biquad;
mod chunkware;
mod compressor;
mod hardware;

pub use agc::RmsAgc;
pub use am::{AmChannel, Carrier};
pub use biquad::Biquad;
pub use chunkware::{SimpleComp, SimpleLimit};
pub use compressor::Compressor;
pub use hardware::{Hardware, HardwareFilter};
