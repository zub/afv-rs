//! The state a session shares between its owner, its background task and [`crate::Client`]: the
//! REST client, the open link, the stored transceivers and the state.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64};

use afv_proto::api::{CrossCoupleGroup, Transceiver};
use afv_proto::voice::{AudioRx, AudioTx, Dto};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{mpsc, watch};

use super::SessionState;
use super::link::Link;
use crate::api::Api;
use crate::config::{Config, Timing};
use crate::error::{Error, Result};

const INCOMING_AUDIO_CAPACITY: usize = 512;

pub(crate) struct Shared {
    pub(super) api: Api,
    /// `None` while reconnecting.
    pub(super) link: Mutex<Option<Link>>,
    /// The server may hold our callsign: set before asking for it, cleared once released. Kept
    /// apart from `link` because the callsign is allocated before the link opens, and opening
    /// can fail or be cancelled in between.
    pub(super) allocated: AtomicBool,
    /// The transmit sequence a new link starts from: where the last released link stopped.
    pub(super) next_tx_sequence: AtomicU64,
    transceivers: Mutex<Vec<Transceiver>>,
    cross_couple_groups: Mutex<Option<Vec<CrossCoupleGroup>>>,
    /// Held while posting transceivers or cross-couple groups, and while releasing the callsign,
    /// so that posts reach the server in order, a periodic refresh cannot overwrite a newer update
    /// with an older one, and none arrives after the callsign is released.
    pub(super) posting: AsyncMutex<()>,
    pub(super) state: watch::Sender<SessionState>,
    pub(super) incoming_audio: mpsc::Sender<AudioRx>,
    incoming_audio_rx: Mutex<Option<mpsc::Receiver<AudioRx>>>,
}

impl Shared {
    pub(super) fn new(config: Config) -> Result<Self> {
        let (audio_tx, audio_rx) = mpsc::channel(INCOMING_AUDIO_CAPACITY);
        Ok(Self {
            api: Api::new(config)?,
            link: Mutex::new(None),
            allocated: AtomicBool::new(false),
            next_tx_sequence: AtomicU64::new(0),
            transceivers: Mutex::new(Vec::new()),
            cross_couple_groups: Mutex::new(None),
            posting: AsyncMutex::new(()),
            state: watch::Sender::new(SessionState::Disconnected(None)),
            incoming_audio: audio_tx,
            incoming_audio_rx: Mutex::new(Some(audio_rx)),
        })
    }

    pub(crate) fn timing(&self) -> &Timing {
        &self.api.config().timing
    }

    pub(crate) fn callsign(&self) -> &str {
        &self.api.config().callsign
    }

    pub(crate) fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state.subscribe()
    }

    pub(crate) fn take_incoming_audio(&self) -> Option<mpsc::Receiver<AudioRx>> {
        self.incoming_audio_rx.lock().unwrap().take()
    }

    pub(crate) fn send_audio(&self, sequence: u32, opus: &[u8], last_packet: bool, transceivers: &[u16]) -> Result<()> {
        self.send(&AudioTx {
            callsign: self.callsign().to_owned(),
            sequence,
            audio: opus.to_vec(),
            last_packet,
            transceivers: transceivers.to_vec(),
        })
    }

    pub(crate) async fn set_transceivers(&self, transceivers: Vec<Transceiver>) -> Result<()> {
        let _posting = self.posting.lock().await;
        *self.transceivers.lock().unwrap() = transceivers.clone();
        self.api.post_transceivers(&transceivers).await
    }

    /// Replaces the stored transceivers without posting them; they go out with the next refresh.
    pub(crate) fn store_transceivers(&self, transceivers: Vec<Transceiver>) {
        *self.transceivers.lock().unwrap() = transceivers;
    }

    pub(crate) async fn set_cross_couple_groups(&self, groups: Vec<CrossCoupleGroup>) -> Result<()> {
        let _posting = self.posting.lock().await;
        *self.cross_couple_groups.lock().unwrap() = Some(groups.clone());
        self.api.post_cross_couple_groups(&groups).await
    }

    /// Re-sends the stored transceivers and cross-couple groups, logging failures.
    pub(super) async fn post_transceivers(&self) {
        // Read the stored sets only once it is our turn, so they are the newest.
        let _posting = self.posting.lock().await;
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

    pub(super) fn set_state(&self, state: SessionState) {
        tracing::info!(?state, "session state");
        self.state.send_replace(state);
    }

    /// Seals `dto` and sends it without blocking; a full send buffer drops it.
    pub(super) fn send<D: Dto>(&self, dto: &D) -> Result<()> {
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
}
