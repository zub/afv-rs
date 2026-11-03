use std::sync::Arc;
use std::time::{Duration, Instant};

use afv_audio::codec::{Decoder, Encoder};
use afv_audio::radio::{Output, Signal};
use afv_audio::{Bus, Carrier, EffectSamples, FRAME_SAMPLES, Frame, Pan, Radio, RadioSettings, RxChange, Transmitter};
use afv_proto::voice::RxTransceiver;

const FREQ: u32 = 122_800_000;

fn sine(frame_index: usize, freq: f32, amp: f32) -> Frame {
    std::array::from_fn(|i| {
        let t = (frame_index * FRAME_SAMPLES + i) as f32 / 48_000.0;
        amp * (std::f32::consts::TAU * freq * t).sin()
    })
}

fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
}

fn rx(frequency: u32, distance_ratio: f32) -> Vec<RxTransceiver> {
    vec![RxTransceiver { id: 0, frequency, distance_ratio }]
}

fn clean(frequency: u32) -> RadioSettings {
    RadioSettings { frequency, effects: false, ..RadioSettings::default() }
}

#[test]
fn opus_roundtrip_preserves_a_tone() {
    let mut enc = Encoder::new().unwrap();
    let mut dec = Decoder::new().unwrap();
    let mut out = [0.0; FRAME_SAMPLES];
    for i in 0..10 {
        let packet = enc.encode(&sine(i, 440.0, 0.5)).unwrap().to_vec();
        assert!(packet.len() < 200, "16 kbit/s frames are small, got {}", packet.len());
        dec.decode(&packet, &mut out).unwrap();
    }
    let level = rms(&out);
    assert!((level - 0.5 / 2f32.sqrt()).abs() < 0.08, "rms {level}");
    dec.conceal(&mut out).unwrap();
}

#[test]
fn transmitter_sequence_ticks_every_frame_and_sends_a_tail() {
    let mut tx = Transmitter::new().unwrap();
    let mic = sine(0, 440.0, 0.3);
    assert!(tx.process(&mic).unwrap().is_none());
    assert!(tx.process(&mic).unwrap().is_none());

    tx.set_ptt(true);
    let first = tx.process(&mic).unwrap().unwrap();
    assert_eq!((first.sequence, first.last_packet), (2, false));
    assert_eq!(tx.process(&mic).unwrap().unwrap().sequence, 3);

    tx.set_ptt(false);
    let tail = tx.process(&mic).unwrap().unwrap();
    assert_eq!((tail.sequence, tail.last_packet), (4, true));
    assert!(tx.process(&mic).unwrap().is_none());
    tx.set_ptt(true);
    assert_eq!(tx.process(&mic).unwrap().unwrap().sequence, 6);
}

#[test]
fn transmitter_applies_gain_with_clipping_and_meters() {
    let mut tx = Transmitter::new().unwrap();
    tx.set_gain(4.0);
    tx.process(&[0.5; FRAME_SAMPLES]).unwrap();
    assert!(tx.last_frame().iter().all(|&s| s == 1.0));
    assert!(tx.meter().peak() == 1.0);
    assert!(tx.meter().level() > 0.0);
}

fn process(
    radio: &mut Radio,
    streams: &[(&Frame, &[RxTransceiver])],
    fx: &EffectSamples,
) -> (Frame, Frame, Option<RxChange>) {
    let (mut l, mut r) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
    // Talker `i` transmits 150·i Hz off frequency.
    let signals = streams.iter().enumerate().map(|(i, &(audio, transceivers))| Signal {
        audio,
        transceivers,
        carrier: Carrier { offset_hz: 150 * i as i16, phase: 0 },
    });
    let change = radio.process(signals, fx, Output::Stereo { left: &mut l, right: &mut r });
    (l, r, change)
}

#[test]
fn radio_without_effects_passes_matching_voice_only() {
    let fx = EffectSamples::default();
    let voice = sine(0, 440.0, 0.4);
    let mut radio = Radio::new(RadioSettings { gain: 0.5, ..clean(FREQ) });

    let (l, r, change) = process(&mut radio, &[(&voice, &rx(FREQ, 1.0))], &fx);
    assert_eq!(change, Some(RxChange::Started));
    assert!(l.iter().zip(&voice).all(|(o, v)| *o == 0.5 * v));
    assert_eq!(l, r);

    let (l, _, change) = process(&mut radio, &[(&voice, &rx(FREQ + 25_000, 1.0))], &fx);
    assert_eq!(change, Some(RxChange::Ended));
    assert!(l.iter().all(|&s| s == 0.0));
}

