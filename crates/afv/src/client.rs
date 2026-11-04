//! The client: a session plus radios, PTT and audio.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use afv_audio::EffectSamples;
use afv_proto::api::CrossCoupleGroup;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

use crate::audio::Audio;
use crate::events::{Event, Events};
use crate::radio::{Plan, Position, RadioConfig, RadioId};
use crate::session::Shared;
use crate::{Result, Session, SessionBuilder, SessionState, Timing};

/// Changes arriving within this window are sent to the server together.
const DEBOUNCE: Duration = Duration::from_millis(20);
const EVENT_CAPACITY: usize = 256;

/// Configures and connects a [`Client`].
#[derive(Debug, Clone, Default)]
pub struct ClientBuilder {
    session: SessionBuilder,
    effects: EffectSamples,
}

impl ClientBuilder {
    pub fn api_url(mut self, url: impl Into<String>) -> Self {
        self.session = self.session.api_url(url);
        self
    }

    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.session = self.session.credentials(username, password);
        self
    }

    pub fn callsign(mut self, callsign: impl Into<String>) -> Self {
        self.session = self.session.callsign(callsign);
        self
    }

    pub fn client_name(mut self, name: impl Into<String>) -> Self {
        self.session = self.session.client_name(name);
        self
    }

    /// Protocol timers; use [`Timing::atc`] for ATC and ATIS clients.
    pub fn timing(mut self, timing: Timing) -> Self {
        self.session = self.session.timing(timing);
        self
    }

    /// Recordings for the radio simulation (noise, crackle, click). Without them radios play
    /// voice through their radio set alone.
    pub fn effects(mut self, effects: EffectSamples) -> Self {
        self.effects = effects;
        self
    }

    /// Logs in and opens the voice link.
    ///
    /// # Panics
    ///
    /// If credentials or callsign were not set.
    pub async fn connect(self) -> Result<Client> {
        let session = self.session.connect().await?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let shared = session.shared().clone();
        let audio = Audio::new(shared.clone(), events.clone(), self.effects)?;
        let inner =
            Arc::new(Inner { shared, audio, events, state: Mutex::new(State::default()), changed: Notify::new() });
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(inner.clone().sync_loop(shutdown_rx));
        Ok(Client { session, inner, shutdown, task })
    }
}

/// An AFV client: radios, push-to-talk and audio on top of a [`Session`].
///
/// ```no_run
/// # async fn example() -> afv::Result<()> {
/// use afv::radio::{Position, RadioConfig};
///
/// let client = afv::Client::builder().credentials("1234567", "password").callsign("SAS123").connect().await?;
/// client.set_position(Position { lat_deg: 60.19, lon_deg: 11.1, height_msl_m: 200.0, height_agl_m: 0.0 });
/// let com1 = client.add_radio(RadioConfig { tx: true, ..RadioConfig::pilot("122.800".parse().unwrap()) });
/// client.set_ptt(true);
/// # Ok(()) }
/// ```
pub struct Client {
    session: Session,
    inner: Arc<Inner>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct Inner {
    shared: Arc<Shared>,
    audio: Audio,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    /// Signals the sync task that radios or position changed.
    changed: Notify,
}

#[derive(Default)]
struct State {
    radios: Vec<(RadioId, RadioConfig)>,
    next_id: u32,
    position: Option<Position>,
    ptt_requested: bool,
    ptt_open: bool,
    /// What the server last accepted. Initially nothing, which an empty plan matches.
    acked: Plan,
    in_flight: bool,
    /// Cross-couple groups were sent before, so they must be sent (perhaps empty) again.
    coupling_sent: bool,
}

impl State {
    fn plan(&self) -> Plan {
        Plan::build(self.radios.iter().map(|(_, r)| r), self.position)
    }

    /// Whether the server knows the current frequencies, so PTT may open.
    fn synced(&self) -> bool {
        !self.in_flight && !self.plan().differs_from(&self.acked)
    }
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub fn callsign(&self) -> &str {
        self.session.callsign()
    }

    /// The network session underneath, e.g. for station lookups
    /// ([`Session::station_transceivers`]).
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn state(&self) -> SessionState {
        self.session.state()
    }

    /// The audio engine, to be fed by audio devices.
    pub fn audio(&self) -> Audio {
        self.inner.audio.clone()
    }

    /// Subscribes to events. Each subscriber gets every event from now on.
    pub fn events(&self) -> Events {
        Events(self.inner.events.subscribe())
    }

    /// Sets the client's position, used by radios on [`crate::radio::Sites::Own`]. Positions
    /// reach the server with the next periodic refresh, except the first.
    pub fn set_position(&self, position: Position) {
        self.inner.lock().position = Some(position);
        self.inner.changed.notify_one();
    }

    pub fn position(&self) -> Option<Position> {
        self.inner.lock().position
    }

    pub fn add_radio(&self, config: RadioConfig) -> RadioId {
        let mut state = self.inner.lock();
        let id = RadioId(state.next_id);
        state.next_id += 1;
        state.radios.push((id, config));
        self.inner.radios_changed(&state);
        id
    }

