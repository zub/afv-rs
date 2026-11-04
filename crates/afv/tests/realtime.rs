//! The real-time bridge end to end: a thread plays the part of an audio device, the fake server
//! echoes the transmission back.

mod support;

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use afv::radio::{OutputBus, Position, RadioAudio, RadioConfig};
use afv::realtime::{self, Format};
use afv::{Event, Events};
use support::FakeServer;

const POS: Position = Position { lat_deg: 60.19, lon_deg: 11.1, height_msl_m: 200.0, height_agl_m: 0.0 };

async fn wait_event(events: &mut Events, timeout: Duration, pred: impl Fn(&Event) -> bool) -> Event {
    let wait = async {
        loop {
            match events.next().await {
                Some(e) if pred(&e) => return e,
                Some(_) => {}
                None => panic!("event stream closed"),
            }
        }
    };
    tokio::time::timeout(timeout, wait).await.expect("timed out waiting for event")
}

#[tokio::test(flavor = "multi_thread")]
async fn transmission_round_trips_through_the_bridge() {
    let server = FakeServer::start().await;
    server.fake.echo.store(true, SeqCst);
    let client = server.client_builder().connect().await.unwrap();
    let mut events = client.events();
    client.set_position(POS);
    client.add_radio(RadioConfig {
        tx: true,
        audio: RadioAudio { effects: false, ..RadioAudio::default() },
        ..RadioConfig::pilot("122.800".parse().unwrap())
    });

    let mic = Format { sample_rate: 44_100, channels: 1 };
    let speaker = Format { sample_rate: 48_000, channels: 2 };
    let (bridge, capture, playback) =
        realtime::bridge(&client.audio(), Some(mic), &[(OutputBus::Headset, speaker)]).unwrap();
    let (mut capture, mut playback) = (capture.unwrap(), playback.into_iter().next().unwrap());

    // The "device": every 10 ms, 441 microphone samples of a tone in and 480 stereo frames out.
    let stop = Arc::new(AtomicBool::new(false));
    let output = Arc::new(Mutex::new(Vec::new()));
    let device = {
        let (stop, output) = (stop.clone(), output.clone());
        std::thread::spawn(move || {
            let mut t = 0usize;
            let mut out = vec![0.0f32; 960];
            while !stop.load(SeqCst) {
                let mic: Vec<f32> = (0..441)
                    .map(|_| {
                        t += 1;
                        0.3 * (t as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin()
                    })
                    .collect();
                capture.push(&mic);
                playback.fill(&mut out);
                output.lock().unwrap().extend(out.chunks(2).map(|f| f[0]));
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    client.set_ptt(true);
    wait_event(&mut events, Duration::from_secs(2), |e| matches!(e, Event::PttOpened)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    client.set_ptt(false);
    let heard_from = output.lock().unwrap().len();
    let started = wait_event(&mut events, Duration::from_secs(3), |e| matches!(e, Event::RxStarted { .. })).await;
    assert!(matches!(started, Event::RxStarted { ref callsign, .. } if callsign == "ECHO"));
    wait_event(&mut events, Duration::from_secs(3), |e| matches!(e, Event::RxEnded { .. })).await;
    stop.store(true, SeqCst);
    device.join().unwrap();
    drop(bridge);

    let sent = server.fake.audio.lock().unwrap().len();
    assert!((15..=25).contains(&sent), "about 400 ms transmitted, got {sent} packets");
    let peak = output.lock().unwrap()[heard_from..].iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.1, "the echo was played (peak {peak})");
    client.disconnect().await;
}

#[tokio::test]
async fn formats_no_device_has_are_refused_up_front() {
    let server = FakeServer::start().await;
    let client = server.client_builder().connect().await.unwrap();
    let audio = client.audio();
    let good = Format { sample_rate: 48_000, channels: 2 };
    for bad in [
        Format { sample_rate: 48_000, channels: 0 },
        Format { sample_rate: 0, channels: 1 },
        Format { sample_rate: u32::MAX, channels: 1 },
        Format { sample_rate: 48_000, channels: u16::MAX },
    ] {
        assert!(realtime::bridge(&audio, Some(bad), &[(OutputBus::Headset, good)]).is_err(), "mic {bad:?}");
        assert!(realtime::bridge(&audio, None, &[(OutputBus::Headset, good), (OutputBus::Speaker, bad)]).is_err());
    }
    assert!(realtime::bridge(&audio, Some(good), &[(OutputBus::Headset, good)]).is_ok());
    client.disconnect().await;
}
