//! Minimal WAV loading for effect samples.

use std::path::{Path, PathBuf};

use crate::SAMPLE_RATE;
use crate::effects::Samples;

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("{path}: {source}")]
    Read { path: PathBuf, source: hound::Error },
    #[error("{path}: sample rate is {rate} Hz, expected {SAMPLE_RATE} Hz")]
    SampleRate { path: PathBuf, rate: u32 },
}

/// Loads a WAV file as mono 48 kHz `f32`, averaging channels.
pub fn load(path: &Path) -> Result<Samples, WavError> {
    let err = |source| WavError::Read { path: path.to_owned(), source };
    let reader = hound::WavReader::open(path).map_err(err)?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE {
        return Err(WavError::SampleRate { path: path.to_owned(), rate: spec.sample_rate });
    }
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<_, _>>().map_err(err)?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.into_samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>().map_err(err)?
        }
    };
    let channels = spec.channels.max(1) as usize;
    Ok(interleaved.chunks(channels).map(|c| c.iter().sum::<f32>() / channels as f32).collect())
}
