//! Renders two recordings through the full transmit and receive path as single and double
//! transmissions, for listening:
//!
//! ```text
//! cargo run -p afv-audio --example render_double_tx -- a.wav b.wav OUT_DIR
//! ```
//!
//! Inputs must be 48 kHz WAV files (macOS: `say -o a.wav --file-format=WAVE --data-format=LEF32@48000 "..."`).
//! Station A starts at 0.2 s, station B cuts in at 1.2 s. Writes 16-bit mono WAVs to `OUT_DIR`,
//! ending with station A alone on HF under each of the [`HfConditions`] presets and on VHF through
//! each [`VhfHardware`] set.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use afv_audio::{Bus, EffectSamples, FRAME_SAMPLES, HfConditions, RadioSettings, Transmitter, VhfHardware};
use afv_proto::voice::RxTransceiver;

const VHF: u32 = 118_500_000;
/// A North Atlantic HF frequency.
const HF: u32 = 8_891_000;
const HF_PRESETS: [(&str, HfConditions); 5] = [
    ("off", HfConditions::OFF),
    ("good", HfConditions::GOOD),
    ("fair", HfConditions::FAIR),
    ("poor", HfConditions::POOR),
    ("flutter", HfConditions::FLUTTER),
];
/// Background kept after the last transmission, in frames: HF has no squelch, so 4 s of the band
/// between transmissions; VHF only the squelch tail.
const HF_TAIL_FRAMES: usize = 200;
const VHF_TAIL_FRAMES: usize = 25;

struct Talker<'a> {
    callsign: &'static str,
    audio: &'a [f32],
    start_frame: usize,
    distance_ratio: f32,
}

fn render(
    out: &Path,
    name: &str,
    frequency: u32,
    vhf_hardware: VhfHardware,
    hf: HfConditions,
    talkers: &[Talker],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bus = Bus::new(EffectSamples::default());
    bus.set_hf_conditions(hf);
    bus.set_radio(1, RadioSettings { frequency, vhf_hardware, ..RadioSettings::default() });
    let mut txs = talkers.iter().map(|_| Transmitter::new()).collect::<Result<Vec<_>, _>>()?;
    let tail = if frequency < 30_000_000 { HF_TAIL_FRAMES } else { VHF_TAIL_FRAMES };
    let frames = talkers.iter().map(|t| t.start_frame + t.audio.len() / FRAME_SAMPLES + 2).max().unwrap_or(0) + tail;
    let start = Instant::now();
    let (mut l, mut r) = ([0.0; FRAME_SAMPLES], [0.0; FRAME_SAMPLES]);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(out.join(format!("{name}.wav")), spec)?;
    for n in 0..frames {
        let now = start + Duration::from_millis(20 * n as u64);
        for (t, tx) in talkers.iter().zip(&mut txs) {
            let i = n.saturating_sub(t.start_frame) * FRAME_SAMPLES;
            let talking = n >= t.start_frame && i < t.audio.len();
            let mut mic = [0.0; FRAME_SAMPLES];
            if talking {
                let chunk = &t.audio[i..t.audio.len().min(i + FRAME_SAMPLES)];
                mic[..chunk.len()].copy_from_slice(chunk);
            }
            tx.set_ptt(talking);
            if let Some(p) = tx.process(&mic)? {
                let rx = [RxTransceiver { id: 0, frequency, distance_ratio: t.distance_ratio }];
                bus.push(t.callsign, p.sequence, p.opus, p.last_packet, &rx, now)?;
            }
        }
        bus.render_stereo(&mut l, &mut r, |_, _, _| {});
        for s in l {
            wav.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
    }
    wav.finalize()?;
    println!("wrote {}", out.join(format!("{name}.wav")).display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [a, b, out] = args.as_slice() else {
        return Err("usage: render_double_tx A.wav B.wav OUT_DIR".into());
    };
    let (a, b) = (afv_audio::wav::load(a)?, afv_audio::wav::load(b)?);
    std::fs::create_dir_all(out)?;
    let t = |callsign, audio, start_frame, distance_ratio| Talker { callsign, audio, start_frame, distance_ratio };
    let fair = HfConditions::FAIR;
    render(out, "1_single", VHF, VhfHardware::default(), fair, &[t("SAS123", &a, 10, 1.0)])?;
    render(
        out,
        "2_vhf_equal_strength",
        VHF,
        VhfHardware::default(),
        fair,
        &[t("SAS123", &a, 10, 1.0), t("NAX45F", &b, 60, 1.0)],
    )?;
    render(
        out,
        "3_vhf_strong_vs_weak",
        VHF,
        VhfHardware::default(),
        fair,
        &[t("SAS123", &a, 10, 1.0), t("NAX45F", &b, 60, 0.3)],
    )?;
    render(
        out,
        "4_vhf_weak_vs_strong",
        VHF,
        VhfHardware::default(),
        fair,
        &[t("SAS123", &a, 10, 0.3), t("NAX45F", &b, 60, 1.0)],
    )?;
    render(
        out,
        "5_hf_equal_strength",
        HF,
        VhfHardware::default(),
        fair,
        &[t("SAS123", &a, 10, 1.0), t("NAX45F", &b, 60, 1.0)],
    )?;
    for (name, hf) in HF_PRESETS {
        render(out, &format!("6_hf_{name}"), HF, VhfHardware::default(), hf, &[t("SAS123", &a, 10, 1.0)])?;
    }
    for (name, hardware) in [
        ("schmid", VhfHardware::SchmidEd137b),
        ("rockwell", VhfHardware::RockwellCollins2100),
        ("garex", VhfHardware::Garex220),
    ] {
        render(out, &format!("7_vhf_{name}"), VHF, hardware, fair, &[t("SAS123", &a, 10, 1.0)])?;
    }
    Ok(())
}
