//! HF propagation for single-sideband voice: receiver mistuning, the Watterson multipath fading
//! model, and atmospheric static. See `docs/HF.md` for a plain description.
//!
//! - **Mistuning.** Aviation HF is upper sideband without a carrier, so a receiver tuned off the
//!   transmitter shifts every tone of the voice by the same number of hertz. Unlike a pitch
//!   change, this pulls the harmonics out of their ratios: voices sound squeaky or growly.
//! - **Multipath fading** (Watterson, Juroshek and Bensema 1970; CCIR 520, ITU-R F.1487). The
//!   signal arrives over two ionospheric paths a few milliseconds apart. Each path's gain is a
//!   complex random process whose spectrum is a Gaussian as wide as the Doppler spread, so it
//!   varies in level and in phase. Two copies a delay τ apart cancel at frequencies 1/τ apart:
//!   notches sweep through the voice as the gains change, and the phase changes make it warble.
//! - **Static.** Lightning anywhere within thousands of kilometres reaches the receiver as short
//!   broadband impulses. The receiving set's steep sideband filter rings with each one, turning
//!   it into a brief blip of a few milliseconds.
//!
//! [`HfChannel`] applies mistuning and fading to one station; [`Atmospherics`] makes the noise,
//! static and sounders at the receiver. Both take their strengths from [`HfConditions`].

mod atmospherics;
mod channel;

pub(crate) use atmospherics::Atmospherics;
pub use channel::HfChannel;

// The noise floors of the presets, 40, 26 and 16 dB signal-to-noise. ITU-R F.339 grades SSB
// telephony by the least signal-to-noise each grade needs: 33 dB for good commercial quality,
// 15 dB for marginally commercial and 6 dB for just usable. Circuits are planned to do better than
// their grade's minimum, so GOOD sits well above good commercial, FAIR between the two commercial
// grades and POOR at marginally commercial. Just usable is a `noise_level` of 0.5.
const GOOD_NOISE: f32 = 0.01;
const FAIR_NOISE: f32 = 0.05;
const POOR_NOISE: f32 = 0.16;
// Sounders under every preset but OFF. They are transmitters of their own, sweeping the whole
// band, so how many pass and how loud does not depend on the station heard.
const SOUNDER_RATE_HZ: f32 = 0.1;
const SOUNDER_LEVEL: f32 = 0.8;

/// How strongly HF propagation colours received voice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HfConditions {
    /// Largest tuning error between a station and the receiver, in Hz. Each station keeps its own
    /// fixed error within ±this.
    pub offset_hz: f32,
    /// Largest slow drift around a station's tuning error, in Hz.
    pub drift_hz: f32,
    /// Delay between the two propagation paths, in ms; 0 for a single path. Watterson's
    /// reference channels use 0.5 (good), 1 (moderate) and 2 ms (poor).
    pub multipath_delay_ms: f32,
    /// Doppler spread of each path in Hz, i.e. how fast it fades: 0.1 (good), 0.5 (moderate),
    /// 1 (poor), 10 (polar flutter). 0 stops fading.
    pub fading_rate_hz: f32,
    /// 0 for steady paths, up to 1 for full Rayleigh fading with deep nulls; in between, a steady
    /// component with fading on top (Rician).
    pub fading_depth: f32,
    /// Slow fading on top, from changing ionospheric absorption: the standard deviation of the
    /// signal level in dB, varying over tens of seconds (log-normal). 0 for none.
    pub slow_fading_db: f32,
    /// Background noise relative to an unfaded voice, both measured within the receiver's
    /// passband: 1 is a signal-to-noise ratio of 0 dB, 0.1 is 20 dB. The receiver's gain control
    /// raises the noise in the pauses, so it is heard louder than this.
    pub noise_level: f32,
    /// Static crashes per second from distant lightning, on average; each may set off a run of
    /// crackling.
    pub crackle_rate_hz: f32,
    /// Loudness of the static, 0 to 1.
    pub crackle_level: f32,
    /// Ionospheric sounders sweeping past the frequency, per second on average; each is heard as
    /// a short rising blip.
    pub sounder_rate_hz: f32,
    /// Loudness of the sounders, 0 to 1.
    pub sounder_level: f32,
}

