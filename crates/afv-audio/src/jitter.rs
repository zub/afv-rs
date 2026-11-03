//! Jitter buffer for one remote talker, keyed on the AFV `SequenceCounter` (one tick per 20 ms).
//!
//! Replaces the Speex jitter buffer used by afv-native. Playback starts once `target` frames are
//! buffered (or the transmission's last packet has arrived), then advances one sequence per frame.
//! A gap yields [`Pop::Missing`] so the decoder can conceal it. Packets arriving after their slot
//! was played count as late and raise the target delay; a backlog beyond the target plus
//! a few frames is skipped to bound latency. Once the last packet has played, the buffer
//! starts over for the talker's next transmission. Storage is fixed, so pushing and popping never
//! allocate.

use crate::codec::MAX_PACKET;

/// Slots in the ring; also the furthest ahead of the play position a packet may arrive.
const CAPACITY: usize = 64;
/// Initial and minimum frames buffered before playback.
pub const MIN_TARGET: u32 = 2;
/// Upper bound for the adaptive target (200 ms).
pub const MAX_TARGET: u32 = 10;
/// Buffered frames beyond the target that are tolerated before skipping ahead.
const MAX_EXCESS: u32 = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Pop<'a> {
    /// Still buffering; output silence.
    NotReady,
    /// The packet for this frame.
    Packet(&'a [u8]),
    /// This frame's packet is missing; conceal it.
    Missing,
    /// The transmission's last packet has been played.
    Ended,
}

struct Slot {
    sequence: u32,
    len: u16,
    filled: bool,
    data: [u8; MAX_PACKET],
}

pub struct JitterBuffer {
    slots: Box<[Slot]>,
    /// Next sequence to play; `None` while buffering.
    next: Option<u32>,
    target: u32,
    end: Option<u32>,
    /// Last sequence of the transmission played out most recently; stragglers up to it are
    /// dropped while the next transmission buffers.
    finished: Option<u32>,
    buffered: u32,
}

