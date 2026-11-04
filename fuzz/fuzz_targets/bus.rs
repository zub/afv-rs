//! The whole receive path: relayed packets (callsign, sequence, Opus bytes, transceivers with
//! server-chosen frequencies and distance ratios) into a bus with radios, rendered frame by frame.
#![no_main]
use std::time::{Duration, Instant};

use afv_audio::{Bus, EffectSamples, FRAME_SAMPLES, RadioSettings};
use afv_proto::voice::RxTransceiver;
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

const FREQS: [u32; 4] = [122_800_000, 8_891_000, 118_000_000, 0];

#[derive(Arbitrary, Debug)]
enum Op {
    Push { talker: u8, sequence: u32, opus: Vec<u8>, last: bool, rx: Vec<(u8, u8, f32)>, advance_ms: u16 },
    Render { stereo: bool },
    Radio { id: u8, freq: u8, effects: bool, gain: f32 },
    Remove { id: u8 },
    Transmitting { id: u8, on: bool },
    Purge,
}

fuzz_target!(|ops: Vec<Op>| {
    let mut bus = Bus::new(EffectSamples::default());
    let mut now = Instant::now();
    let (mut left, mut right) = ([0.0f32; FRAME_SAMPLES], [0.0f32; FRAME_SAMPLES]);
    bus.set_radio(0, RadioSettings { frequency: FREQS[0], ..Default::default() });
    bus.set_radio(1, RadioSettings { frequency: FREQS[1], ..Default::default() });
    let mut renders = 0;
    for op in ops {
        match op {
            Op::Push { talker, sequence, opus, last, rx, advance_ms } => {
                now += Duration::from_millis(advance_ms as u64 % 2000);
                let rx: Vec<_> = rx
                    .into_iter()
                    .take(8)
                    .map(|(id, f, d)| RxTransceiver { id: id as u16, frequency: FREQS[f as usize % 4], distance_ratio: d })
                    .collect();
                let callsign = format!("T{}", talker % 8);
                if bus.hears(&rx) {
                    let _ = bus.push(&callsign, sequence, &opus, last, &rx, now);
                }
            }
            Op::Render { stereo } => {
                renders += 1;
                if renders > 200 {
                    continue;
                }
                bus.render(&mut left, stereo.then_some(&mut right), |_, _, _| {});
                // Gain is caller-controlled and may be inf/NaN; only check with sane gains.
            }
            Op::Radio { id, freq, effects, gain } => {
                let mut s = RadioSettings { frequency: FREQS[freq as usize % 4], ..Default::default() };
                s.audio.effects = effects;
                s.audio.gain = if gain.is_finite() { gain.clamp(0.0, 4.0) } else { 1.0 };
                bus.set_radio((id % 4) as u32, s);
            }
            Op::Remove { id } => bus.remove_radio((id % 4) as u32, |_, _, _| {}),
            Op::Transmitting { id, on } => bus.set_transmitting((id % 4) as u32, on),
            Op::Purge => bus.purge_idle(now, Duration::from_secs(60)),
        }
        assert!(left.iter().chain(right.iter()).all(|s| s.is_finite()), "non-finite output");
    }
});
