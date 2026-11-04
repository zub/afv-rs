//! The session's background task: opens the voice link, keeps it alive with heartbeats,
//! transceiver refreshes and token renewal, and reopens it when it is lost.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use afv_proto::api::VoiceServer;
use afv_proto::voice::Heartbeat;
use afv_proto::{Channel, Message};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until};

use super::liveness::{Liveness, Transition};
use super::{LinkLoss, SessionState, Shared};
use crate::error::{Error, Result};

/// The largest UDP datagram that can arrive.
const MAX_DATAGRAM: usize = 65_536;
const TOKEN_RETRY_DELAY: Duration = Duration::from_secs(10);
/// Socket errors tolerated within [`SOCKET_ERROR_WINDOW`] before the link is considered lost.
const SOCKET_ERROR_LIMIT: u32 = 3;
const SOCKET_ERROR_WINDOW: Duration = Duration::from_secs(10);

/// The sending half of an open voice link.
pub(super) struct Link {
    /// Non-blocking std handle to the socket, used to send from any thread. Unlike
    /// `tokio::net::UdpSocket::try_send`, it does not depend on the runtime having observed
    /// writability first.
    pub(super) sender: Arc<std::net::UdpSocket>,
    pub(super) channel: Channel,
}

impl Link {
    /// Opens a UDP socket connected to the voice server, numbering datagrams from `next_sequence`.
    /// Returns the link for sending and a tokio handle to the same socket for receiving.
    async fn open(server: &VoiceServer, next_sequence: u64) -> Result<(Self, UdpSocket)> {
        let address = &server.address_ipv4;
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
        let channel = Channel::continuing(&server.channel_config, next_sequence);
        let link = Self { sender: Arc::new(socket.try_clone()?), channel };
        Ok((link, UdpSocket::from_std(socket)?))
    }
}

/// A freshly opened link, as the background task needs it.
pub(super) struct Established {
    /// When to renew the bearer token.
    token_refresh: Instant,
    socket: UdpSocket,
}

enum LinkEnd {
    Shutdown,
    Lost(LinkLoss),
    Fatal(Error),
}

impl Shared {
    /// Logs in, allocates a voice session and opens the UDP link.
    pub(super) async fn establish(&self) -> Result<Established> {
        let token_refresh = self.api.authenticate().await?;
        // From here on the server may hold the callsign, even if the request fails or this
        // future is dropped before the reply: `release` must then give it back.
        self.allocated.store(true, Ordering::Relaxed);
        let server = self.api.allocate_voice_session().await?;
        // Keep numbering datagrams where the previous link stopped: harmless if the server forgot
        // the old session, and needed if it did not.
        let (link, socket) = Link::open(&server, self.next_tx_sequence.load(Ordering::Relaxed)).await?;
        *self.link.lock().unwrap() = Some(link);
        self.set_state(SessionState::Connected);
        self.post_transceivers().await;
        Ok(Established { token_refresh, socket })
    }

    pub(super) async fn run(self: Arc<Self>, mut established: Established, mut shutdown: watch::Receiver<bool>) {
        loop {
            match self.run_link(established, &mut shutdown).await {
                LinkEnd::Shutdown => break,
                LinkEnd::Fatal(e) => {
                    self.teardown().await;
                    self.set_state(SessionState::Disconnected(Some(Arc::new(e))));
                    return;
                }
                LinkEnd::Lost(reason) => {
                    tracing::warn!(?reason, "voice link lost");
                    self.release().await;
                    match self.reconnect(reason, &mut shutdown).await {
                        Ok(next) => established = next,
                        Err(Some(e)) => {
                            self.teardown().await;
                            self.set_state(SessionState::Disconnected(Some(Arc::new(e))));
                            return;
                        }
                        Err(None) => break,
                    }
                }
            }
        }
        self.teardown().await;
        self.set_state(SessionState::Disconnected(None))
    }

    async fn teardown(&self) {
        self.release().await;
        self.api.clear_token();
    }

