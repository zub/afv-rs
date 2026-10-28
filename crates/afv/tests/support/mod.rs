//! In-process fake of the AFV API and voice servers.

#![allow(dead_code)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use afv::proto::api::{
    AuthRequest, CrossCoupleGroup, PostCallsignResponse, Station, StationTransceiver, Transceiver, VoiceServer,
};
use afv::proto::voice::{AudioRx, AudioTx, HeartbeatAck};
use afv::proto::{Channel, ChannelConfig, Message};
use afv::{SessionState, Timing};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::net::{TcpListener, UdpSocket};

pub const USERNAME: &str = "1234567";
pub const PASSWORD: &str = "hunter2";

#[derive(Default)]
pub struct Fake {
    pub token_ttl_secs: AtomicU64,
    pub auths: AtomicU32,
    pub allocations: AtomicU32,
    pub releases: AtomicU32,
    pub heartbeats: AtomicU32,
    /// Reply to heartbeats with `HA`.
    pub answer_heartbeats: AtomicBool,
    /// Ignore every datagram.
    pub silent: AtomicBool,
    pub transceiver_posts: Mutex<Vec<Vec<Transceiver>>>,
    pub cross_couple_posts: Mutex<Vec<Vec<CrossCoupleGroup>>>,
    pub audio: Mutex<Vec<AudioTx>>,
    tokens: Mutex<HashSet<String>>,
    channel: Mutex<Option<Channel>>,
    client: Mutex<Option<SocketAddr>>,
}

pub struct FakeServer {
    pub api_url: String,
    pub fake: Arc<Fake>,
    udp: Arc<UdpSocket>,
}

impl FakeServer {
    pub async fn start() -> Self {
        let fake = Arc::new(Fake::default());
        fake.token_ttl_secs.store(3600, SeqCst);
        fake.answer_heartbeats.store(true, SeqCst);

        let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let udp_addr = udp.local_addr().unwrap();
        tokio::spawn(voice_loop(fake.clone(), udp.clone()));

        let app = Router::new()
            .route("/api/v1/auth", post(auth))
            .route("/api/v1/users/{user}/callsigns/{callsign}", post(allocate).delete(release))
            .route("/api/v1/users/{user}/callsigns/{callsign}/transceivers", post(transceivers))
            .route("/api/v1/users/{user}/callsigns/{callsign}/crossCoupleGroups", post(cross_couple))
            .route("/api/v1/stations/aliased", get(aliased))
            .route("/api/v1/stations/byName/{name}", get(station))
            .route("/api/v1/stations/byName/{name}/transceivers/allDistinctObeyExclusions", get(station_transceivers))
            .route("/api/v1/stations/byName/{name}/vccsStations", get(vccs))
            .with_state((fake.clone(), udp_addr));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Self { api_url, fake, udp }
    }

    /// Sends an `AR` frame to the client using the current session keys.
    pub async fn send_audio(&self, audio: &AudioRx) {
        let client = self.fake.client.lock().unwrap().expect("client has not sent anything yet");
        let datagram = self.fake.channel.lock().unwrap().as_mut().unwrap().seal(audio).unwrap();
        self.udp.send_to(&datagram, client).await.unwrap();
    }

    pub fn builder(&self) -> afv::ClientBuilder {
        afv::Session::builder()
            .api_url(&self.api_url)
            .credentials(USERNAME, PASSWORD)
            .callsign("SAS123")
            .timing(fast_timing())
    }
}

pub fn fast_timing() -> Timing {
    Timing {
        heartbeat_interval: Duration::from_millis(30),
        silence_timeout: Duration::from_secs(5),
        degraded_after: 5,
        timeout_after: 10,
        transceiver_refresh: Duration::from_millis(200),
        token_refresh_margin: Duration::from_secs(60),
        reconnect_backoff: Duration::from_millis(20),
        reconnect_backoff_max: Duration::from_millis(100),
        http_timeout: Duration::from_secs(2),
    }
}

/// Polls `cond` until it holds, panicking after two seconds.
pub async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits for a session state matching `pred`.
pub async fn wait_state(session: &afv::Session, pred: impl Fn(&SessionState) -> bool) -> SessionState {
    let mut rx = session.watch_state();
    let state = tokio::time::timeout(Duration::from_secs(2), rx.wait_for(|s| pred(s)))
        .await
        .unwrap_or_else(|_| panic!("timed out; state is {:?}", session.state()))
        .unwrap();
    state.clone()
}

