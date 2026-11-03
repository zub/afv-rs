mod support;

use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use afv::proto::api::{CrossCoupleGroup, Transceiver};
use afv::proto::voice::{AudioRx, RxTransceiver};
use afv::{AuthError, Error, LinkLoss, SessionState};
use support::{FakeServer, eventually, fast_timing, wait_state};

fn transceiver(id: u16, frequency: u32) -> Transceiver {
    Transceiver { id, frequency, lat_deg: 59.9, lon_deg: 10.7, height_msl_m: 100.0, height_agl_m: 0.0, rco: false }
}

#[tokio::test]
async fn connects_and_keeps_heartbeating() {
    let server = FakeServer::start().await;
    let session = server.builder().connect().await.unwrap();
    assert!(matches!(session.state(), SessionState::Connected));
    eventually("heartbeats", || server.fake.heartbeats.load(SeqCst) >= 5).await;
    assert!(matches!(session.state(), SessionState::Connected));
    assert_eq!(server.fake.auths.load(SeqCst), 1);
    assert_eq!(server.fake.allocations.load(SeqCst), 1);
}

#[tokio::test]
async fn wrong_password_is_reported() {
    let server = FakeServer::start().await;
    let err = server.builder().credentials(support::USERNAME, "wrong").connect().await.err().unwrap();
    assert!(matches!(err, Error::Auth(AuthError::BadPassword)), "{err:?}");
}

#[tokio::test]
async fn token_expiring_within_margin_is_a_clock_error() {
    let server = FakeServer::start().await;
    let timing = afv::Timing { token_refresh_margin: Duration::from_secs(7200), ..fast_timing() };
    let err = server.builder().timing(timing).connect().await.err().unwrap();
    assert!(matches!(err, Error::Auth(AuthError::TokenExpired)), "{err:?}");
}

#[tokio::test]
async fn transceivers_are_posted_and_refreshed() {
    let server = FakeServer::start().await;
    let session = server.builder().connect().await.unwrap();
    session.set_transceivers(vec![transceiver(0, 122_800_000), transceiver(1, 0)]).await.unwrap();
    session.set_cross_couple_groups(vec![CrossCoupleGroup { id: 0, transceiver_ids: vec![0] }]).await.unwrap();

    // Frequency 0 is filtered out.
    assert_eq!(server.fake.transceiver_posts.lock().unwrap()[0], vec![transceiver(0, 122_800_000)]);
    eventually("periodic refresh", || server.fake.transceiver_posts.lock().unwrap().len() >= 3).await;
    eventually("cross-couple refresh", || server.fake.cross_couple_posts.lock().unwrap().len() >= 2).await;
}

#[tokio::test]
async fn audio_flows_both_ways() {
    let server = FakeServer::start().await;
    let session = server.builder().connect().await.unwrap();
    let mut incoming = session.take_incoming_audio().unwrap();
    assert!(session.take_incoming_audio().is_none());

    session.send_audio(7, &[1, 2, 3], false, &[0]).unwrap();
    eventually("AT at server", || !server.fake.audio.lock().unwrap().is_empty()).await;
    let sent = server.fake.audio.lock().unwrap()[0].clone();
    assert_eq!(
        (sent.callsign.as_str(), sent.sequence, &sent.audio[..], sent.transceivers),
        ("SAS123", 7, &[1, 2, 3][..], vec![0])
    );

    let rx = AudioRx {
        callsign: "ENGM_TWR".into(),
        sequence: 99,
        audio: vec![9; 40],
        last_packet: true,
        transceivers: vec![RxTransceiver { id: 0, frequency: 118_300_000, distance_ratio: 0.8 }],
    };
    server.send_audio(&rx).await;
    let got = tokio::time::timeout(Duration::from_secs(2), incoming.recv()).await.unwrap().unwrap();
    assert_eq!(got, rx);
}

#[tokio::test]
async fn degrades_and_recovers() {
    let server = FakeServer::start().await;
    let timing = afv::Timing { timeout_after: 1000, ..fast_timing() };
    let session = server.builder().timing(timing).connect().await.unwrap();

    server.fake.answer_heartbeats.store(false, SeqCst);
    wait_state(&session, |s| matches!(s, SessionState::Degraded)).await;
    server.fake.answer_heartbeats.store(true, SeqCst);
    wait_state(&session, |s| matches!(s, SessionState::Connected)).await;
    assert_eq!(server.fake.allocations.load(SeqCst), 1);
}

