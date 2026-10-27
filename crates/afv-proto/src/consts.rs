//! Timing and audio parameters a compatible client follows.

use std::time::Duration;

/// Default API server.
pub const DEFAULT_API_URL: &str = "https://voice1.vatsim.net";

/// Interval between `H` heartbeats.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
/// The voice session is considered dead after this long without any valid datagram.
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(20);
/// Unacknowledged heartbeats before the connection is reported as degraded.
pub const DEGRADED_AFTER_MISSED_HEARTBEATS: u32 = 5;
/// Unacknowledged heartbeats before the voice session is torn down and reconnected.
pub const TIMEOUT_AFTER_MISSED_HEARTBEATS: u32 = 10;

/// Periodic transceiver refresh for pilot clients.
pub const PILOT_TRANSCEIVER_REFRESH: Duration = Duration::from_secs(20);
/// Periodic transceiver refresh for ATC and ATIS clients.
pub const ATC_TRANSCEIVER_REFRESH: Duration = Duration::from_secs(60);

/// Re-authenticate this long before the bearer token expires.
pub const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);
/// Token lifetime assumed when the JWT carries no `exp` claim.
pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);

pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
pub const FRAME_SAMPLES: usize = 960;
/// Opus encoder bitrate in bits per second.
pub const OPUS_BITRATE: i32 = 16_384;

/// Frequencies below this are treated as HF for radio effects.
pub const HF_UPPER_LIMIT_HZ: u32 = 30_000_000;