#[test]
fn radio_pan_mute_and_transmit() {
    let fx = EffectSamples::default();
    let voice = sine(0, 440.0, 0.4);
    let streams = [(&voice, &rx(FREQ, 1.0)[..])];

    let mut radio = Radio::new(RadioSettings { pan: Pan::Left, ..clean(FREQ) });
    let (l, r, _) = process(&mut radio, &streams, &fx);
    assert!(rms(&l) > 0.1 && rms(&r) == 0.0);

    let mut radio = Radio::new(RadioSettings { muted: true, ..clean(FREQ) });
    let (l, _, change) = process(&mut radio, &streams, &fx);
    assert_eq!((rms(&l), change), (0.0, Some(RxChange::Started)), "muted radios still detect reception");

    let mut radio = Radio::new(clean(FREQ));
    radio.set_transmitting(true);
    let (l, _, change) = process(&mut radio, &streams, &fx);
    assert_eq!((rms(&l), change), (0.0, None));
}

#[test]
fn simultaneous_vhf_transmissions_beat_against_each_other() {
    let fx = EffectSamples::default();
    let silence = [0.0; FRAME_SAMPLES];
    let mut radio = Radio::new(clean(FREQ));
    let both = [(&silence, &rx(FREQ, 1.0)[..]), (&silence, &rx(FREQ, 1.0)[..])];
    process(&mut radio, &both, &fx);
    let (l, _, _) = process(&mut radio, &both, &fx);
    // Two unmodulated carriers 150 Hz apart: a loud heterodyne with a 150 Hz period.
    assert!(rms(&l) > 0.3, "rms {}", rms(&l));
    let period = 48_000 / 150;
    assert!(l.iter().zip(&l[period..]).all(|(a, b)| (a - b).abs() < 0.02));

    // One talker alone is unaffected.
    let (l, _, _) = process(&mut radio, &both[..1], &fx);
    assert_eq!(rms(&l), 0.0);
}

#[test]
fn simultaneous_hf_transmissions_just_add_up() {
    let fx = EffectSamples::default();
    let hf = 8_891_000;
    let (a, b) = (sine(0, 440.0, 0.3), sine(0, 700.0, 0.2));
    let mut radio = Radio::new(clean(hf));
    let (l, _, _) = process(&mut radio, &[(&a, &rx(hf, 1.0)), (&b, &rx(hf, 0.5))], &fx);
    assert!(l.iter().zip(a.iter().zip(&b)).all(|(o, (a, b))| (o - (a + b)).abs() < 1e-6));
}

#[test]
fn effects_add_noise_and_click() {
    let ones: Arc<[f32]> = vec![1.0; 4800].into();
    let fx = EffectSamples { vhf_noise: Some(ones.clone()), click: Some(ones), ..EffectSamples::default() };
    let silence = [0.0; FRAME_SAMPLES];
    let mut radio = Radio::new(RadioSettings { frequency: FREQ, gain: 2.0, ..RadioSettings::default() });

    // Silent voice: the output is the VHF noise bed alone, 0.17 × gain.
    let (l, _, _) = process(&mut radio, &[(&silence, &rx(FREQ, 1.0))], &fx);
    assert!(l.iter().all(|&s| (s - 0.34).abs() < 1e-6), "{}", l[0]);

    // Reception ends: the squelch click plays at 1.3 × gain.
    let (l, _, change) = process(&mut radio, &[], &fx);
    assert_eq!(change, Some(RxChange::Ended));
    assert!(l.iter().all(|&s| (s - 2.6).abs() < 1e-6));
}

#[test]
fn weak_signals_sound_quieter_and_use_the_strongest_transceiver() {
    let fx = EffectSamples::default();
    let settings = RadioSettings { frequency: FREQ, ..RadioSettings::default() };
    let level = |transceivers: &[RxTransceiver]| {
        let mut radio = Radio::new(settings.clone());
        let mut total = 0.0;
        for i in 0..20 {
            let voice = sine(i, 440.0, 0.3);
            let (l, _, _) = process(&mut radio, &[(&voice, transceivers)], &fx);
            total += rms(&l);
        }
        total
    };
    let strong = level(&rx(FREQ, 1.0));
    let weak = level(&rx(FREQ, 0.1));
    assert!(weak < strong * 0.8, "weak {weak} vs strong {strong}");

    let both = [
        RxTransceiver { id: 0, frequency: FREQ, distance_ratio: 0.1 },
        RxTransceiver { id: 1, frequency: FREQ, distance_ratio: 1.0 },
    ];
    assert_eq!(level(&both), strong);
}

