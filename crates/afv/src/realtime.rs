//! A bridge between real-time audio callbacks and [`Audio`].
//!
//! Audio APIs call back on real-time threads that must not block, allocate or wait on a network.
//! The bridge gives each callback a [`Capture`] or [`Playback`] handle that only moves samples
//! through a lock-free ring buffer and wakes a worker thread. Both sides move whole interleaved
//! frames only, so a ring that runs full or empty never shifts the channels; the worker does all decoding,
//! mixing, encoding and sending through [`Audio::capture`] and [`Audio::render`].
//!
//! Use it to connect an audio API other than `cpal`; the `cpal` backend in [`crate::devices`]
//! is built on it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use afv_audio::{ConvertError, check_format};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::audio::Audio;
use crate::radio::OutputBus;

/// Ring buffer length per stream.
const RING: Duration = Duration::from_millis(500);
/// How long the worker sleeps when no callback wakes it.
const IDLE_WAKE: Duration = Duration::from_millis(5);
/// Rendered audio kept ready beyond what the device asked for in its last callback.
const RENDER_AHEAD: Duration = Duration::from_millis(20);

/// Interleaved `f32` audio format of a device stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
}

/// `n` samples rounded down to whole frames of `channels`.
fn whole(n: usize, channels: usize) -> usize {
    n - n % channels
}

impl Format {
    fn ring_len(self) -> usize {
        (self.sample_rate as f64 * RING.as_secs_f64()) as usize * self.channels as usize
    }
}

/// Feeds microphone samples from a real-time callback.
pub struct Capture {
    ring: Producer<f32>,
    channels: usize,
    wake: Thread,
}

impl Capture {
    /// Queues interleaved samples. Real-time safe. Frames that do not fit are dropped.
    pub fn push(&mut self, samples: &[f32]) {
        self.push_map(samples, |s| s);
    }

    /// Like [`Capture::push`], converting each sample from the device format.
    pub fn push_map<T: Copy>(&mut self, samples: &[T], convert: impl Fn(T) -> f32) {
        let n = whole(samples.len().min(self.ring.slots()), self.channels);
        if let Ok(chunk) = self.ring.write_chunk_uninit(n) {
            chunk.fill_from_iter(samples[..n].iter().map(|&s| convert(s)));
        }
        self.wake.unpark();
    }
}

/// Takes rendered samples in a real-time callback.
pub struct Playback {
    ring: Consumer<f32>,
    channels: usize,
    last_request: Arc<AtomicUsize>,
    wake: Thread,
}

impl Playback {
    /// Fills `out` with interleaved samples. Real-time safe. On underrun the rest is silence.
    pub fn fill(&mut self, out: &mut [f32]) {
        self.fill_map(out, |s| s);
    }

    /// Like [`Playback::fill`], converting each sample to the device format.
    pub fn fill_map<T>(&mut self, out: &mut [T], convert: impl Fn(f32) -> T) {
        self.last_request.store(out.len(), Relaxed);
        let mut filled = 0;
        if let Ok(chunk) = self.ring.read_chunk(whole(out.len().min(self.ring.slots()), self.channels)) {
            filled = chunk.len();
            for (o, s) in out.iter_mut().zip(chunk) {
                *o = convert(s);
            }
        }
        for o in &mut out[filled..] {
            *o = convert(0.0);
        }
        self.wake.unpark();
    }
}

