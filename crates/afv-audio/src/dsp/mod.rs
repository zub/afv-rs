//! Signal-processing building blocks.

use std::cell::RefCell;

use num_traits::Float;
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

/// The amplitude ratio of `db` decibels.
pub(crate) fn db_to_gain<F: Float + From<u8>>(db: F) -> F {
    F::powf(10u8.into(), db / 20u8.into())
}

/// `e^(−step/time_constant)`: how much of its previous value a first-order smoother keeps after
/// `step` (in the same unit as the time constant). One minus it is the share it moves.
pub(crate) fn pole<F: Float>(step: F, time_constant: F) -> F {
    (-step / time_constant).exp()
}

/// The unfaded voice level of a received stream (at the stream AGC's target), which background
/// levels and the receivers' gain controls are set against.
pub(crate) fn voice_rms() -> f32 {
    db_to_gain(RmsAgc::DEFAULT_TARGET_DB)
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