#[test]
fn bus_plays_a_received_transmission_end_to_end() {
    let mut tx = Transmitter::new().unwrap();
    let mut bus = Bus::new(EffectSamples::default());
    bus.set_radio(1, clean(FREQ));
    bus.set_radio(2, clean(121_500_000));

    let start = Instant::now();
    let mut changes = Vec::new();
    let (mut l, mut r) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
    let mut loud_frames = 0;
    tx.set_ptt(true);
    for i in 0..40 {
        if i == 30 {
            tx.set_ptt(false);
        }
        let now = start + Duration::from_millis(20 * i as u64);
        if let Some(p) = tx.process(&sine(i, 440.0, 0.3)).unwrap() {
            bus.push("SAS123", p.sequence, p.opus, p.last_packet, &rx(FREQ, 1.0), now).unwrap();
        }
        bus.render_stereo(&mut l, &mut r, |id, callsign, c| changes.push((i, id, callsign.to_owned(), c)));
        if rms(&l) > 0.05 {
            loud_frames += 1;
        }
    }
    assert!(loud_frames >= 25, "heard {loud_frames} frames");
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert_eq!((changes[0].1, &*changes[0].2, changes[0].3), (1, "SAS123", RxChange::Started));
    assert_eq!((changes[1].1, &*changes[1].2, changes[1].3), (1, "SAS123", RxChange::Ended));
    assert_eq!(bus.radio(2).unwrap().receiving(), 0);

    bus.purge_idle(start + Duration::from_secs(120), afv_audio::bus::STREAM_IDLE_TIMEOUT);
}

#[test]
fn bus_reports_each_talker_per_radio() {
    let mut bus = Bus::new(EffectSamples::default());
    bus.set_radio(1, clean(FREQ));
    bus.set_radio(2, clean(FREQ));
    bus.set_radio(3, clean(121_500_000));
    assert!(bus.hears(&rx(FREQ, 1.0)) && !bus.hears(&rx(118_000_000, 1.0)));
    let mut txs = [Transmitter::new().unwrap(), Transmitter::new().unwrap()];
    txs.iter_mut().for_each(|t| t.set_ptt(true));
    let start = Instant::now();
    let mut events = Vec::new();
    let (mut l, mut r) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
    for i in 0..60 {
        let now = start + Duration::from_millis(20 * i as u64);
        let [a, b] = &mut txs;
        for (tx, callsign, freq) in [(a, "SAS123", FREQ), (b, "NAX45F", 121_500_000)] {
            if i == 30 && callsign == "NAX45F" {
                tx.set_ptt(false);
            }
            if let Some(p) = tx.process(&sine(i, 440.0, 0.3)).unwrap() {
                bus.push(callsign, p.sequence, p.opus, p.last_packet, &rx(freq, 1.0), now).unwrap();
            }
        }
        if i == 40 {
            // Transmitting on radio 1 stops reception there.
            bus.set_transmitting(1, true);
        }
        bus.render_stereo(&mut l, &mut r, |id, callsign, c| events.push((id, callsign.to_owned(), c)));
    }
    let started: Vec<_> = events.iter().filter(|e| e.2 == RxChange::Started).map(|e| (e.0, &*e.1)).collect();
    let ended: Vec<_> = events.iter().filter(|e| e.2 == RxChange::Ended).map(|e| (e.0, &*e.1)).collect();
    assert_eq!(started.len(), 3, "{events:?}");
    assert!(started.contains(&(1, "SAS123")) && started.contains(&(2, "SAS123")) && started.contains(&(3, "NAX45F")));
    assert_eq!(ended, [(3, "NAX45F"), (1, "SAS123")], "{events:?}");
}

#[test]
fn bus_mono_ignores_pan_and_plays_sounds() {
    let mut bus = Bus::new(EffectSamples::default());
    bus.play_sound(vec![0.5; 100].into(), 2.0);
    let mut out = [0.0; FRAME_SAMPLES];
    bus.render_mono(&mut out, |_, _, _| {});
    assert_eq!((out[0], out[99], out[100]), (1.0, 1.0, 0.0));
    bus.render_mono(&mut out, |_, _, _| {});
    assert_eq!(out[0], 0.0);
}
