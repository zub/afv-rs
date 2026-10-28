//! Heartbeat bookkeeping for the voice link, kept free of I/O so it can be tested directly.

use tokio::time::Instant;

use crate::config::Timing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    None,
    Degraded,
    Recovered,
    TimedOut,
}

#[derive(Debug)]
pub(crate) struct Liveness {
    degraded_after: u32,
    timeout_after: u32,
    silence_timeout: std::time::Duration,
    /// Heartbeats sent since the last `HA`.
    unacked: u32,
    degraded: bool,
    last_rx: Instant,
}

impl Liveness {
    pub(crate) fn new(timing: &Timing, now: Instant) -> Self {
        Self {
            degraded_after: timing.degraded_after,
            timeout_after: timing.timeout_after,
            silence_timeout: timing.silence_timeout,
            unacked: 0,
            degraded: false,
            last_rx: now,
        }
    }

    /// Call after sending a heartbeat.
    pub(crate) fn heartbeat_sent(&mut self) -> Transition {
        self.unacked += 1;
        if self.unacked >= self.timeout_after {
            return Transition::TimedOut;
        }
        if !self.degraded && self.unacked >= self.degraded_after {
            self.degraded = true;
            return Transition::Degraded;
        }
        Transition::None
    }

    /// Call when an `HA` arrives (in addition to [`Self::packet_received`]).
    pub(crate) fn ack_received(&mut self) -> Transition {
        self.unacked = 0;
        if std::mem::take(&mut self.degraded) { Transition::Recovered } else { Transition::None }
    }

    /// Call for every valid datagram.
    pub(crate) fn packet_received(&mut self, now: Instant) {
        self.last_rx = now;
    }

    /// When the link counts as dead if nothing more arrives.
    pub(crate) fn silence_deadline(&self) -> Instant {
        self.last_rx + self.silence_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liveness() -> Liveness {
        Liveness::new(&Timing::pilot(), Instant::now())
    }

    #[test]
    fn acked_heartbeats_stay_healthy() {
        let mut l = liveness();
        for _ in 0..100 {
            assert_eq!(l.heartbeat_sent(), Transition::None);
            assert_eq!(l.ack_received(), Transition::None);
        }
    }

    #[test]
    fn degrades_then_recovers() {
        let mut l = liveness();
        for _ in 0..4 {
            assert_eq!(l.heartbeat_sent(), Transition::None);
        }
        assert_eq!(l.heartbeat_sent(), Transition::Degraded);
        assert_eq!(l.heartbeat_sent(), Transition::None);
        assert_eq!(l.ack_received(), Transition::Recovered);
        assert_eq!(l.ack_received(), Transition::None);
    }

    #[test]
    fn times_out_after_ten_unacked() {
        let mut l = liveness();
        let transitions: Vec<_> = (0..10).map(|_| l.heartbeat_sent()).collect();
        assert_eq!(transitions[4], Transition::Degraded);
        assert_eq!(transitions[9], Transition::TimedOut);
    }

    #[test]
    fn packets_push_out_the_silence_deadline() {
        let start = Instant::now();
        let mut l = Liveness::new(&Timing::pilot(), start);
        let later = start + std::time::Duration::from_secs(5);
        l.packet_received(later);
        assert_eq!(l.silence_deadline(), later + Timing::pilot().silence_timeout);
    }
}