/// The worker thread. Dropping it stops the worker; the handles then stay silent.
pub struct Bridge {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

struct CaptureSide {
    ring: Consumer<f32>,
    format: Format,
}

struct PlaybackSide {
    bus: OutputBus,
    ring: Producer<f32>,
    format: Format,
    last_request: Arc<AtomicUsize>,
}

/// The worker and the handles for its streams: one [`Capture`] if there is a microphone, one
/// [`Playback`] per output in the order given.
pub type Bridged = (Bridge, Option<Capture>, Vec<Playback>);

/// Starts a worker for a microphone stream (if any) and output streams in the given formats.
/// Fails, before starting anything, on a format with no channels, no sample rate, or values no
/// device has.
pub fn bridge(audio: &Audio, mic: Option<Format>, outputs: &[(OutputBus, Format)]) -> Result<Bridged, ConvertError> {
    for format in mic.iter().chain(outputs.iter().map(|(_, f)| f)) {
        check_format(format.sample_rate, format.channels)?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (sides_tx, sides_rx) = std::sync::mpsc::channel();
    let worker = {
        let (audio, stop) = (audio.clone(), stop.clone());
        thread::Builder::new()
            .name("afv-audio".into())
            .spawn(move || {
                if let Ok((capture, playback)) = sides_rx.recv() {
                    run(&audio, &stop, capture, playback);
                }
            })
            .expect("spawning the audio thread")
    };
    let wake = worker.thread().clone();

    let (capture, capture_side) = match mic {
        Some(format) => {
            let (producer, consumer) = RingBuffer::new(format.ring_len());
            (
                Some(Capture { ring: producer, channels: format.channels as usize, wake: wake.clone() }),
                Some(CaptureSide { ring: consumer, format }),
            )
        }
        None => (None, None),
    };
    let mut playback = Vec::new();
    let mut playback_sides = Vec::new();
    for &(bus, format) in outputs {
        let (producer, consumer) = RingBuffer::new(format.ring_len());
        let last_request = Arc::new(AtomicUsize::new(0));
        playback.push(Playback {
            ring: consumer,
            channels: format.channels as usize,
            last_request: last_request.clone(),
            wake: wake.clone(),
        });
        playback_sides.push(PlaybackSide { bus, ring: producer, format, last_request });
    }
    let _ = sides_tx.send((capture_side, playback_sides));
    Ok((Bridge { stop, worker: Some(worker) }, capture, playback))
}

fn run(audio: &Audio, stop: &AtomicBool, mut capture: Option<CaptureSide>, mut playback: Vec<PlaybackSide>) {
    let mut buf = Vec::new();
    while !stop.load(Relaxed) {
        thread::park_timeout(IDLE_WAKE);
        if let Some(c) = &mut capture {
            buf.clear();
            if let Ok(chunk) = c.ring.read_chunk(whole(c.ring.slots(), c.format.channels as usize)) {
                buf.extend(chunk);
            }
            if let Err(e) = audio.capture(&buf, c.format.sample_rate, c.format.channels) {
                tracing::error!("microphone: {e}");
                capture = None;
            }
        }
        for p in &mut playback {
            // Keep the device's next request plus a little more ready, in whole frames.
            let (rate, ch) = (p.format.sample_rate, p.format.channels as usize);
            let capacity = p.ring.buffer().capacity();
            let ahead = (rate as f64 * RENDER_AHEAD.as_secs_f64()) as usize * ch;
            let target = (p.last_request.load(Relaxed) + ahead).min(capacity);
            let queued = capacity - p.ring.slots();
            if queued >= target {
                continue;
            }
            buf.clear();
            buf.resize((target - queued) / ch * ch, 0.0);
            if let Err(e) = audio.render(p.bus, &mut buf, rate, p.format.channels) {
                tracing::error!("{:?} output: {e}", p.bus);
                continue;
            }
            // Fits: only this thread fills the ring, and it had room for `buf`.
            if let Ok(chunk) = p.ring.write_chunk_uninit(buf.len()) {
                chunk.fill_from_iter(buf.iter().copied());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_never_splits_a_frame() {
        let (mut producer, consumer) = RingBuffer::new(16);
        let mut playback =
            Playback { ring: consumer, channels: 2, last_request: Arc::default(), wake: thread::current() };
        // The worker is caught mid-frame: L of the second frame is in, R is not yet.
        for s in [1.0, -1.0, 2.0] {
            producer.push(s).unwrap();
        }
        let mut out = [9.0; 4];
        playback.fill(&mut out);
        assert_eq!(out, [1.0, -1.0, 0.0, 0.0], "underrun is whole frames of silence");
        producer.push(-2.0).unwrap();
        playback.fill(&mut out);
        assert_eq!(out, [2.0, -2.0, 0.0, 0.0], "left stays left");
    }

    #[test]
    fn capture_drops_whole_frames_when_full() {
        let (producer, mut consumer) = RingBuffer::new(5);
        let mut capture = Capture { ring: producer, channels: 2, wake: thread::current() };
        capture.push(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        assert_eq!(consumer.slots(), 4);
        let chunk = consumer.read_chunk(4).unwrap();
        assert_eq!(chunk.into_iter().collect::<Vec<_>>(), [1.0, -1.0, 2.0, -2.0]);
    }
}