fn make_token(ttl: u64) -> String {
    let exp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + ttl;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"{USERNAME}","exp":{exp},"n":{}}}"#, rand_suffix()));
    format!("{header}.{payload}.")
}

fn rand_suffix() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, SeqCst)
}

type AppState = State<(Arc<Fake>, SocketAddr)>;

fn authorized(fake: &Fake, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) else { return false };
    value.strip_prefix("Bearer ").is_some_and(|t| fake.tokens.lock().unwrap().contains(t))
}

async fn auth(State((fake, _)): AppState, Json(req): Json<AuthRequest>) -> Response {
    if req.username != USERNAME || req.password != PASSWORD {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    fake.auths.fetch_add(1, SeqCst);
    let token = make_token(fake.token_ttl_secs.load(SeqCst));
    fake.tokens.lock().unwrap().insert(token.clone());
    token.into_response()
}

async fn allocate(State((fake, udp_addr)): AppState, headers: HeaderMap) -> Response {
    if !authorized(&fake, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let n = fake.allocations.fetch_add(1, SeqCst) as u8 + 1;
    let config = ChannelConfig::new(format!("tag-{n}"), [n; 32], [n.wrapping_add(100); 32]);
    *fake.channel.lock().unwrap() = Some(Channel::new(&config.peer()));
    Json(PostCallsignResponse {
        voice_server: VoiceServer { address_ipv4: udp_addr.to_string(), address_ipv6: None, channel_config: config },
    })
    .into_response()
}

async fn release(State((fake, _)): AppState, headers: HeaderMap) -> StatusCode {
    if !authorized(&fake, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    fake.releases.fetch_add(1, SeqCst);
    StatusCode::OK
}

async fn transceivers(
    State((fake, _)): AppState,
    headers: HeaderMap,
    Json(body): Json<Vec<Transceiver>>,
) -> StatusCode {
    if !authorized(&fake, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    fake.transceiver_posts.lock().unwrap().push(body);
    StatusCode::OK
}

async fn cross_couple(
    State((fake, _)): AppState,
    headers: HeaderMap,
    Json(body): Json<Vec<CrossCoupleGroup>>,
) -> StatusCode {
    if !authorized(&fake, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    fake.cross_couple_posts.lock().unwrap().push(body);
    StatusCode::OK
}

fn engm_twr() -> Station {
    Station { id: "1".into(), name: "ENGM_TWR".into(), frequency: Some(118_300_000), frequency_alias: None }
}

async fn aliased() -> Json<Vec<Station>> {
    Json(vec![Station { frequency_alias: Some(199_998_000), ..engm_twr() }])
}

async fn station(Path(name): Path<String>) -> Response {
    if name == "ENGM_TWR" { Json(engm_twr()).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn station_transceivers(Path(_): Path<String>) -> Json<Vec<StationTransceiver>> {
    Json(vec![StationTransceiver {
        id: "a".into(),
        name: "Site A".into(),
        lat_deg: 60.19,
        lon_deg: 11.1,
        height_msl_m: 210.0,
        height_agl_m: 25.0,
        rco: false,
    }])
}

async fn vccs(Path(_): Path<String>) -> Json<Vec<Station>> {
    Json(vec![
        Station { id: "2".into(), name: "ENGM_GND".into(), frequency: Some(121_600_000), frequency_alias: None },
        Station { id: "3".into(), name: "ENOR_CTR".into(), frequency: Some(120_450_000), frequency_alias: None },
    ])
}

async fn voice_loop(fake: Arc<Fake>, udp: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65_536];
    loop {
        let (len, from) = udp.recv_from(&mut buf).await.unwrap();
        if fake.silent.load(SeqCst) {
            continue;
        }
        let reply = {
            let mut channel = fake.channel.lock().unwrap();
            let Some(channel) = channel.as_mut() else { continue };
            let Ok(message) = channel.open(&buf[..len]) else { continue };
            *fake.client.lock().unwrap() = Some(from);
            match message {
                Message::Heartbeat(_) => {
                    fake.heartbeats.fetch_add(1, SeqCst);
                    fake.answer_heartbeats.load(SeqCst).then(|| channel.seal(&HeartbeatAck).unwrap())
                }
                Message::AudioTx(audio) => {
                    fake.audio.lock().unwrap().push(audio);
                    None
                }
                _ => None,
            }
        };
        if let Some(reply) = reply {
            let _ = udp.send_to(&reply, from).await;
        }
    }
}