#[tokio::test]
async fn transceiver_posts_never_overlap() {
    // Slow answers and a refresh faster than them: without ordering, a refresh carrying the old
    // set could reach the server after the update that replaced it.
    let server = FakeServer::start().await;
    let timing = afv::Timing { transceiver_refresh: Duration::from_millis(30), ..fast_timing() };
    let session = server.builder().timing(timing).connect().await.unwrap();
    server.fake.transceiver_delay_ms.store(50, SeqCst);
    session.set_transceivers(vec![transceiver(0, 122_800_000)]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.set_transceivers(vec![transceiver(0, 121_500_000)]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(server.transceiver_post_count() >= 4);
    assert_eq!(server.fake.max_transceiver_posts_in_flight.load(SeqCst), 1);
    assert_eq!(server.last_transceivers().unwrap(), [transceiver(0, 121_500_000)]);
}

#[tokio::test]
async fn heartbeat_timeout_reconnects_and_restores_transceivers() {
    let server = FakeServer::start().await;
    let session = server.builder().connect().await.unwrap();
    session.set_transceivers(vec![transceiver(0, 122_800_000)]).await.unwrap();
    let posts_before = server.fake.transceiver_posts.lock().unwrap().len();

    server.fake.answer_heartbeats.store(false, SeqCst);
    let state = wait_state(&session, |s| matches!(s, SessionState::Reconnecting { .. })).await;
    assert!(matches!(state, SessionState::Reconnecting { reason: LinkLoss::HeartbeatTimeout, .. }), "{state:?}");
    server.fake.answer_heartbeats.store(true, SeqCst);

    wait_state(&session, |s| matches!(s, SessionState::Connected)).await;
    assert!(server.fake.releases.load(SeqCst) >= 1, "old callsign released");
    assert!(server.fake.allocations.load(SeqCst) >= 2);
    assert!(server.fake.auths.load(SeqCst) >= 2, "re-authenticated");
    eventually("transceivers re-posted", || server.fake.transceiver_posts.lock().unwrap().len() > posts_before).await;

    // The new link works with the new keys.
    let heartbeats = server.fake.heartbeats.load(SeqCst);
    eventually("heartbeats on new link", || server.fake.heartbeats.load(SeqCst) > heartbeats + 2).await;
}

#[tokio::test]
async fn silence_reconnects() {
    let server = FakeServer::start().await;
    let timing = afv::Timing { silence_timeout: Duration::from_millis(200), timeout_after: 1000, ..fast_timing() };
    let session = server.builder().timing(timing).connect().await.unwrap();

    server.fake.silent.store(true, SeqCst);
    let state = wait_state(&session, |s| matches!(s, SessionState::Reconnecting { .. })).await;
    assert!(matches!(state, SessionState::Reconnecting { reason: LinkLoss::Silence, .. }), "{state:?}");
    server.fake.silent.store(false, SeqCst);
    wait_state(&session, |s| matches!(s, SessionState::Connected)).await;
}

#[tokio::test]
async fn token_is_refreshed_before_expiry() {
    let server = FakeServer::start().await;
    // Tokens live 3600 s (whole seconds in `exp`); refreshing 3598.5 s early means after
    // 0.5–1.5 s, while a fresh token still has more than the margin left.
    let timing = afv::Timing { token_refresh_margin: Duration::from_millis(3_598_500), ..fast_timing() };
    let session = server.builder().timing(timing).connect().await.unwrap();
    eventually("second login", || server.fake.auths.load(SeqCst) >= 2).await;
    assert!(matches!(session.state(), SessionState::Connected));
    // Requests keep working with the new token.
    session.set_transceivers(vec![transceiver(0, 121_500_000)]).await.unwrap();
    assert_eq!(server.fake.allocations.load(SeqCst), 1);
}

#[tokio::test]
async fn disconnect_releases_the_callsign() {
    let server = FakeServer::start().await;
    let session = server.builder().connect().await.unwrap();
    let mut state = session.watch_state();
    session.disconnect().await;
    assert_eq!(server.fake.releases.load(SeqCst), 1);
    assert!(matches!(*state.borrow_and_update(), SessionState::Disconnected(None)));
}

#[tokio::test]
async fn dropping_the_session_releases_the_callsign() {
    let server = FakeServer::start().await;
    drop(server.builder().connect().await.unwrap());
    eventually("release after drop", || server.fake.releases.load(SeqCst) == 1).await;
}

#[tokio::test]
async fn station_lookups() {
    let server = FakeServer::start().await;
    let session = server.builder().connect().await.unwrap();

    assert_eq!(session.station("ENGM_TWR").await.unwrap().unwrap().frequency, Some(118_300_000));
    assert!(session.station("NOPE").await.unwrap().is_none());
    assert_eq!(session.station_transceivers("ENGM_TWR").await.unwrap().len(), 1);
    let vccs: Vec<_> = session.vccs("ENGM_TWR").await.unwrap().into_iter().map(|s| s.name).collect();
    assert_eq!(vccs, ["ENGM_GND", "ENOR_CTR"]);
    assert_eq!(session.aliased_stations().await.unwrap()[0].frequency_alias, Some(199_998_000));
}