/// `a - b` for sequence numbers that may wrap.
fn diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        let slots = (0..CAPACITY).map(|_| Slot { sequence: 0, len: 0, filled: false, data: [0; MAX_PACKET] }).collect();
        Self { slots, next: None, target: MIN_TARGET, end: None, finished: None, buffered: 0 }
    }

    /// Frames currently held.
    pub fn len(&self) -> u32 {
        self.buffered
    }

    pub fn is_empty(&self) -> bool {
        self.buffered == 0
    }

    /// Current playback delay target in frames.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Drops all packets and starts buffering again. The adaptive target is kept.
    pub fn reset(&mut self) {
        self.slots.iter_mut().for_each(|s| s.filled = false);
        self.next = None;
        self.end = None;
        self.finished = None;
        self.buffered = 0;
    }

    /// Stores a packet. Returns `false` if it was dropped (late, duplicate or oversized).
    pub fn push(&mut self, sequence: u32, packet: &[u8], last: bool) -> bool {
        if packet.len() > MAX_PACKET {
            return false;
        }
        if self.next.is_none()
            && let Some(finished) = self.finished
            && (0..CAPACITY as i32).contains(&diff(finished, sequence))
        {
            return false;
        }
        if let Some(next) = self.next {
            let ahead = diff(sequence, next);
            if ahead < 0 {
                self.target = (self.target + 1).min(MAX_TARGET);
                return false;
            }
            if ahead as usize >= CAPACITY {
                // Too far ahead to be the same stream position: start over from here.
                self.reset();
            }
        }
        let slot = &mut self.slots[sequence as usize % CAPACITY];
        if slot.filled {
            if slot.sequence == sequence {
                return false;
            }
            // Only possible while buffering: an older packet still occupies the slot.
            self.buffered -= 1;
        }
        slot.sequence = sequence;
        slot.len = packet.len() as u16;
        slot.data[..packet.len()].copy_from_slice(packet);
        slot.filled = true;
        self.buffered += 1;
        if last {
            self.end = Some(sequence);
        }
        true
    }

    fn oldest(&self) -> Option<u32> {
        self.slots.iter().filter(|s| s.filled).map(|s| s.sequence).reduce(|a, b| if diff(a, b) < 0 { a } else { b })
    }

    /// Takes the packet for the next 20 ms frame.
    pub fn pop(&mut self) -> Pop<'_> {
        let next = match self.next {
            Some(next) => next,
            None => {
                if self.buffered < self.target && self.end.is_none() {
                    return Pop::NotReady;
                }
                let Some(oldest) = self.oldest() else { return Pop::NotReady };
                oldest
            }
        };
        let mut next = next;
        if let Some(end) = self.end
            && diff(next, end) > 0
        {
            // Played out. Packets of the next transmission, including any already here,
            // buffer up from scratch.
            self.finished = Some(end);
            self.next = None;
            self.end = None;
            return Pop::Ended;
        }
        // Bound latency: skip ahead if far more than the target is buffered.
        while self.buffered > self.target + MAX_EXCESS {
            let slot = &mut self.slots[next as usize % CAPACITY];
            if slot.filled && slot.sequence == next {
                slot.filled = false;
                self.buffered -= 1;
            }
            next = next.wrapping_add(1);
        }
        self.next = Some(next.wrapping_add(1));
        let slot = &mut self.slots[next as usize % CAPACITY];
        if slot.filled && slot.sequence == next {
            slot.filled = false;
            self.buffered -= 1;
            Pop::Packet(&slot.data[..slot.len as usize])
        } else {
            Pop::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32) -> [u8; 4] {
        seq.to_le_bytes()
    }

    #[test]
    fn buffers_then_plays_in_order() {
        let mut jb = JitterBuffer::new();
        jb.push(100, &pkt(100), false);
        assert_eq!(jb.pop(), Pop::NotReady);
        jb.push(101, &pkt(101), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(100)));
        assert_eq!(jb.pop(), Pop::Packet(&pkt(101)));
        assert_eq!(jb.pop(), Pop::Missing);
    }

    #[test]
    fn reorders_and_conceals_gaps() {
        let mut jb = JitterBuffer::new();
        jb.push(11, &pkt(11), false);
        jb.push(10, &pkt(10), false);
        jb.push(13, &pkt(13), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(10)));
        assert_eq!(jb.pop(), Pop::Packet(&pkt(11)));
        assert_eq!(jb.pop(), Pop::Missing);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(13)));
    }

    #[test]
    fn last_packet_starts_playback_and_ends_it() {
        let mut jb = JitterBuffer::new();
        jb.push(5, &pkt(5), true);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(5)));
        assert_eq!(jb.pop(), Pop::Ended);
    }

    #[test]
    fn late_packets_are_dropped_and_raise_the_target() {
        let mut jb = JitterBuffer::new();
        jb.push(1, &pkt(1), false);
        jb.push(2, &pkt(2), false);
        jb.pop();
        jb.pop();
        assert!(!jb.push(1, &pkt(1), false));
        assert_eq!(jb.target(), MIN_TARGET + 1);
    }

    #[test]
    fn duplicates_are_dropped() {
        let mut jb = JitterBuffer::new();
        assert!(jb.push(1, &pkt(1), false));
        assert!(!jb.push(1, &pkt(1), false));
        assert_eq!(jb.len(), 1);
    }

    #[test]
    fn backlog_is_skipped() {
        let mut jb = JitterBuffer::new();
        for s in 0..20 {
            jb.push(s, &pkt(s), false);
        }
        // 20 buffered, target 2, excess 4: skips to keep at most 6 before playing.
        assert_eq!(jb.pop(), Pop::Packet(&pkt(14)));
        assert_eq!(jb.len(), 5);
    }

    #[test]
    fn sequence_wraps() {
        let mut jb = JitterBuffer::new();
        jb.push(u32::MAX, &pkt(u32::MAX), false);
        jb.push(0, &pkt(0), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(u32::MAX)));
        assert_eq!(jb.pop(), Pop::Packet(&pkt(0)));
    }

    #[test]
    fn far_future_packet_resynchronises() {
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0), false);
        jb.push(1, &pkt(1), false);
        jb.pop();
        jb.push(1000, &pkt(1000), false);
        jb.push(1001, &pkt(1001), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(1000)));
    }

    #[test]
    fn target_is_capped() {
        let mut jb = JitterBuffer::new();
        jb.push(100, &pkt(100), false);
        jb.push(101, &pkt(101), false);
        jb.pop();
        for _ in 0..50 {
            jb.push(50, &pkt(50), false);
        }
        assert_eq!(jb.target(), MAX_TARGET);
    }

    #[test]
    fn a_raised_target_delays_the_next_playback_start() {
        let mut jb = JitterBuffer::new();
        jb.push(1, &pkt(1), false);
        jb.push(2, &pkt(2), false);
        jb.pop();
        jb.push(0, &pkt(0), false);
        jb.push(0, &pkt(0), false);
        assert_eq!(jb.target(), MIN_TARGET + 2);
        jb.reset();
        assert_eq!(jb.target(), MIN_TARGET + 2, "reset keeps the learned target");
        for s in 10..13 {
            jb.push(s, &pkt(s), false);
            assert_eq!(jb.pop(), Pop::NotReady);
        }
        jb.push(13, &pkt(13), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(10)));
    }

    #[test]
    fn oversized_packets_are_rejected() {
        let mut jb = JitterBuffer::new();
        assert!(!jb.push(0, &[0; MAX_PACKET + 1], false));
        assert!(jb.push(0, &[7; MAX_PACKET], false));
        jb.push(1, &pkt(1), false);
        assert_eq!(jb.pop(), Pop::Packet(&[7; MAX_PACKET]));
    }

    #[test]
    fn an_empty_last_packet_ends_after_the_gap() {
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0), false);
        jb.push(2, &pkt(2), true);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(0)));
        assert_eq!(jb.pop(), Pop::Missing);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(2)));
        assert_eq!(jb.pop(), Pop::Ended);
        assert_eq!(jb.pop(), Pop::NotReady);
    }

    #[test]
    fn the_next_transmission_plays_after_an_end() {
        // The sender's counter keeps running between transmissions.
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0), false);
        jb.push(1, &pkt(1), true);
        jb.push(15, &pkt(15), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(0)));
        assert_eq!(jb.pop(), Pop::Packet(&pkt(1)));
        assert_eq!(jb.pop(), Pop::Ended);
        assert!(!jb.push(1, &pkt(1), true), "a straggler of the ended transmission");
        assert_eq!(jb.pop(), Pop::NotReady);
        jb.push(16, &pkt(16), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(15)));
        assert_eq!(jb.pop(), Pop::Packet(&pkt(16)));
    }

    #[test]
    fn a_restarted_counter_plays_after_an_end() {
        let mut jb = JitterBuffer::new();
        jb.push(5000, &pkt(5000), true);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(5000)));
        assert_eq!(jb.pop(), Pop::Ended);
        jb.push(0, &pkt(0), false);
        jb.push(1, &pkt(1), false);
        assert_eq!(jb.pop(), Pop::Packet(&pkt(0)));
        assert_eq!(jb.target(), MIN_TARGET, "not counted as late");
    }

    /// Random reordering, loss and duplication around a moving play position.
    #[test]
    fn randomised_arrival_keeps_order_and_bookkeeping() {
        use rand::{RngExt, SeedableRng};
        let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
        let mut next_rand = move |n: u32| rng.random_range(0..n);
        // Start just below the wrap to exercise it.
        let mut jb = JitterBuffer::new();
        let start = u32::MAX - 500;
        let mut sent = start;
        let mut last_played: Option<u32> = None;
        for _ in 0..5000 {
            // The sender produces one packet per frame; the network delivers zero to three of
            // the recent ones in any order.
            sent = sent.wrapping_add(1);
            for _ in 0..next_rand(4) {
                let seq = sent.wrapping_sub(next_rand(12));
                if diff(seq, start) > 0 {
                    jb.push(seq, &pkt(seq), false);
                }
            }
            let filled = jb.slots.iter().filter(|s| s.filled).count() as u32;
            assert_eq!(jb.len(), filled);
            assert!(jb.len() as usize <= CAPACITY);
            if let Pop::Packet(p) = jb.pop() {
                let seq = u32::from_le_bytes(p.try_into().unwrap());
                if let Some(prev) = last_played {
                    assert!(diff(seq, prev) > 0, "{seq} played after {prev}");
                }
                last_played = Some(seq);
            }
            assert!(jb.len() <= jb.target() + MAX_EXCESS + 4);
        }
        assert!(last_played.is_some_and(|s| diff(sent, s) < 32), "playback kept up");
    }
}
