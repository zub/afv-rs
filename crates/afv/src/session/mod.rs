//! A connected AFV session: REST login plus the encrypted UDP voice link, kept alive and
//! reconnected by a background task.
//!
//! - this module: the public [`Session`] and its builder and states;
//! - `shared`: the state the session shares with its background task and with [`crate::Client`];
//! - `link`: the background task, which opens, watches and reopens the voice link;
//! - `liveness`: heartbeat bookkeeping, free of I/O.

use std::io;
use std::sync::Arc;

use afv_proto::api::{CrossCoupleGroup, Station, StationTransceiver, Transceiver};
use afv_proto::voice::AudioRx;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::{Config, Credentials, Timing};
use crate::error::{Error, Result};

mod link;
mod liveness;
mod shared;

pub(crate) use shared::Shared;

/// Connection status of a [`Session`].
#[derive(Debug, Clone)]
pub enum SessionState {
    Connected,
    /// Still connected, but several heartbeats went unanswered.
    Degraded,
    /// The voice link was lost; the session is logging in again.
    Reconnecting {
        reason: LinkLoss,
        attempt: u32,
    },
    /// The session has ended: `None` if closed on request, otherwise the error that ended it.
    Disconnected(Option<Arc<Error>>),
}

/// Why the voice link was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkLoss {
    /// Too many heartbeats went unacknowledged.
    HeartbeatTimeout,
    /// No valid datagram arrived within the silence timeout.
    Silence,
    /// Repeated socket errors.
    Socket(io::ErrorKind),
}

/// Configures and opens a [`Session`].
#[derive(Debug, Clone)]
pub struct SessionBuilder {
    api_url: String,
    credentials: Option<Credentials>,
    callsign: Option<String>,
    client_name: String,
    timing: Timing,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            api_url: afv_proto::consts::DEFAULT_API_URL.into(),
            credentials: None,
            callsign: None,
            client_name: concat!("afv-rs ", env!("CARGO_PKG_VERSION")).into(),
            timing: Timing::default(),
        }
    }
}

impl SessionBuilder {
    pub fn api_url(mut self, url: impl Into<String>) -> Self {
        self.api_url = url.into();
        self
    }

    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some(Credentials::new(username, password));
        self
    }

    pub fn callsign(mut self, callsign: impl Into<String>) -> Self {
        self.callsign = Some(callsign.into());
        self
    }

    pub fn client_name(mut self, name: impl Into<String>) -> Self {
        self.client_name = name.into();
        self
    }

    pub fn timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// Logs in and opens the voice link. Returns once connected, or with the first error;
    /// later link failures are retried in the background.
    ///
    /// # Panics
    ///
    /// If credentials or callsign were not set.
    pub async fn connect(self) -> Result<Session> {
        let config = Config {
            api_url: self.api_url,
            credentials: self.credentials.expect("SessionBuilder::credentials must be set"),
            callsign: self.callsign.expect("SessionBuilder::callsign must be set"),
            client_name: self.client_name,
            timing: self.timing,
        };
        Session::connect(config).await
    }
}

/// An open AFV session: the network side only. Most applications want [`crate::Client`], which
/// adds radios, PTT and audio on top.
///
/// Dropping it stops the background task, which then releases the callsign on the server
/// if the runtime is still running. Use [`Session::disconnect`] to wait for that.
pub struct Session {
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Session {
    pub fn builder() -> SessionBuilder {
        SessionBuilder::default()
    }

    async fn connect(config: Config) -> Result<Self> {
        let shared = Arc::new(Shared::new(config)?);
        let established = match shared.establish().await {
            Ok(established) => established,
            Err(e) => {
                // E.g. the callsign was allocated but the voice server is unreachable.
                shared.release().await;
                return Err(e);
            }
        };
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(shared.clone().run(established, shutdown_rx));
        Ok(Self { shared, shutdown, task })
    }

    pub fn callsign(&self) -> &str {
        self.shared.callsign()
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn state(&self) -> SessionState {
        self.shared.state.borrow().clone()
    }

    /// Subscribe to state changes.
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.shared.watch_state()
    }

    /// The stream of received audio frames. Can be taken once.
    ///
    /// Frames are dropped if the receiver falls more than 512 frames behind.
    pub fn take_incoming_audio(&self) -> Option<mpsc::Receiver<AudioRx>> {
        self.shared.take_incoming_audio()
    }

    /// Sends one encoded 20 ms Opus frame on the given transceivers.
    ///
    /// Does not block; fails with [`Error::NotConnected`] while reconnecting.
    pub fn send_audio(&self, sequence: u32, opus: &[u8], last_packet: bool, transceivers: &[u16]) -> Result<()> {
        self.shared.send_audio(sequence, opus, last_packet, transceivers)
    }

    /// Replaces the transceiver set and sends it to the server. The set is re-sent periodically
    /// and after every reconnect. Returns once the server has accepted it.
    pub async fn set_transceivers(&self, transceivers: Vec<Transceiver>) -> Result<()> {
        self.shared.set_transceivers(transceivers).await
    }

    /// Replaces the cross-couple groups (ATC). Re-sent with every transceiver refresh.
    pub async fn set_cross_couple_groups(&self, groups: Vec<CrossCoupleGroup>) -> Result<()> {
        self.shared.set_cross_couple_groups(groups).await
    }

    /// Looks up a station by name; `None` if it does not exist.
    pub async fn station(&self, name: &str) -> Result<Option<Station>> {
        self.shared.api.station(name).await
    }

    /// The antenna sites of a station.
    pub async fn station_transceivers(&self, name: &str) -> Result<Vec<StationTransceiver>> {
        self.shared.api.station_transceivers(name).await
    }

    /// The VCCS panel of a station, in panel order.
    pub async fn vccs(&self, name: &str) -> Result<Vec<Station>> {
        self.shared.api.vccs(name).await
    }

    /// Stations with a frequency alias.
    pub async fn aliased_stations(&self) -> Result<Vec<Station>> {
        self.shared.api.aliased_stations().await
    }

    /// Closes the voice link and releases the callsign on the server.
    pub async fn disconnect(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}
