//! Conversion between device audio (any sample rate, channel count and buffer size) and the
//! 48 kHz, 20 ms frames the rest of the crate works on. Resampling uses `rubato`'s FFT resampler.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("unsupported audio format: {rate} Hz, {channels} channels")]
    Format { rate: u32, channels: u16 },
    #[error("cannot resample: {0}")]
    Resampler(#[from] rubato::ResamplerConstructionError),
}

/// Highest device sample rate accepted, above any audio interface's. Buffers and resamplers are
/// sized from the rate, so a nonsense value from a broken driver must not reach them.
pub const MAX_SAMPLE_RATE: u32 = 768_000;
/// Most device channels accepted, for the same reason.
pub const MAX_CHANNELS: u16 = 256;

/// Checks that a device format is one the converters, and buffers sized from it, can handle.
pub fn check_format(rate: u32, channels: u16) -> Result<(), ConvertError> {
    if (1..=MAX_SAMPLE_RATE).contains(&rate) && (1..=MAX_CHANNELS).contains(&channels) {
        Ok(())
    } else {
        Err(ConvertError::Format { rate, channels })
    }
}

/// Resamples `input` (interleaved, `channels` wide) and appends the result to `out`.
/// Consumes whole resampler chunks from the front of `input` and returns how many samples it used.
fn resample(r: &mut Fft<f32>, channels: usize, input: &[f32], out: &mut Vec<f32>, scratch: &mut Vec<f32>) -> usize {
    let mut used = 0;
    loop {
        let need = r.input_frames_next();
        if (input.len() - used) / channels < need {
            return used;
        }
        let chunk = &input[used..used + need * channels];
        let produced = r.output_frames_next();
        scratch.resize(produced * channels, 0.0);
        let input = InterleavedSlice::new(chunk, channels, need).expect("sized to the chunk");
        let mut output = InterleavedSlice::new_mut(&mut scratch[..], channels, produced).expect("sized to the chunk");
        let (frames_in, frames_out) =
            r.process_into_buffer(&input, &mut output, None).expect("buffers are sized by the resampler");
        out.extend_from_slice(&scratch[..frames_out * channels]);
        used += frames_in * channels;
    }
}

/// Turns captured device audio into 48 kHz mono frames, averaging the channels.
pub struct CaptureConverter {
    channels: usize,
    resampler: Option<Fft<f32>>,
    /// Mono input at the device rate, not yet resampled.
    pending: Vec<f32>,
    /// 48 kHz mono, not yet emitted as a frame.
    ready: Vec<f32>,
    scratch: Vec<f32>,
}

impl CaptureConverter {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, ConvertError> {
        check_format(sample_rate, channels)?;
        let resampler = (sample_rate != SAMPLE_RATE)
            .then(|| Fft::new(sample_rate as usize, SAMPLE_RATE as usize, FRAME_SAMPLES, 1, FixedSync::Output))
            .transpose()?;
        Ok(Self { channels: channels as usize, resampler, pending: Vec::new(), ready: Vec::new(), scratch: Vec::new() })
    }