    /// Closes the link and releases the callsign on the server, once no post is in progress.
    pub(super) async fn release(&self) {
        let _posting = self.posting.lock().await;
        if let Some(link) = self.link.lock().unwrap().take() {
            self.next_tx_sequence.store(link.channel.next_sequence(), Ordering::Relaxed);
        }
        if !self.allocated.swap(false, Ordering::Relaxed) {
            return;
        }
        if let Err(e) = self.api.release_voice_session().await {
            tracing::debug!("releasing callsign failed: {e}");
        }
    }

    /// Retries [`Self::establish`] with exponential backoff. `Err(None)` means shutdown was
    /// requested; `Err(Some(_))` is a fatal error.
    async fn reconnect(
        &self,
        reason: LinkLoss,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<Established, Option<Error>> {
        let mut backoff = self.timing().reconnect_backoff;
        for attempt in 1.. {
            self.set_state(SessionState::Reconnecting { reason: reason.clone(), attempt });
            let result = tokio::select! {
                _ = shutdown.changed() => return Err(None),
                r = self.establish() => r,
            };
            match result {
                Ok(established) => return Ok(established),
                Err(e) if e.is_fatal() => return Err(Some(e)),
                Err(e) => {
                    tracing::warn!(attempt, "reconnect failed: {e}");
                    // A callsign allocated before the failure is not left on the server.
                    self.release().await;
                }
            }
            tokio::select! {
                _ = shutdown.changed() => return Err(None),
                _ = sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(self.timing().reconnect_backoff_max);
        }
        unreachable!()
    }

    /// Runs the connected voice link until it is lost or shutdown is requested. The refreshes
    /// and token renewals it starts are aborted when it returns, so none outlives the link.
    async fn run_link(self: &Arc<Self>, established: Established, shutdown: &mut watch::Receiver<bool>) -> LinkEnd {
        let Established { mut token_refresh, socket } = established;
        let timing = self.timing().clone();
        let now = Instant::now();
        let mut liveness = Liveness::new(&timing, now);
        let mut heartbeat = interval_at(now + timing.heartbeat_interval, timing.heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut refresh = interval_at(now + timing.transceiver_refresh, timing.transceiver_refresh);
        refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut refreshes = JoinSet::new();
        let mut token_renewal: JoinSet<Result<Instant>> = JoinSet::new();
        let mut socket_errors = ErrorWindow::default();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let callsign = self.callsign().to_owned();

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
                    while refreshes.try_join_next().is_some() {}
                    // The refresh only keeps the server's view fresh; skip a tick while the last
                    // one is still out rather than queue them up.
                    if refreshes.is_empty() {
                        let shared = self.clone();
                        refreshes.spawn(async move { shared.post_transceivers().await });
                    }
                }

                _ = sleep_until(token_refresh), if token_renewal.is_empty() => {
                    let shared = self.clone();
                    token_renewal.spawn(async move { shared.api.authenticate().await });
                }

                Some(result) = token_renewal.join_next(), if !token_renewal.is_empty() => match result {
                    Ok(Ok(next)) => token_refresh = next,
                    Ok(Err(e)) if e.is_fatal() => return LinkEnd::Fatal(e),
                    Ok(Err(e)) => {
                        tracing::warn!("token refresh failed: {e}");
                        token_refresh = Instant::now() + TOKEN_RETRY_DELAY;
                    }
                    Err(e) => {
                        tracing::error!("token refresh task failed: {e}");
                        token_refresh = Instant::now() + TOKEN_RETRY_DELAY;
                    }
                },

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
                // Full when nothing is playing audio, e.g. a client without output devices.
                if let Err(mpsc::error::TrySendError::Full(_)) = self.incoming_audio.try_send(audio) {
                    tracing::trace!("incoming audio queue full; dropping frame");
                }
            }
            other => tracing::debug!(?other, "ignored message"),
        }
    }
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
