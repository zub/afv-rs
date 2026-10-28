//! A connected AFV session: REST login plus the encrypted UDP voice link, kept alive and
//! reconnected by a background task.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use afv_proto::api::{CrossCoupleGroup, Station, StationTransceiver, Transceiver};
use afv_proto::voice::{AudioRx, AudioTx, Dto, Heartbeat};
use afv_proto::{Channel, Message};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until};

use crate::api::Api;
use crate::config::{Config, Credentials, Timing};
use crate::error::{Error, Result};
use crate::liveness::{Liveness, Transition};

const INCOMING_AUDIO_CAPACITY: usize = 512;
const TOKEN_RETRY_DELAY: Duration = Duration::from_secs(10);
/// Socket errors tolerated within [`SOCKET_ERROR_WINDOW`] before the link is considered lost.
const SOCKET_ERROR_LIMIT: u32 = 3;
const SOCKET_ERROR_WINDOW: Duration = Duration::from_secs(10);

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
pub struct ClientBuilder {
    api_url: String,
    credentials: Option<Credentials>,
    callsign: Option<String>,
    client_name: String,
    timing: Timing,
}

impl Default for ClientBuilder {
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

impl ClientBuilder {
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
            credentials: self.credentials.expect("ClientBuilder::credentials must be set"),
            callsign: self.callsign.expect("ClientBuilder::callsign must be set"),
            client_name: self.client_name,
            timing: self.timing,
        };
        Session::connect(config).await
    }
}

/// An open AFV session.
///
/// Dropping it stops the background task, which then releases the callsign on the server
/// if the runtime is still running. Use [`Session::disconnect`] to wait for that.
pub struct Session {
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct Shared {
    api: Api,
    link: Mutex<Option<Link>>,
    transceivers: Mutex<Vec<Transceiver>>,
    cross_couple_groups: Mutex<Option<Vec<CrossCoupleGroup>>>,
    state: watch::Sender<SessionState>,
    incoming_audio: mpsc::Sender<AudioRx>,
    incoming_audio_rx: Mutex<Option<mpsc::Receiver<AudioRx>>>,
}

struct Link {
    /// Tokio handle, used by the background task to receive.
    socket: Arc<UdpSocket>,
    /// Non-blocking std handle to the same socket, used to send from any thread. Unlike
    /// `tokio::net::UdpSocket::try_send`, it does not depend on the runtime having observed
    /// writability first.
    sender: Arc<std::net::UdpSocket>,
    channel: Channel,
}

enum LinkEnd {
    Shutdown,
    Lost(LinkLoss),
    Fatal(Error),
}

impl Session {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    async fn connect(config: Config) -> Result<Self> {
        let (audio_tx, audio_rx) = mpsc::channel(INCOMING_AUDIO_CAPACITY);
        let shared = Arc::new(Shared {
            api: Api::new(config)?,
            link: Mutex::new(None),
            transceivers: Mutex::new(Vec::new()),
            cross_couple_groups: Mutex::new(None),
            state: watch::Sender::new(SessionState::Disconnected(None)),
            incoming_audio: audio_tx,
            incoming_audio_rx: Mutex::new(Some(audio_rx)),
        });
        let token_refresh = shared.establish().await?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(shared.clone().run(token_refresh, shutdown_rx));
        Ok(Self { shared, shutdown, task })
    }

    pub fn callsign(&self) -> &str {
        &self.shared.api.config().callsign
    }

    pub fn state(&self) -> SessionState {
        self.shared.state.borrow().clone()
    }

    /// Subscribe to state changes.
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.shared.state.subscribe()
    }

    /// The stream of received audio frames. Can be taken once.
    ///
    /// Frames are dropped if the receiver falls more than 512 frames behind.
    pub fn take_incoming_audio(&self) -> Option<mpsc::Receiver<AudioRx>> {
        self.shared.incoming_audio_rx.lock().unwrap().take()
    }

    /// Sends one encoded 20 ms Opus frame on the given transceivers.
    ///
    /// Does not block; fails with [`Error::NotConnected`] while reconnecting.
    pub fn send_audio(&self, sequence: u32, opus: &[u8], last_packet: bool, transceivers: &[u16]) -> Result<()> {
        self.shared.send(&AudioTx {
            callsign: self.callsign().to_owned(),
            sequence,
            audio: opus.to_vec(),
            last_packet,
            transceivers: transceivers.to_vec(),
        })
    }

    /// Replaces the transceiver set and sends it to the server. The set is re-sent periodically
    /// and after every reconnect. Returns once the server has accepted it.
    pub async fn set_transceivers(&self, transceivers: Vec<Transceiver>) -> Result<()> {
        *self.shared.transceivers.lock().unwrap() = transceivers.clone();
        self.shared.api.post_transceivers(&transceivers).await
    }

