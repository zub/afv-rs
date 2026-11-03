use std::fmt;
use std::time::Duration;

use afv_proto::consts;

/// VATSIM login.
#[derive(Clone)]
pub struct Credentials {
    /// VATSIM CID.
    pub username: String,
    pub password: String,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self { username: username.into(), password: password.into() }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("username", &self.username).field("password", &"<redacted>").finish()
    }
}

/// Protocol timers. The defaults follow the AFV protocol; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    pub heartbeat_interval: Duration,
    /// Reconnect after this long without any valid datagram.
    pub silence_timeout: Duration,
    /// Unacknowledged heartbeats before the session is reported as degraded.
    pub degraded_after: u32,
    /// Unacknowledged heartbeats before the session reconnects.
    pub timeout_after: u32,
    /// How often the transceiver set is re-sent while connected.
    pub transceiver_refresh: Duration,
    /// Re-authenticate this long before the bearer token expires.
    pub token_refresh_margin: Duration,
    /// First delay between reconnect attempts; doubles up to `reconnect_backoff_max`.
    pub reconnect_backoff: Duration,
    pub reconnect_backoff_max: Duration,
    /// Per-request timeout for REST calls.
    pub http_timeout: Duration,
    /// Timeout for establishing the HTTPS connection of a REST call.
    pub connect_timeout: Duration,
}

impl Timing {
    /// Timers for a pilot client.
    pub fn pilot() -> Self {
        Self {
            heartbeat_interval: consts::HEARTBEAT_INTERVAL,
            silence_timeout: consts::SILENCE_TIMEOUT,
            degraded_after: consts::DEGRADED_AFTER_MISSED_HEARTBEATS,
            timeout_after: consts::TIMEOUT_AFTER_MISSED_HEARTBEATS,
            transceiver_refresh: consts::PILOT_TRANSCEIVER_REFRESH,
            token_refresh_margin: consts::TOKEN_REFRESH_MARGIN,
            reconnect_backoff: Duration::from_secs(1),
            reconnect_backoff_max: Duration::from_secs(30),
            http_timeout: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(5),
        }
    }

    /// Timers for an ATC or ATIS client (slower transceiver refresh).
    pub fn atc() -> Self {
        Self { transceiver_refresh: consts::ATC_TRANSCEIVER_REFRESH, ..Self::pilot() }
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::pilot()
    }
}

/// Everything needed to open a session.
#[derive(Debug, Clone)]
pub struct Config {
    pub api_url: String,
    pub credentials: Credentials,
    pub callsign: String,
    /// Identifies the client software to the server.
    pub client_name: String,
    pub timing: Timing,
}
