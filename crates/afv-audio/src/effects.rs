//! Recorded effect sounds (squelch click, static, noise) and their playback.

use std::sync::Arc;

use crate::Frame;

/// A mono 48 kHz sample buffer shared between players.
pub type Samples = Arc<[f32]>;

/// The recorded sounds used by the radio simulation. Any missing sound is simply not played.
///
/// afv-native ships these as `Click_f32.wav`, `Crackle_f32.wav`, `AC_Bus_f32.wav`,
/// `WhiteNoise_f32.wav` and `HF_WhiteNoise_f32.wav`.
#[derive(Debug, Clone, Default)]
pub struct EffectSamples {
    /// Squelch tail played when a reception ends.
    pub click: Option<Samples>,
    /// Static, louder as the signal weakens (VHF).
    pub crackle: Option<Samples>,
    /// Electrical hum of the aircraft bus.
    pub ac_bus: Option<Samples>,
    /// Background hiss on VHF.
    pub vhf_noise: Option<Samples>,
    /// Background hiss on HF.
    pub hf_noise: Option<Samples>,
}

impl EffectSamples {
    /// Loads the five afv-native effect files from a directory. Files that are missing are
    /// skipped; files that exist but cannot be read are an error.
    #[cfg(feature = "wav")]
    pub fn load_dir(dir: impl AsRef<std::path::Path>) -> Result<Self, crate::wav::WavError> {
        let dir = dir.as_ref();
        let load = |name: &str| -> Result<Option<Samples>, crate::wav::WavError> {
            let path = dir.join(name);
            if !path.exists() {
                return Ok(None);
            }
            crate::wav::load(&path).map(Some)
        };
        Ok(Self {
            click: load("Click_f32.wav")?,
            crackle: load("Crackle_f32.wav")?,
            ac_bus: load("AC_Bus_f32.wav")?,
            vhf_noise: load("WhiteNoise_f32.wav")?,
            hf_noise: load("HF_WhiteNoise_f32.wav")?,
        })
    }
}

/// Plays a [`Samples`] buffer frame by frame, once or looping.
#[derive(Debug, Clone)]
pub struct SamplePlayer {
    samples: Samples,
    looping: bool,
    position: usize,
    playing: bool,
}

impl SamplePlayer {
    pub fn new(samples: Samples, looping: bool) -> Self {
        let playing = !samples.is_empty();
        Self { samples, looping, position: 0, playing }
    }

    /// Fills `frame` with the next samples, padding with silence after a one-shot ends.
    /// Returns `false` (leaving `frame` untouched) once a one-shot has finished.
    pub fn fill(&mut self, frame: &mut Frame) -> bool {
        if !self.playing {
            return false;
        }
        let len = self.samples.len();
        let mut filled = 0;
        loop {
            if self.looping && self.position >= len {
                self.position = 0;
            }
            let n = (frame.len() - filled).min(len - self.position);
            frame[filled..filled + n].copy_from_slice(&self.samples[self.position..self.position + n]);
            self.position += n;
            filled += n;
            if !self.looping || filled == frame.len() {
                break;
            }
        }
        frame[filled..].fill(0.0);
        if !self.looping && self.position >= len {
            self.playing = false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    #[test]
    fn one_shot_pads_then_stops() {
        let mut p = SamplePlayer::new((0..1000).map(|i| i as f32).collect(), false);
        let mut f = [9.0; FRAME_SAMPLES];
        assert!(p.fill(&mut f));
        assert_eq!(f[959], 959.0);
        assert!(p.fill(&mut f));
        assert_eq!(f[39], 999.0);
        assert_eq!(f[40], 0.0);
        assert!(!p.fill(&mut f));
    }

    #[test]
    fn looping_wraps_within_a_frame() {
        let mut p = SamplePlayer::new((0..100).map(|i| i as f32).collect(), true);
        let mut f = [0.0; FRAME_SAMPLES];
        for _ in 0..3 {
            assert!(p.fill(&mut f));
        }
        // Frame 3 starts at overall sample 1920, i.e. 20 into the loop.
        assert_eq!(f[0], 20.0);
        assert_eq!(f[80], 0.0);
    }
}