    /// Changes a radio's configuration. Returns `false` if there is no such radio.
    pub fn update_radio(&self, id: RadioId, update: impl FnOnce(&mut RadioConfig)) -> bool {
        let mut state = self.inner.lock();
        let Some((_, config)) = state.radios.iter_mut().find(|(i, _)| *i == id) else { return false };
        update(config);
        self.inner.radios_changed(&state);
        true
    }

    pub fn remove_radio(&self, id: RadioId) -> bool {
        let mut state = self.inner.lock();
        let before = state.radios.len();
        state.radios.retain(|(i, _)| *i != id);
        let removed = state.radios.len() != before;
        if removed {
            self.inner.radios_changed(&state);
        }
        removed
    }

    pub fn radio(&self, id: RadioId) -> Option<RadioConfig> {
        self.inner.lock().radios.iter().find(|(i, _)| *i == id).map(|(_, r)| r.clone())
    }

    pub fn radios(&self) -> Vec<(RadioId, RadioConfig)> {
        self.inner.lock().radios.clone()
    }

    /// Presses or releases push-to-talk. Transmission starts once the server knows the current
    /// transceivers ([`Event::PttOpened`]); a pending update is sent first.
    pub fn set_ptt(&self, pressed: bool) {
        let mut state = self.inner.lock();
        state.ptt_requested = pressed;
        if pressed {
            if state.synced() {
                self.inner.open_ptt(&mut state);
            } else {
                self.inner.changed.notify_one();
            }
        } else if state.ptt_open {
            state.ptt_open = false;
            self.inner.audio.set_ptt(false);
            let _ = self.inner.events.send(Event::PttClosed);
        }
    }

    /// Whether audio is being transmitted.
    pub fn ptt_open(&self) -> bool {
        self.inner.lock().ptt_open
    }

    /// Stops transmitting, closes the voice link and releases the callsign on the server.
    pub async fn disconnect(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
        self.inner.audio.set_ptt(false);
        self.session.disconnect().await;
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn radios_changed(&self, state: &State) {
        self.audio.set_radios(&state.radios);
        self.changed.notify_one();
    }

    fn open_ptt(&self, state: &mut State) {
        state.ptt_open = true;
        self.audio.set_ptt(true);
        let _ = self.events.send(Event::PttOpened);
    }

    /// Sends radio changes to the server, retrying failed updates, and forwards session state
    /// changes as events.
    async fn sync_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut session_state = self.shared.watch_state();
        // When to try again after a failed update. Any sync sends the whole current plan, so a
        // successful one in the meantime makes the retry unnecessary.
        let mut retry: Option<Instant> = None;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                changed = session_state.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let state = session_state.borrow_and_update().clone();
                    let _ = self.events.send(Event::Session(state));
                    continue;
                }
                _ = self.changed.notified() => {}
                _ = sleep_until(retry.unwrap_or_else(Instant::now)), if retry.is_some() => {}
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = sleep(DEBOUNCE) => {}
            }
            retry = (!self.sync().await).then(|| Instant::now() + self.shared.timing().transceiver_retry);
        }
    }

    /// Brings the server up to date with the radios. Returns `false` if that failed.
    async fn sync(&self) -> bool {
        let (plan, groups) = {
            let mut state = self.lock();
            let plan = state.plan();
            if !plan.differs_from(&state.acked) {
                // Only positions or the tx selection changed. The server's ids and frequencies
                // are current, so the new tx set applies at once; positions go out with the
                // next periodic refresh.
                self.audio.set_tx_transceivers(&plan.tx);
                self.shared.store_transceivers(plan.transceivers.clone());
                state.acked = plan;
                return true;
            }
            state.in_flight = true;
            // Ids are positions, so the update can give a transmitting id another frequency. The
            // server applies it before its reply arrives; until then, transmit only on ids that
            // mean the same frequency either way, so no audio goes out on a frequency nobody
            // selected.
            self.audio.set_tx_transceivers(&plan.stable_tx(&state.acked));
            let groups = match &plan.groups {
                Some(groups) => Some(groups.clone()),
                None if state.coupling_sent => Some(vec![CrossCoupleGroup { id: 0, transceiver_ids: vec![] }]),
                None => None,
            };
            (plan, groups)
        };
        let mut result = self.shared.set_transceivers(plan.transceivers.clone()).await;
        if result.is_ok()
            && let Some(groups) = &groups
        {
            result = self.shared.set_cross_couple_groups(groups.clone()).await;
        }

        let mut state = self.lock();
        state.in_flight = false;
        match result {
            Ok(()) => {
                state.coupling_sent |= groups.is_some();
                self.audio.set_tx_transceivers(&plan.tx);
                state.acked = plan;
                let _ = self.events.send(Event::TransceiversSynced);
                if state.ptt_requested && !state.ptt_open && state.synced() {
                    self.open_ptt(&mut state);
                }
                true
            }
            Err(e) => {
                tracing::warn!("transceiver update failed: {e}");
                let _ = self.events.send(Event::TransceiverSyncFailed(Arc::new(e)));
                false
            }
        }
    }
}
