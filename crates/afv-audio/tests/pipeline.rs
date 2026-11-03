use std::sync::Arc;
use std::time::{Duration, Instant};

use afv_audio::codec::{Decoder, Encoder};
use afv_audio::radio::{Output, Signal};
use afv_audio::{
    Bus, Carrier, EffectSamples, FRAME_SAMPLES, Frame, HfConditions, Pan, Radio, RadioSettings, RxChange, Transmitter,
    VhfHardware,
};
use afv_proto::voice::RxTransceiver;

const FREQ: u32 = 122_800_000;

fn sine(frame_index: usize, freq: f32, amp: f32) -> Frame {
    std::array::from_fn(|i| {
        let t = (frame_index * FRAME_SAMPLES + i) as f32 / 48_000.0;
        amp * (std::f32::consts::TAU * freq * t).sin()
    })
}

/// Amplitude of `freq` in `x` (one DFT bin).
fn tone(x: &[f32], freq: f64) -> f32 {
    let (mut re, mut im) = (0f64, 0f64);
    for (i, s) in x.iter().enumerate() {
        let a = std::f64::consts::TAU * freq * i as f64 / 48_000.0;
        re += *s as f64 * a.cos();
        im += *s as f64 * a.sin();
    }
    (2.0 * re.hypot(im) / x.len() as f64) as f32
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

fn process(radio: &mut Radio, streams: &[(&Frame, &[RxTransceiver])], fx: &EffectSamples) -> (Frame, Frame, u32) {
    let (mut l, mut r) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
    // Talker `i` transmits 150·i Hz off frequency.
    let signals = streams.iter().enumerate().map(|(i, &(audio, transceivers))| Signal {
        audio,
        transceivers,
        carrier: Carrier { offset_hz: 150 * i as i16, phase: 0 },
    });
    radio.process(signals, fx, Output::Stereo { left: &mut l, right: &mut r });
    (l, r, radio.receiving())
}

#[test]
fn radio_without_effects_passes_matching_voice_only() {
    let fx = EffectSamples::default();
    let voice = sine(0, 440.0, 0.4);
    let mut radio = Radio::new(RadioSettings { gain: 0.5, ..clean(FREQ) });

    let (l, r, receiving) = process(&mut radio, &[(&voice, &rx(FREQ, 1.0))], &fx);
    assert_eq!(receiving, 1);
    assert!(l.iter().zip(&voice).all(|(o, v)| *o == 0.5 * v));
    assert_eq!(l, r);

    let (l, _, receiving) = process(&mut radio, &[(&voice, &rx(FREQ + 25_000, 1.0))], &fx);
    assert_eq!(receiving, 0);
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
    let (l, _, receiving) = process(&mut radio, &streams, &fx);
    assert_eq!((rms(&l), receiving), (0.0, 1), "muted radios still detect reception");

    let mut radio = Radio::new(clean(FREQ));
    radio.set_transmitting(true);
    let (l, _, _) = process(&mut radio, &streams, &fx);
    assert_eq!(rms(&l), 0.0);
    assert!(!radio.hears(&rx(FREQ, 1.0)), "nothing is heard while transmitting");
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
    let (l, _, receiving) = process(&mut radio, &[], &fx);
    assert_eq!(receiving, 0);
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
fn transmitting_mid_reception_neither_clicks_nor_loses_the_talker() {
    let ones: Arc<[f32]> = vec![1.0; 4800].into();
    let fx = EffectSamples { click: Some(ones), ..EffectSamples::default() };
    let silence = [0.0; FRAME_SAMPLES];
    let streams = [(&silence, &rx(FREQ, 1.0)[..])];
    let peak = |l: &Frame| l.iter().fold(0f32, |m, s| m.max(s.abs()));
    let mut radio = Radio::new(RadioSettings { frequency: FREQ, ..RadioSettings::default() });
    for _ in 0..5 {
        process(&mut radio, &streams, &fx);
    }
    radio.set_transmitting(true);
    for _ in 0..5 {
        assert_eq!(peak(&process(&mut radio, &streams, &fx).0), 0.0, "silent while transmitting");
    }
    radio.set_transmitting(false);
    for _ in 0..5 {
        let (l, _, receiving) = process(&mut radio, &streams, &fx);
        assert!(peak(&l) < 0.01, "no click after releasing PTT: {}", peak(&l));
        assert_eq!(receiving, 1);
    }
    let (l, _, receiving) = process(&mut radio, &[], &fx);
    assert_eq!(receiving, 0);
    assert!(peak(&l) > 1.0, "the click once the talker stops: {}", peak(&l));
}

#[test]
fn bus_ends_and_resumes_reception_around_our_own_transmission() {
    let mut bus = Bus::new(EffectSamples::default());
    bus.set_radio(1, clean(FREQ));
    bus.set_radio(2, RadioSettings { muted: true, ..clean(FREQ) });
    let mut tx = Transmitter::new().unwrap();
    tx.set_ptt(true);
    let start = Instant::now();
    let mut events = Vec::new();
    let (mut l, mut r) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
    for i in 0..70 {
        if i == 50 {
            tx.set_ptt(false);
        }
        if let Some(p) = tx.process(&sine(i, 440.0, 0.3)).unwrap() {
            bus.push(
                "SAS123",
                p.sequence,
                p.opus,
                p.last_packet,
                &rx(FREQ, 1.0),
                start + Duration::from_millis(20 * i as u64),
            )
            .unwrap();
        }
        let transmitting = (20..30).contains(&i);
        bus.set_transmitting(1, transmitting);
        bus.render_stereo(&mut l, &mut r, |id, _, c| events.push((i, id, c)));
        if transmitting {
            assert_eq!(rms(&l), 0.0, "radio 1 transmits and radio 2 is muted (frame {i})");
        }
    }
    let on = |radio| events.iter().filter(|e| e.1 == radio).map(|e| (e.0, e.2)).collect::<Vec<_>>();
    let radio_1 = on(1);
    assert_eq!(radio_1[..3], [(0, RxChange::Started), (20, RxChange::Ended), (30, RxChange::Started)], "{events:?}");
    assert!(matches!(radio_1[3..], [(i, RxChange::Ended)] if i > 50), "{events:?}");
    let radio_2 = on(2);
    assert!(
        matches!(radio_2[..], [(0, RxChange::Started), (i, RxChange::Ended)] if i > 50),
        "muted radios report: {events:?}"
    );
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

const HF: u32 = 8_891_000;

/// A 1 kHz transmission from `callsign` on `frequency`, through a bus with one radio. Returns the
/// output after the first 400 ms.
fn render_one(callsign: &str, frequency: u32, radio: RadioSettings, conditions: HfConditions) -> Vec<f32> {
    let mut tx = Transmitter::new().unwrap();
    tx.set_ptt(true);
    let mut bus = Bus::new(EffectSamples::default());
    bus.set_hf_conditions(conditions);
    bus.set_radio(1, radio);
    let start = Instant::now();
    let mut out = Vec::new();
    let mut frame = [0.0; FRAME_SAMPLES];
    for i in 0..60 {
        let p = tx.process(&sine(i, 1000.0, 0.3)).unwrap().unwrap();
        bus.push(
            callsign,
            p.sequence,
            p.opus,
            false,
            &rx(frequency, 1.0),
            start + Duration::from_millis(20 * i as u64),
        )
        .unwrap();
        bus.render_mono(&mut frame, |_, _, _| {});
        out.extend_from_slice(&frame);
    }
    out.split_off(20 * FRAME_SAMPLES)
}

#[test]
fn hf_propagation_shifts_voice_on_hf_radios_with_effects() {
    // A tuning error anywhere within ±100 Hz; with this seed the station's is clearly audible.
    let conditions = HfConditions { offset_hz: 100.0, ..HfConditions::OFF };
    let radio = RadioSettings { frequency: HF, ..RadioSettings::default() };
    // The strongest tone between 850 and 1150 Hz, in 1 Hz steps.
    let strongest =
        |out: &[f32]| (850..=1150).max_by(|&a, &b| tone(out, a as f64).total_cmp(&tone(out, b as f64))).unwrap();

    afv_audio::dsp::seed_rng(1);
    let shifted = strongest(&render_one("SAS123", HF, radio.clone(), conditions));
    assert!((shifted - 1000i32).abs() > 20, "the 1 kHz tone comes out at {shifted} Hz");

    afv_audio::dsp::seed_rng(1);
    let clean = strongest(&render_one("SAS123", HF, RadioSettings { effects: false, ..radio }, conditions));
    assert_eq!(clean, 1000, "effects off");
}

#[test]
fn vhf_is_untouched_by_hf_conditions() {
    let radio = RadioSettings { frequency: FREQ, ..RadioSettings::default() };
    let poor = render_one("SAS123", FREQ, radio.clone(), HfConditions::POOR);
    let off = render_one("SAS123", FREQ, radio, HfConditions::OFF);
    assert_eq!(poor, off);
}

#[test]
fn hf_radios_hear_their_background_all_the_time() {
    let fx = EffectSamples::default();
    let silence = [0.0; FRAME_SAMPLES];
    let peak_over = |settings: RadioSettings, receiving: bool, transmitting: bool| {
        let frequency = settings.frequency;
        let mut radio = Radio::new(settings);
        radio.set_hf_conditions(HfConditions::POOR);
        radio.set_transmitting(transmitting);
        let transceivers = rx(frequency, 1.0);
        let streams: &[(&Frame, &[RxTransceiver])] = if receiving { &[(&silence, &transceivers)] } else { &[] };
        (0..100).map(|_| process(&mut radio, streams, &fx).0).fold(0f32, |m, l| l.iter().fold(m, |m, s| m.max(s.abs())))
    };
    let hf = RadioSettings { frequency: HF, ..RadioSettings::default() };
    assert!(peak_over(hf.clone(), true, false) > 0.05, "while receiving");
    assert!(peak_over(hf.clone(), false, false) > 0.05, "and with nobody transmitting: HF has no squelch");
    assert_eq!(peak_over(hf.clone(), false, true), 0.0, "silent while transmitting");
    assert_eq!(peak_over(RadioSettings { muted: true, ..hf.clone() }, false, false), 0.0, "muted");
    for receiving in [true, false] {
        assert_eq!(peak_over(RadioSettings { hf_squelch: true, ..hf.clone() }, receiving, false), 0.0, "HF squelch");
        assert_eq!(peak_over(RadioSettings { effects: false, ..hf.clone() }, receiving, false), 0.0, "effects off");
    }
    let vhf = RadioSettings { frequency: FREQ, ..RadioSettings::default() };
    assert_eq!(peak_over(vhf, false, false), 0.0, "VHF stays squelched");
}

#[test]
fn hf_radios_do_not_click_at_the_end_of_a_transmission() {
    let ones: Arc<[f32]> = vec![1.0; 4800].into();
    let fx = EffectSamples { click: Some(ones), ..EffectSamples::default() };
    let silence = [0.0; FRAME_SAMPLES];
    let end = |squelch: bool| {
        let mut radio = Radio::new(RadioSettings { frequency: HF, hf_squelch: squelch, ..RadioSettings::default() });
        radio.set_hf_conditions(HfConditions::OFF);
        process(&mut radio, &[(&silence, &rx(HF, 1.0))], &fx);
        let (l, _, receiving) = process(&mut radio, &[], &fx);
        assert_eq!(receiving, 0);
        l.iter().fold(0f32, |m, s| m.max(s.abs()))
    };
    assert!(end(false) < 0.01, "no squelch, no click: {}", end(false));
    // Nor when the transmission ends while we are transmitting ourselves and only heard later.
    let mut radio = Radio::new(RadioSettings { frequency: HF, ..RadioSettings::default() });
    radio.set_hf_conditions(HfConditions::OFF);
    process(&mut radio, &[(&silence, &rx(HF, 1.0))], &fx);
    radio.set_transmitting(true);
    process(&mut radio, &[], &fx);
    radio.set_transmitting(false);
    let (l, _, _) = process(&mut radio, &[], &fx);
    assert!(l.iter().all(|s| s.abs() < 0.01), "no click after releasing PTT");
    assert!(end(true) > 1.0, "with HF squelch the click is back: {}", end(true));
}

/// `frames` frames of `radio` receiving `voice(frame_index)`, from frame `first` on.
fn frames(radio: &mut Radio, first: usize, frames: usize, voice: impl Fn(usize) -> Frame) -> Vec<f32> {
    let (fx, transceivers) = (EffectSamples::default(), rx(radio.settings().frequency, 1.0));
    (first..first + frames).flat_map(|i| process(radio, &[(&voice(i), &transceivers)], &fx).0).collect()
}

fn radio(settings: RadioSettings) -> Radio {
    let mut radio = Radio::new(settings);
    radio.set_hf_conditions(HfConditions::OFF);
    radio
}

fn hf_radio() -> Radio {
    radio(RadioSettings { frequency: HF, ..RadioSettings::default() })
}

/// A 1 kHz tone plus a weaker one at `freq`; returns the level of `freq` relative to 1 kHz, in
/// dB. With both at once the gain control is set by the 1 kHz tone, so the ratio is the filter's.
fn relative_db(radio: &mut Radio, freq: f32) -> f32 {
    let two_tone = |i| -> Frame {
        let (a, b) = (sine(i, 1000.0, 0.5), sine(i, freq, 0.05));
        std::array::from_fn(|n| a[n] + b[n])
    };
    let out = frames(radio, 0, 50, two_tone);
    20.0 * (10.0 * tone(&out[24_000..], freq as f64) / tone(&out[24_000..], 1000.0)).log10()
}

#[test]
fn hf_radios_hear_through_the_hfs900d() {
    let vhf = |f| relative_db(&mut radio(RadioSettings { frequency: FREQ, ..RadioSettings::default() }), f);
    let hf = |f| relative_db(&mut hf_radio(), f);
    // Collins' published mask: 35 dB down at 2900 Hz, 60 dB at 3300 Hz.
    assert!(vhf(2900.0) > -20.0, "VHF passes 2.9 kHz: {} dB", vhf(2900.0));
    assert!(hf(2900.0) < -35.0, "HF at 2.9 kHz: {} dB", hf(2900.0));
    assert!(hf(3300.0) < -55.0, "HF at 3.3 kHz: {} dB", hf(3300.0));
    assert!(hf(150.0) < -35.0, "HF at 150 Hz: {} dB", hf(150.0));
    assert!(hf(2400.0).abs() < 4.0 && hf(400.0).abs() < 4.0, "HF passband: {} and {} dB", hf(2400.0), hf(400.0));
}

#[test]
fn the_vhf_set_never_colours_hf() {
    let render = |vhf_hardware| {
        let mut r = radio(RadioSettings { frequency: HF, vhf_hardware, ..RadioSettings::default() });
        frames(&mut r, 0, 30, |i| sine(i, 700.0, 0.4))
    };
    let schmid = render(VhfHardware::SchmidEd137b);
    assert_eq!(render(VhfHardware::RockwellCollins2100), schmid);
    assert_eq!(render(VhfHardware::Garex220), schmid);
}

#[test]
fn the_hf_agc_holds_the_level_and_the_volume_comes_after_it() {
    let level = |amplitude: f32, gain: f32| {
        let mut r = radio(RadioSettings { frequency: HF, gain, ..RadioSettings::default() });
        let out = frames(&mut r, 0, 50, |i| sine(i, 1000.0, amplitude));
        tone(&out[24_000..], 1000.0)
    };
    let (loud, quiet) = (level(1.0, 1.0), level(0.5, 1.0));
    assert!((loud / quiet - 1.0).abs() < 0.05, "6 dB less signal, same output: {loud} vs {quiet}");
    let turned_down = level(1.0, 0.5);
    assert!((turned_down / loud - 0.5).abs() < 0.005, "volume halves the output: {turned_down} vs {loud}");
}

#[test]
fn an_hf_set_comes_back_from_transmitting_like_new() {
    let voice = |i| sine(i, 700.0, 0.6);
    let mut r = hf_radio();
    frames(&mut r, 0, 50, voice);
    r.set_transmitting(true);
    frames(&mut r, 50, 10, voice);
    r.set_transmitting(false);
    let after = frames(&mut r, 60, 50, voice);
    assert_eq!(after, frames(&mut hf_radio(), 60, 50, voice));
}

#[test]
fn retuning_across_bands_switches_the_set() {
    let voice = |i| sine(i, 700.0, 0.6);
    let mut r = radio(RadioSettings { frequency: FREQ, ..RadioSettings::default() });
    frames(&mut r, 0, 50, voice);
    r.set_settings(RadioSettings { frequency: HF, ..r.settings().clone() });
    assert_eq!(frames(&mut r, 50, 50, voice), frames(&mut hf_radio(), 50, 50, voice));
    assert!(relative_db(&mut r, 2900.0) < -35.0, "the HF set");
    r.set_settings(RadioSettings { frequency: FREQ, ..r.settings().clone() });
    assert!(relative_db(&mut r, 2900.0) > -20.0, "back to the VHF set");
}

#[test]
fn hf_static_goes_through_the_sideband_filter() {
    // Static only: silent voice on an HF radio under frequent crackle, quiet enough not to drive
    // the audio stage into saturation (whose harmonics would reach beyond the sideband).
    let mut radio = Radio::new(RadioSettings { frequency: HF, ..RadioSettings::default() });
    radio.set_hf_conditions(HfConditions { crackle_rate_hz: 20.0, crackle_level: 0.05, ..HfConditions::OFF });
    let (fx, silence, transceivers) = (EffectSamples::default(), [0.0; FRAME_SAMPLES], rx(HF, 1.0));
    let out: Vec<f32> = (0..250).flat_map(|_| process(&mut radio, &[(&silence, &transceivers)], &fx).0).collect();
    let band = |lo: f64, hi: f64| (0..20).map(|k| tone(&out, lo + (hi - lo) * k as f64 / 20.0).powi(2)).sum::<f32>();
    let (voice, high) = (band(800.0, 1200.0), band(7000.0, 9000.0));
    assert!(voice > 1e4 * high, "1 kHz band {voice}, 8 kHz band {high}");
}

#[test]
fn hf_radios_play_each_kind_of_background() {
    let only_noise = HfConditions { noise_level: 0.5, ..HfConditions::OFF };
    let only_sounders = HfConditions { sounder_rate_hz: 5.0, sounder_level: 1.0, ..HfConditions::OFF };
    let output = |conditions: HfConditions, squelch: bool| {
        let mut radio = Radio::new(RadioSettings { frequency: HF, hf_squelch: squelch, ..RadioSettings::default() });
        radio.set_hf_conditions(conditions);
        let (fx, silence, transceivers) = (EffectSamples::default(), [0.0; FRAME_SAMPLES], rx(HF, 1.0));
        (0..250).flat_map(|_| process(&mut radio, &[(&silence, &transceivers)], &fx).0).collect::<Vec<f32>>()
    };
    let noise = output(only_noise, false);
    let audible = noise[4800..].iter().filter(|s| s.abs() > 1e-4).count() as f32 / (noise.len() - 4800) as f32;
    assert!(audible > 0.99, "the noise floor is continuous: {:.0} %", audible * 100.0);
    assert!(output(only_sounders, false).iter().any(|s| s.abs() > 0.01), "sounders are heard");
    for c in [only_noise, only_sounders] {
        assert!(output(c, true).iter().all(|&s| s == 0.0), "HF squelch removes {c:?}");
    }
}
