mod support;

use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use afv::proto::api::CrossCoupleGroup;
use afv::proto::voice::{AudioRx, RxTransceiver};
use afv::radio::{Coupling, Output, OutputBus, Position, RadioConfig, Site, Sites};
use afv::{Event, Events, Frequency, SessionState, Timing};
use afv_audio::codec::Encoder;
use support::{FakeServer, eventually, fast_timing};

const POS: Position = Position { lat_deg: 60.19, lon_deg: 11.1, height_msl_m: 200.0, height_agl_m: 0.0 };

fn freq(mhz: &str) -> Frequency {
    mhz.parse().unwrap()
}

fn tx_radio(mhz: &str) -> RadioConfig {
    RadioConfig { tx: true, ..RadioConfig::pilot(freq(mhz)) }
}

/// Waits for an event matching `pred`, returning it; panics after two seconds.
async fn wait_event(events: &mut Events, pred: impl Fn(&Event) -> bool) -> Event {
    let wait = async {
        loop {
            match events.next().await {
                Some(e) if pred(&e) => return e,
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), wait).await.expect("timed out waiting for event")
}

#[tokio::test]
async fn radios_become_transceivers_with_dense_ids() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    client.set_position(POS);
    let com1 = client.add_radio(tx_radio("122.800"));
    let com2 = client.add_radio(RadioConfig::pilot(freq("121.500")));
    wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced)).await;
    let posted = server.last_transceivers().unwrap();
    let summary: Vec<_> = posted.iter().map(|t| (t.id, t.frequency, t.lat_deg)).collect();
    assert_eq!(summary, [(0, 122_800_000, 60.19), (1, 121_500_000, 60.19)]);
    assert_eq!(server.transceiver_post_count(), 1, "changes within the debounce window are sent together");

    assert!(client.remove_radio(com1));
    wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced)).await;
    assert_eq!(
        server.last_transceivers().unwrap().iter().map(|t| (t.id, t.frequency)).collect::<Vec<_>>(),
        [(0, 121_500_000)]
    );
    assert_eq!(client.radios().len(), 1);
    assert_eq!(client.radio(com2).unwrap().frequency, freq("121.500"));
    client.disconnect().await;
}

#[tokio::test]
async fn position_changes_wait_for_the_periodic_refresh() {
    let server = FakeServer::start().await;
    let client = server
        .client_builder()
        .timing(Timing { transceiver_refresh: Duration::from_millis(500), ..fast_timing() })
        .connect()
        .await
        .unwrap();
    let mut events = client.events();
    client.set_position(POS);
    client.add_radio(tx_radio("122.800"));
    wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced)).await;
    let posts = server.transceiver_post_count();

    client.set_position(Position { lat_deg: 61.0, ..POS });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.transceiver_post_count(), posts, "a move alone is not sent at once");
    client.set_ptt(true);
    assert!(client.ptt_open(), "a move does not hold PTT back");
    eventually("the refresh to carry the new position", || {
        server.last_transceivers().is_some_and(|t| t[0].lat_deg == 61.0)
    })
    .await;
    client.disconnect().await;
}

#[tokio::test]
async fn ptt_waits_for_the_server_to_know_the_frequency() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    client.set_position(POS);
    server.fake.transceiver_delay_ms.store(200, SeqCst);
    let com1 = client.add_radio(tx_radio("122.800"));
    client.set_ptt(true);
    assert!(!client.ptt_open());
    let first = wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced | Event::PttOpened)).await;
    assert!(matches!(first, Event::TransceiversSynced), "synced before PTT opened");
    wait_event(&mut events, |e| matches!(e, Event::PttOpened)).await;
    assert!(client.ptt_open());

    // Already synced: releasing and pressing again is immediate.
    client.set_ptt(false);
    assert!(!client.ptt_open());
    client.set_ptt(true);
    assert!(client.ptt_open());
    client.set_ptt(false);

    // A retune makes the next press wait again.
    while events.try_next().is_some() {}
    client.update_radio(com1, |r| r.frequency = freq("121.500"));
    client.set_ptt(true);
    assert!(!client.ptt_open());
    wait_event(&mut events, |e| matches!(e, Event::PttOpened)).await;
    assert_eq!(server.last_transceivers().unwrap()[0].frequency, 121_500_000);
    client.disconnect().await;
}

#[tokio::test]
async fn a_failed_update_keeps_ptt_closed_until_it_succeeds() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    client.set_position(POS);
    server.fake.fail_transceivers.store(true, SeqCst);
    client.add_radio(tx_radio("122.800"));
    client.set_ptt(true);
    wait_event(&mut events, |e| matches!(e, Event::TransceiverSyncFailed(_))).await;
    assert!(!client.ptt_open());

    server.fake.fail_transceivers.store(false, SeqCst);
    // Pressing PTT again retries at once.
    client.set_ptt(true);
    wait_event(&mut events, |e| matches!(e, Event::PttOpened)).await;
    client.disconnect().await;
}