    /// Adds interleaved samples (a trailing partial frame is ignored) and calls `on_frame` for
    /// each complete 20 ms frame.
    pub fn push(&mut self, interleaved: &[f32], mut on_frame: impl FnMut(&Frame)) {
        let ch = self.channels;
        self.pending.extend(interleaved.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        match &mut self.resampler {
            None => self.ready.append(&mut self.pending),
            Some(r) => {
                let used = resample(r, 1, &self.pending, &mut self.ready, &mut self.scratch);
                self.pending.drain(..used);
            }
        }
        let frames = self.ready.as_chunks::<FRAME_SAMPLES>().0;
        frames.iter().for_each(&mut on_frame);
        let emitted = frames.len() * FRAME_SAMPLES;
        self.ready.drain(..emitted);
    }
}

/// Fills device buffers from 48 kHz frames rendered on demand. Devices with one channel get a
/// mono rendering; with two or more, the first two channels get stereo and the rest silence.
pub struct RenderConverter {
    channels: usize,
    /// Channels rendered: 1 or 2.
    rendered: usize,
    resampler: Option<Fft<f32>>,
    /// Interleaved (`rendered` wide) audio at the device rate, not yet written out.
    ready: Vec<f32>,
    position: usize,
    frame: Vec<f32>,
    scratch: Vec<f32>,
    left: Box<Frame>,
    right: Box<Frame>,
}

impl RenderConverter {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, ConvertError> {
        check_format(sample_rate, channels)?;
        let rendered = (channels as usize).min(2);
        let resampler = (sample_rate != SAMPLE_RATE)
            .then(|| Fft::new(SAMPLE_RATE as usize, sample_rate as usize, FRAME_SAMPLES, rendered, FixedSync::Input))
            .transpose()?;
        Ok(Self {
            channels: channels as usize,
            rendered,
            resampler,
            ready: Vec::new(),
            position: 0,
            frame: Vec::with_capacity(FRAME_SAMPLES * rendered),
            scratch: Vec::new(),
            left: Box::new([0.0; FRAME_SAMPLES]),
            right: Box::new([0.0; FRAME_SAMPLES]),
        })
    }

