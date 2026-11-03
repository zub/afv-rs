//! Signal-processing building blocks.

use std::cell::RefCell;

use rand::SeedableRng;
use rand::rngs::SmallRng;

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

thread_local! {
    /// The thread's random numbers, seeded once from the operating system. Not `rand::rng()`,
    /// which reseeds from the OS every 64 KiB of output: several system calls a second on an
    /// audio thread.
    static RNG: RefCell<SmallRng> = RefCell::new(rand::make_rng());
}

/// Runs `f` with this thread's random number generator. Not reentrant: `f` must not call it again.
pub(crate) fn with_rng<T>(f: impl FnOnce(&mut SmallRng) -> T) -> T {
    RNG.with_borrow_mut(f)
}

/// Seeds this thread's random numbers, so that a test can repeat a run.
#[doc(hidden)]
pub fn seed_rng(seed: u64) {
    RNG.set(SmallRng::seed_from_u64(seed));
}