    /// Replaces the cross-couple groups (ATC). Re-sent with every transceiver refresh.
    pub async fn set_cross_couple_groups(&self, groups: Vec<CrossCoupleGroup>) -> Result<()> {
        *self.shared.cross_couple_groups.lock().unwrap() = Some(groups.clone());
        self.shared.api.post_cross_couple_groups(&groups).await
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

impl Shared {
    fn timing(&self) -> &Timing {
        &self.api.config().timing
    }

    fn set_state(&self, state: SessionState) {
        tracing::info!(?state, "session state");
        self.state.send_replace(state);
    }

    fn send<D: Dto>(&self, dto: &D) -> Result<()> {
        let (sender, datagram) = {
            let mut link = self.link.lock().unwrap();
            let link = link.as_mut().ok_or(Error::NotConnected)?;
            (link.sender.clone(), link.channel.seal(dto)?)
        };
        match sender.send(&datagram) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                tracing::warn!("UDP send buffer full; dropped {}", D::NAME);
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Logs in, allocates a voice session and opens the UDP link.
    async fn establish(&self) -> Result<Instant> {
        let token_refresh = self.api.authenticate().await?;
        let server = self.api.allocate_voice_session().await?;
        let (socket, sender) = open_socket(&server.address_ipv4).await?;
        *self.link.lock().unwrap() = Some(Link {
            socket: Arc::new(socket),
            sender: Arc::new(sender),
            channel: Channel::new(&server.channel_config),
        });
        self.set_state(SessionState::Connected);
        self.post_transceivers().await;
        Ok(token_refresh)
    }

    /// Re-sends the stored transceivers and cross-couple groups, logging failures.
    async fn post_transceivers(&self) {
        let transceivers = self.transceivers.lock().unwrap().clone();
        if !transceivers.is_empty()
            && let Err(e) = self.api.post_transceivers(&transceivers).await
        {
            tracing::warn!("transceiver update failed: {e}");
        }
        let groups = self.cross_couple_groups.lock().unwrap().clone();
        if let Some(groups) = groups
            && let Err(e) = self.api.post_cross_couple_groups(&groups).await
        {
            tracing::warn!("cross-couple update failed: {e}");
        }
    }

    async fn run(self: Arc<Self>, mut token_refresh: Instant, mut shutdown: watch::Receiver<bool>) {
        loop {
            match self.run_link(&mut token_refresh, &mut shutdown).await {
                LinkEnd::Shutdown => break,
                LinkEnd::Fatal(e) => {
                    self.teardown().await;
                    self.set_state(SessionState::Disconnected(Some(Arc::new(e))));
                    return;
                }
                LinkEnd::Lost(reason) => {
                    tracing::warn!(?reason, "voice link lost");
                    self.link.lock().unwrap().take();
                    if let Err(e) = self.api.release_voice_session().await {
                        tracing::debug!("releasing callsign failed: {e}");
                    }
                    match self.reconnect(reason, &mut shutdown).await {
                        Ok(next_refresh) => token_refresh = next_refresh,
                        Err(Some(e)) => {
                            self.api.clear_token();
                            self.set_state(SessionState::Disconnected(Some(Arc::new(e))));
                            return;
                        }
                        Err(None) => break,
                    }
                }
            }
        }
        self.teardown().await;
        self.set_state(SessionState::Disconnected(None));
    }

    async fn teardown(&self) {
        if self.link.lock().unwrap().take().is_some()
            && let Err(e) = self.api.release_voice_session().await
        {
            tracing::debug!("releasing callsign failed: {e}");
        }
        self.api.clear_token();
    }

    /// Retries [`Self::establish`] with exponential backoff. `Err(None)` means shutdown was
    /// requested; `Err(Some(_))` is a fatal error.
    async fn reconnect(
        &self,
        reason: LinkLoss,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<Instant, Option<Error>> {
        let mut backoff = self.timing().reconnect_backoff;
        for attempt in 1.. {
            self.set_state(SessionState::Reconnecting { reason: reason.clone(), attempt });
            let result = tokio::select! {
                _ = shutdown.changed() => return Err(None),
                r = self.establish() => r,
            };
            match result {
                Ok(next_refresh) => return Ok(next_refresh),
                Err(e) if e.is_fatal() => return Err(Some(e)),
                Err(e) => tracing::warn!(attempt, "reconnect failed: {e}"),
            }
            tokio::select! {
                _ = shutdown.changed() => return Err(None),
                _ = sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(self.timing().reconnect_backoff_max);
        }
        unreachable!()
    }

    /// Runs the connected voice link until it is lost or shutdown is requested.
    async fn run_link(self: &Arc<Self>, token_refresh: &mut Instant, shutdown: &mut watch::Receiver<bool>) -> LinkEnd {
        let Some(socket) = self.link.lock().unwrap().as_ref().map(|l| l.socket.clone()) else {
            return LinkEnd::Lost(LinkLoss::Silence);
        };
        let timing = self.timing().clone();
        let now = Instant::now();
        let mut liveness = Liveness::new(&timing, now);
        let mut heartbeat = interval_at(now + timing.heartbeat_interval, timing.heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut refresh = interval_at(now + timing.transceiver_refresh, timing.transceiver_refresh);
        refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut token_task: Option<JoinHandle<Result<Instant>>> = None;
        let mut socket_errors = ErrorWindow::default();
        let mut buf = vec![0u8; 65_536];
        let callsign = self.api.config().callsign.clone();

        loop {
            tokio::select! {
                _ = shutdown.changed() => return LinkEnd::Shutdown,

                _ = heartbeat.tick() => {
                    if let Err(e) = self.send(&Heartbeat { callsign: callsign.clone() }) {
                        tracing::warn!("sending heartbeat failed: {e}");
                    }
                    match liveness.heartbeat_sent() {
                        Transition::Degraded => self.set_state(SessionState::Degraded),
                        Transition::TimedOut => return LinkEnd::Lost(LinkLoss::HeartbeatTimeout),
                        _ => {}
                    }
                }

                _ = sleep_until(liveness.silence_deadline()) => return LinkEnd::Lost(LinkLoss::Silence),

                _ = refresh.tick() => {
                    let shared = self.clone();
                    tokio::spawn(async move { shared.post_transceivers().await });
                }

                _ = sleep_until(*token_refresh), if token_task.is_none() => {
                    let shared = self.clone();
                    token_task = Some(tokio::spawn(async move { shared.api.authenticate().await }));
                }

                result = async { token_task.as_mut().unwrap().await }, if token_task.is_some() => {
                    token_task = None;
                    match result {
                        Ok(Ok(next)) => *token_refresh = next,
                        Ok(Err(e)) if e.is_fatal() => return LinkEnd::Fatal(e),
                        Ok(Err(e)) => {
                            tracing::warn!("token refresh failed: {e}");
                            *token_refresh = Instant::now() + TOKEN_RETRY_DELAY;
                        }
                        Err(e) => {
                            tracing::error!("token refresh task failed: {e}");
                            *token_refresh = Instant::now() + TOKEN_RETRY_DELAY;
                        }
                    }
                }

                received = socket.recv(&mut buf) => match received {
                    Ok(len) => self.handle_datagram(&buf[..len], &mut liveness),
                    Err(e) => {
                        tracing::warn!("UDP receive error: {e}");
                        if socket_errors.record(Instant::now()) {
                            return LinkEnd::Lost(LinkLoss::Socket(e.kind()));
                        }
                    }
                },
            }
        }
    }

    fn handle_datagram(&self, datagram: &[u8], liveness: &mut Liveness) {
        let opened = {
            let mut link = self.link.lock().unwrap();
            let Some(link) = link.as_mut() else { return };
            link.channel.open(datagram)
        };
        let message = match opened {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("dropped datagram: {e}");
                return;
            }
        };
        liveness.packet_received(Instant::now());
        match message {
            Message::HeartbeatAck => {
                if liveness.ack_received() == Transition::Recovered {
                    self.set_state(SessionState::Connected);
                }
            }
            Message::AudioRx(audio) => {
                if let Err(mpsc::error::TrySendError::Full(_)) = self.incoming_audio.try_send(audio) {
                    tracing::warn!("incoming audio queue full; dropping frame");
                }
            }
            other => tracing::debug!(?other, "ignored message"),
        }
    }
}

/// Opens a UDP socket connected to the voice server, returning a tokio handle for receiving and
/// a non-blocking std handle for sending.
async fn open_socket(address: &str) -> Result<(UdpSocket, std::net::UdpSocket)> {
    let remote = tokio::net::lookup_host(address).await?.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("voice server address {address:?} did not resolve"))
    })?;
    let local: SocketAddr = match remote {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = std::net::UdpSocket::bind(local)?;
    socket.connect(remote)?;
    socket.set_nonblocking(true)?;
    let sender = socket.try_clone()?;
    Ok((UdpSocket::from_std(socket)?, sender))
}

/// Counts errors within a sliding time window.
#[derive(Default)]
struct ErrorWindow {
    first: Option<Instant>,
    count: u32,
}

impl ErrorWindow {
    /// Records an error; returns `true` once the limit is reached within the window.
    fn record(&mut self, now: Instant) -> bool {
        match self.first {
            Some(first) if now.duration_since(first) <= SOCKET_ERROR_WINDOW => self.count += 1,
            _ => {
                self.first = Some(now);
                self.count = 1;
            }
        }
        self.count >= SOCKET_ERROR_LIMIT
    }
}