#[tokio::test]
async fn transmits_captured_audio_on_the_tx_transceivers() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    client.set_position(POS);
    client.add_radio(RadioConfig::pilot(freq("121.500")));
    client.add_radio(tx_radio("122.800"));
    client.set_ptt(true);
    wait_event(&mut events, |e| matches!(e, Event::PttOpened)).await;

    // 200 ms of a tone from a 44.1 kHz stereo microphone, in 10 ms buffers.
    let audio = client.audio();
    let mut t = 0usize;
    let mut capture = |ms: usize| {
        for _ in 0..ms / 10 {
            let buf: Vec<f32> = (0..441)
                .flat_map(|_| {
                    t += 1;
                    let s = 0.3 * (t as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin();
                    [s, s]
                })
                .collect();
            audio.capture(&buf, 44_100, 2).unwrap();
        }
    };
    capture(200);
    eventually("audio to arrive", || server.fake.audio.lock().unwrap().len() >= 8).await;
    client.set_ptt(false);
    capture(100);
    eventually("the final packet", || server.fake.audio.lock().unwrap().last().is_some_and(|a| a.last_packet)).await;

    let sent = server.fake.audio.lock().unwrap().clone();
    assert!(sent.iter().all(|a| a.callsign == "SAS123" && a.transceivers == [1]), "only the tx radio's transceiver");
    assert!(sent.windows(2).all(|w| w[1].sequence == w[0].sequence + 1), "consecutive sequence numbers");
    assert_eq!(sent.iter().filter(|a| a.last_packet).count(), 1);
    client.disconnect().await;
}

#[tokio::test]
async fn received_audio_is_played_and_reported_per_radio() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    client.set_position(POS);
    let com1 = client.add_radio(RadioConfig { effects: false, ..RadioConfig::pilot(freq("122.800")) });
    let speaker = client.add_radio(RadioConfig {
        output: Output { bus: OutputBus::Speaker, ..Output::default() },
        ..RadioConfig::pilot(freq("122.800"))
    });
    wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced)).await;
    // The fake learns the client's address from its first heartbeat.
    eventually("a heartbeat", || server.fake.heartbeats.load(SeqCst) > 0).await;

    let mut encoder = Encoder::new().unwrap();
    let audio = client.audio();
    let mut loudest = 0f32;
    let mut out = vec![0.0; 960 * 2];
    for i in 0..40u32 {
        if i < 25 {
            let tone: [f32; 960] = std::array::from_fn(|n| {
                0.3 * ((i as usize * 960 + n) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin()
            });
            server
                .send_audio(&AudioRx {
                    callsign: "NAX45F".into(),
                    sequence: i,
                    audio: encoder.encode(&tone).unwrap().to_vec(),
                    last_packet: i == 24,
                    transceivers: vec![RxTransceiver { id: 0, frequency: 122_800_000, distance_ratio: 1.0 }],
                })
                .await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        audio.render(OutputBus::Headset, &mut out, 48_000, 2).unwrap();
        loudest = out.iter().fold(loudest, |m, s| m.max(s.abs()));
    }
    assert!(loudest > 0.1, "heard the transmission (peak {loudest})");
    let started = wait_event(&mut events, |e| matches!(e, Event::RxStarted { .. })).await;
    assert!(matches!(started, Event::RxStarted { radio, ref callsign } if radio == com1 && callsign == "NAX45F"));
    wait_event(&mut events, |e| matches!(e, Event::RxEnded { radio, .. } if *radio == com1)).await;
    // The speaker bus was never rendered, so its radio reports nothing.
    assert!(
        std::iter::from_fn(|| events.try_next())
            .all(|e| !matches!(e, Event::RxStarted { radio, .. } if radio == speaker))
    );
    client.disconnect().await;
}

#[tokio::test]
async fn coupled_atc_radios_send_cross_couple_groups() {
    let server = FakeServer::start().await;
    let client = server.client_builder().timing(Timing::atc()).api_url(&server.api_url).connect().await.unwrap();
    let mut events = client.events();
    let sites = |lats: &[f64]| {
        Sites::Fixed(lats.iter().map(|&lat| Site { position: Position { lat_deg: lat, ..POS }, rco: false }).collect())
    };
    let twr = client.add_radio(RadioConfig {
        tx: true,
        coupling: Coupling::Across,
        ..RadioConfig::atc(freq("118.300"), sites(&[60.1, 60.2]))
    });
    client.add_radio(RadioConfig {
        tx: true,
        coupling: Coupling::Across,
        ..RadioConfig::atc(freq("120.450"), sites(&[59.0]))
    });
    wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced)).await;
    assert_eq!(
        server.fake.cross_couple_posts.lock().unwrap().last().unwrap(),
        &[CrossCoupleGroup { id: 0, transceiver_ids: vec![0, 1, 2] }]
    );

    // Turning coupling off sends an empty group rather than nothing.
    client.update_radio(twr, |r| r.coupling = Coupling::None);
    wait_event(&mut events, |e| matches!(e, Event::TransceiversSynced)).await;
    assert_eq!(
        server.fake.cross_couple_posts.lock().unwrap().last().unwrap(),
        &[CrossCoupleGroup { id: 0, transceiver_ids: vec![2] }]
    );
    client.disconnect().await;
}

#[tokio::test]
async fn session_state_changes_are_events() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    server.fake.answer_heartbeats.store(false, SeqCst);
    wait_event(&mut events, |e| matches!(e, Event::Session(SessionState::Degraded))).await;
    server.fake.answer_heartbeats.store(true, SeqCst);
    wait_event(&mut events, |e| matches!(e, Event::Session(SessionState::Connected))).await;
    client.disconnect().await;
}
