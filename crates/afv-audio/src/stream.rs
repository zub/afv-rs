//! One remote talker: jitter buffer, Opus decoder and AGC. Port of afv-native's
//! `RemoteVoiceSource`.

use std::time::{Duration, Instant};

use afv_proto::voice::RxTransceiver;

use crate::Frame;
use crate::codec::{Decoder, OpusError};
use crate::dsp::RmsAgc;
use crate::jitter::{JitterBuffer, Pop};

/// A gap this long between packets starts a new transmission: buffers and decoder are reset.
pub const NEW_TRANSMISSION_GAP: Duration = Duration::from_millis(500);
/// Consecutive frames with an empty buffer after which the stream goes quiet.
const MAX_EMPTY_FRAMES: u32 = 10;

pub struct RemoteStream {
    jitter: JitterBuffer,
    decoder: Decoder,
    agc: RmsAgc,
    transceivers: Vec<RxTransceiver>,
    last_packet_at: Option<Instant>,
    empty_frames: u32,
    active: bool,
}

impl RemoteStream {
    pub fn new() -> Result<Self, OpusError> {
        Ok(Self {
            jitter: JitterBuffer::new(),
            decoder: Decoder::new()?,
            agc: RmsAgc::default(),
            transceivers: Vec::with_capacity(8),
            last_packet_at: None,
            empty_frames: 0,
            active: false,
        })
    }

    pub fn agc_mut(&mut self) -> &mut RmsAgc {
        &mut self.agc
    }

    /// The receiving transceivers listed in the most recent packet.
    pub fn transceivers(&self) -> &[RxTransceiver] {
        &self.transceivers
    }

    /// Whether the stream is currently producing audio.
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn last_packet_at(&self) -> Option<Instant> {
        self.last_packet_at
    }

    /// Adds a received packet.
    pub fn push(&mut self, sequence: u32, opus: &[u8], last: bool, transceivers: &[RxTransceiver], now: Instant) {
        if self.last_packet_at.is_none_or(|t| now.saturating_duration_since(t) > NEW_TRANSMISSION_GAP) {
            self.jitter.reset();
            let _ = self.decoder.reset();
        }
        self.jitter.push(sequence, opus, last);
        self.transceivers.clear();
        self.transceivers.extend_from_slice(transceivers);
        self.last_packet_at = Some(now);
        self.empty_frames = 0;
        self.active = true;
    }

    /// Produces the next 20 ms frame. Returns `false` (and silence) once the stream has gone quiet.
    pub fn next_frame(&mut self, frame: &mut Frame) -> bool {
        if !self.active {
            frame.fill(0.0);
            return false;
        }
        let ok = match self.jitter.pop() {
            Pop::Packet(packet) => self.decoder.decode(packet, frame).is_ok(),
            Pop::Missing => self.decoder.conceal(frame).is_ok(),
            Pop::NotReady => {
                frame.fill(0.0);
                true
            }
            Pop::Ended => false,
        };
        if !ok {
            return self.stop(frame);
        }
        if self.jitter.is_empty() {
            self.empty_frames += 1;
            if self.empty_frames > MAX_EMPTY_FRAMES {
                return self.stop(frame);
            }
        }
        self.agc.process(frame);
        true
    }

    fn stop(&mut self, frame: &mut Frame) -> bool {
        self.active = false;
        frame.fill(0.0);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;
    use crate::codec::Encoder;
    use crate::test_util::{rms, sine_frame};

    /// Opus packets of a 440 Hz tone, one per frame.
    fn packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = Encoder::new().unwrap();
        (0..n).map(|i| enc.encode(&sine_frame(i, 440.0, 0.3)).unwrap().to_vec()).collect()
    }

    fn rx(frequency: u32) -> [RxTransceiver; 1] {
        [RxTransceiver { id: 0, frequency, distance_ratio: 1.0 }]
    }

    #[test]
    fn is_silent_and_inactive_until_the_first_packet() {
        let mut s = RemoteStream::new().unwrap();
        let mut f = [1.0; FRAME_SAMPLES];
        assert!(!s.next_frame(&mut f));
        assert!(!s.is_active());
        assert!(f.iter().all(|&x| x == 0.0));
        assert_eq!(s.last_packet_at(), None);
    }

    #[test]
    fn buffers_then_plays_audio() {
        let p = packets(20);
        let mut s = RemoteStream::new().unwrap();
        let now = Instant::now();
        let mut f = [0.0; FRAME_SAMPLES];
        s.push(0, &p[0], false, &rx(1), now);
        assert!(s.next_frame(&mut f), "still buffering, but active");
        assert!(f.iter().all(|&x| x == 0.0));
        for (i, packet) in p.iter().enumerate().skip(1) {
            s.push(i as u32, packet, false, &rx(1), now);
            assert!(s.next_frame(&mut f));
        }
        assert!(rms(&f) > 0.1, "rms {}", rms(&f));
    }

    #[test]
    fn last_packet_ends_the_stream() {
        let p = packets(2);
        let mut s = RemoteStream::new().unwrap();
        let mut f = [0.0; FRAME_SAMPLES];
        s.push(0, &p[0], false, &rx(1), Instant::now());
        s.push(1, &p[1], true, &rx(1), Instant::now());
        assert!(s.next_frame(&mut f));
        assert!(s.next_frame(&mut f));
        assert!(!s.next_frame(&mut f));
        assert!(!s.is_active());
        assert!(f.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn goes_quiet_after_ten_frames_without_packets() {
        // Without a last packet the stream conceals losses for MAX_EMPTY_FRAMES frames once the
        // buffer runs dry, then stops.
        let p = packets(2);
        let mut s = RemoteStream::new().unwrap();
        let mut f = [0.0; FRAME_SAMPLES];
        s.push(0, &p[0], false, &rx(1), Instant::now());
        s.push(1, &p[1], false, &rx(1), Instant::now());
        let frames = std::iter::from_fn(|| s.next_frame(&mut f).then_some(())).take(100).count();
        // One frame with a packet still buffered, then MAX_EMPTY_FRAMES with an empty buffer.
        assert_eq!(frames, 1 + MAX_EMPTY_FRAMES as usize);
        assert!(!s.is_active());
    }

    #[test]
    fn a_long_gap_starts_a_new_transmission() {
        let p = packets(3);
        let t0 = Instant::now();
        let mut s = RemoteStream::new().unwrap();
        s.push(10, &p[0], false, &rx(1), t0);
        s.push(11, &p[1], false, &rx(1), t0 + Duration::from_millis(100));
        assert_eq!(s.jitter.len(), 2, "a short gap keeps buffered packets");
        s.push(0, &p[2], false, &rx(1), t0 + Duration::from_millis(100) + NEW_TRANSMISSION_GAP * 2);
        assert_eq!(s.jitter.len(), 1, "a long gap drops them");
    }

    #[test]
    fn transceivers_follow_the_latest_packet() {
        let p = packets(2);
        let mut s = RemoteStream::new().unwrap();
        s.push(0, &p[0], false, &rx(1), Instant::now());
        s.push(1, &p[1], false, &rx(2), Instant::now());
        assert_eq!(s.transceivers(), &rx(2));
    }
}