impl HfConditions {
    /// Clean single-sideband audio. The signal still passes the channel's allpass filters, which
    /// change its phase but not its sound, so switching conditions never clicks.
    pub const OFF: Self = Self {
        offset_hz: 0.0,
        drift_hz: 0.0,
        multipath_delay_ms: 0.0,
        fading_rate_hz: 0.0,
        fading_depth: 0.0,
        slow_fading_db: 0.0,
        noise_level: 0.0,
        crackle_rate_hz: 0.0,
        crackle_level: 0.0,
        sounder_rate_hz: 0.0,
        sounder_level: 0.0,
    };
    /// About as good as HF gets: Watterson's "good" channel with a steady component left in.
    pub const GOOD: Self = Self {
        offset_hz: 40.0,
        drift_hz: 5.0,
        multipath_delay_ms: 0.5,
        fading_rate_hz: 0.1,
        fading_depth: 0.8,
        slow_fading_db: 2.0,
        noise_level: GOOD_NOISE,
        crackle_rate_hz: 0.5,
        crackle_level: 0.3,
        sounder_rate_hz: SOUNDER_RATE_HZ,
        sounder_level: SOUNDER_LEVEL,
    };
    /// Typical conditions, and the default: Watterson's "moderate" channel.
    pub const FAIR: Self = Self {
        offset_hz: 200.0,
        drift_hz: 25.0,
        multipath_delay_ms: 1.0,
        fading_rate_hz: 0.5,
        fading_depth: 1.0,
        slow_fading_db: 9.0,
        noise_level: FAIR_NOISE,
        crackle_rate_hz: 2.0,
        crackle_level: 0.6,
        sounder_rate_hz: SOUNDER_RATE_HZ,
        sounder_level: SOUNDER_LEVEL,
    };
    /// Watterson's "poor" channel, badly tuned, with frequent static.
    pub const POOR: Self = Self {
        offset_hz: 500.0,
        drift_hz: 60.0,
        multipath_delay_ms: 2.0,
        fading_rate_hz: 1.0,
        fading_depth: 1.0,
        slow_fading_db: 14.0,
        noise_level: POOR_NOISE,
        crackle_rate_hz: 6.0,
        crackle_level: 1.0,
        sounder_rate_hz: SOUNDER_RATE_HZ,
        sounder_level: SOUNDER_LEVEL,
    };
    /// Watterson's "flutter" channel: fast fading as on polar routes.
    pub const FLUTTER: Self = Self {
        offset_hz: 200.0,
        drift_hz: 25.0,
        multipath_delay_ms: 0.5,
        fading_rate_hz: 10.0,
        fading_depth: 1.0,
        slow_fading_db: 9.0,
        noise_level: FAIR_NOISE,
        crackle_rate_hz: 2.0,
        crackle_level: 0.6,
        sounder_rate_hz: SOUNDER_RATE_HZ,
        sounder_level: SOUNDER_LEVEL,
    };

    /// Non-finite values as 0, so a bad setting cannot leave a channel's state broken.
    fn sanitized(&self) -> Self {
        let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
        Self {
            offset_hz: finite(self.offset_hz),
            drift_hz: finite(self.drift_hz),
            multipath_delay_ms: finite(self.multipath_delay_ms),
            fading_rate_hz: finite(self.fading_rate_hz),
            fading_depth: finite(self.fading_depth),
            slow_fading_db: finite(self.slow_fading_db),
            noise_level: finite(self.noise_level),
            crackle_rate_hz: finite(self.crackle_rate_hz),
            crackle_level: finite(self.crackle_level),
            sounder_rate_hz: finite(self.sounder_rate_hz),
            sounder_level: finite(self.sounder_level),
        }
    }
}

impl Default for HfConditions {
    fn default() -> Self {
        Self::FAIR
    }
}
