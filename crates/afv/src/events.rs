use std::sync::Arc;

use tokio::sync::broadcast;

use crate::radio::RadioId;
use crate::{Error, SessionState};

/// Something that happened in a [`crate::Client`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// The connection state changed.
    Session(SessionState),
    /// Transmission started: PTT is pressed and the server knows the current transceivers.
    PttOpened,
    PttClosed,
    /// `callsign` is now heard on `radio`. Only reported for radios on a bus that is rendered.
    RxStarted {
        radio: RadioId,
        callsign: String,
    },
    RxEnded {
        radio: RadioId,
        callsign: String,
    },
    /// The server accepted the current transceivers.
    TransceiversSynced,
    /// Sending the transceivers failed; it is retried. PTT stays closed meanwhile.
    TransceiverSyncFailed(Arc<Error>),
    /// An audio device failed or changed (`cpal` backend).
    DeviceError(String),
    /// The receiver fell behind and missed this many events.
    Missed(u64),
}

/// The stream of [`Event`]s from [`crate::Client::events`].
#[derive(Debug)]
pub struct Events(pub(crate) broadcast::Receiver<Event>);

impl Events {
    /// The next event; `None` once the client is gone.
    pub async fn next(&mut self) -> Option<Event> {
        match self.0.recv().await {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(n)) => Some(Event::Missed(n)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    /// The next event if one is waiting.
    pub fn try_next(&mut self) -> Option<Event> {
        match self.0.try_recv() {
            Ok(event) => Some(event),
            Err(broadcast::error::TryRecvError::Lagged(n)) => Some(Event::Missed(n)),
            Err(_) => None,
        }
    }
}
