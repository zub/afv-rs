//! WAV files for the examples. The library takes samples, not files, so it has no WAV dependency.

use std::error::Error;
use std::path::Path;

use afv::{EffectSamples, Samples};
use afv_audio::SAMPLE_RATE;

/// Reads a WAV file as interleaved `f32`, whatever its sample format.
pub fn read(path: &Path) -> Result<(hound::WavSpec, Vec<f32>), hound::Error> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.into_samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    Ok((spec, samples))
}

/// Loads afv-native's five effect files from `dir` as mono 48 kHz. Missing files are skipped;
/// files that exist but cannot be read are an error.
pub fn load_effects(dir: &Path) -> Result<EffectSamples, Box<dyn Error>> {
    let load = |name: &str| -> Result<Option<Samples>, Box<dyn Error>> {
        let path = dir.join(name);
        if !path.exists() {
            return Ok(None);
        }
        let (spec, samples) = read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if spec.sample_rate != SAMPLE_RATE {
            return Err(format!("{}: {} Hz, expected {SAMPLE_RATE} Hz", path.display(), spec.sample_rate).into());
        }
        let channels = spec.channels.max(1) as usize;
        Ok(Some(samples.chunks(channels).map(|c| c.iter().sum::<f32>() / channels as f32).collect()))
    };
    Ok(EffectSamples {
        click: load("Click_f32.wav")?,
        crackle: load("Crackle_f32.wav")?,
        ac_bus: load("AC_Bus_f32.wav")?,
        vhf_noise: load("WhiteNoise_f32.wav")?,
        hf_noise: load("HF_WhiteNoise_f32.wav")?,
    })
}