    /// Fills `out` (interleaved; a trailing partial frame is zeroed). `render` produces the next
    /// 48 kHz frame: into its first argument for mono, or both for stereo.
    pub fn fill(&mut self, out: &mut [f32], mut render: impl FnMut(&mut Frame, Option<&mut Frame>)) {
        let (ch, rendered) = (self.channels, self.rendered);
        let frames = out.len() / ch;
        while (self.ready.len() - self.position) / rendered < frames {
            render(&mut self.left, (rendered == 2).then_some(&mut *self.right));
            self.frame.clear();
            if rendered == 2 {
                self.frame.extend(self.left.iter().zip(self.right.iter()).flat_map(|(l, r)| [*l, *r]));
            } else {
                self.frame.extend_from_slice(&self.left[..]);
            }
            match &mut self.resampler {
                None => self.ready.extend_from_slice(&self.frame),
                Some(r) => {
                    resample(r, rendered, &self.frame, &mut self.ready, &mut self.scratch);
                }
            }
        }
        let source = &self.ready[self.position..self.position + frames * rendered];
        for (o, s) in out.chunks_exact_mut(ch).zip(source.chunks_exact(rendered)) {
            o[..rendered].copy_from_slice(s);
            o[rendered..].fill(0.0);
        }
        out[frames * ch..].fill(0.0);
        self.position += frames * rendered;
        if self.position >= self.ready.len() / 2 {
            self.ready.drain(..self.position);
            self.position = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amplitude of `freq` in `x` sampled at `rate`.
    fn tone(x: &[f32], freq: f64, rate: u32) -> f32 {
        let (mut re, mut im) = (0f64, 0f64);
        for (i, s) in x.iter().enumerate() {
            let a = std::f64::consts::TAU * freq * i as f64 / rate as f64;
            re += *s as f64 * a.cos();
            im += *s as f64 * a.sin();
        }
        (2.0 * re.hypot(im) / x.len() as f64) as f32
    }

    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (0.5 * (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin()) as f32).collect()
    }

    #[test]
    fn rejects_empty_and_absurd_formats() {
        assert!(CaptureConverter::new(0, 1).is_err());
        assert!(CaptureConverter::new(48_000, 0).is_err());
        assert!(RenderConverter::new(44_100, 0).is_err());
        assert!(RenderConverter::new(u32::MAX, 2).is_err());
        assert!(CaptureConverter::new(48_000, u16::MAX).is_err());
        assert!(check_format(MAX_SAMPLE_RATE, MAX_CHANNELS).is_ok());
    }

    #[test]
    fn capture_at_48k_reframes_without_changing_samples() {
        let input: Vec<f32> = (0..5000).map(|i| i as f32).collect();
        let mut c = CaptureConverter::new(48_000, 1).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(333) {
            c.push(chunk, |f| out.extend_from_slice(f));
        }
        assert_eq!(out.len(), 5 * FRAME_SAMPLES);
        assert_eq!(out, input[..out.len()]);
    }

    #[test]
    fn capture_averages_channels() {
        let stereo: Vec<f32> = (0..FRAME_SAMPLES).flat_map(|_| [1.0, 0.0]).collect();
        let mut c = CaptureConverter::new(48_000, 2).unwrap();
        let mut frames = 0;
        c.push(&stereo, |f| {
            frames += 1;
            assert!(f.iter().all(|&s| s == 0.5));
        });
        assert_eq!(frames, 1);
    }

    #[test]
    fn capture_resamples_to_48k() {
        // One second of a 1 kHz tone at 44.1 kHz in 441-frame stereo buffers.
        let mono = sine(1000.0, 44_100, 44_100);
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mut c = CaptureConverter::new(44_100, 2).unwrap();
        let mut out = Vec::new();
        for chunk in stereo.chunks(441 * 2) {
            c.push(chunk, |f| out.extend_from_slice(f));
        }
        // All but the resampler's buffering arrives.
        let frames = out.len() / FRAME_SAMPLES;
        assert!((47..=50).contains(&frames), "{frames} frames");
        let settled = &out[out.len() - 24_000..];
        assert!((tone(settled, 1000.0, 48_000) - 0.5).abs() < 0.01, "{}", tone(settled, 1000.0, 48_000));
        assert!(tone(settled, 1102.5, 48_000) < 0.01, "tone at the wrong frequency");
    }

    #[test]
    fn render_at_48k_copies_frames_into_any_buffer_size() {
        let mut r = RenderConverter::new(48_000, 2).unwrap();
        let mut n = 0.0;
        let mut out = Vec::new();
        for size in [1, 100, 959, 960, 961, 2000] {
            let mut buf = vec![9.0; size * 2];
            r.fill(&mut buf, |l, right| {
                for (i, s) in l.iter_mut().enumerate() {
                    *s = n + i as f32;
                }
                right.unwrap().fill(-1.0);
                n += FRAME_SAMPLES as f32;
            });
            out.extend_from_slice(&buf);
        }
        let left: Vec<f32> = out.chunks(2).map(|f| f[0]).collect();
        assert!(left.iter().enumerate().all(|(i, &s)| s == i as f32));
        assert!(out.chunks(2).all(|f| f[1] == -1.0));
    }

    #[test]
    fn render_maps_channels() {
        let mut mono = RenderConverter::new(48_000, 1).unwrap();
        let mut buf = vec![0.0; 10];
        mono.fill(&mut buf, |l, right| {
            assert!(right.is_none(), "mono devices get a mono rendering");
            l.fill(0.25);
        });
        assert!(buf.iter().all(|&s| s == 0.25));

        let mut quad = RenderConverter::new(48_000, 4).unwrap();
        let mut buf = vec![9.0; 4 * 10 + 3];
        quad.fill(&mut buf, |l, r| {
            l.fill(0.25);
            r.unwrap().fill(0.5);
        });
        assert!(buf[..40].chunks(4).all(|f| f == [0.25, 0.5, 0.0, 0.0]));
        assert_eq!(buf[40..], [0.0; 3], "a trailing partial frame is silence");
    }

    #[test]
    fn render_resamples_to_the_device_rate() {
        let mut r = RenderConverter::new(44_100, 2).unwrap();
        let mut index = 0;
        let mut out = Vec::new();
        for _ in 0..100 {
            let mut buf = vec![0.0; 441 * 2];
            r.fill(&mut buf, |l, right| {
                let s = sine(1000.0, 48_000, (index + 1) * FRAME_SAMPLES);
                l.copy_from_slice(&s[index * FRAME_SAMPLES..]);
                right.unwrap().fill(0.0);
                index += 1;
            });
            out.extend(buf.chunks(2).map(|f| f[0]));
        }
        assert_eq!(out.len(), 44_100);
        let settled = &out[22_050..];
        assert!((tone(settled, 1000.0, 44_100) - 0.5).abs() < 0.01, "{}", tone(settled, 1000.0, 44_100));
        // One second of output takes 50 frames, plus what the resampler holds back.
        assert!((50..=52).contains(&index), "{index} frames rendered");
    }
}
