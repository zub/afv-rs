/// Sliding-window replay filter for received datagram sequence numbers.
///
/// Remembers the highest sequence seen and which of the 63 sequences below it have arrived.
/// Newer sequences are always accepted; older ones are accepted once, if still inside the window.
///
/// afv-native tracks the lowest *missing* sequence instead. Both accept every in-order or
/// mildly reordered packet exactly once; they differ only in which very old stragglers they
/// drop, which is local policy and invisible on the wire.
#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    highest: Option<u64>,
    /// Bit `i` set means `highest - i` has been received.
    seen: u64,
}

impl ReplayWindow {
    pub const SIZE: u64 = 64;

    pub fn new() -> Self {
        Self::default()
    }

    /// Records `seq` and returns `true` if it had not been seen before and is not too old.
    pub fn accept(&mut self, seq: u64) -> bool {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            self.seen = 1;
            return true;
        };
        if seq > highest {
            let shift = seq - highest;
            self.seen = if shift >= Self::SIZE { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.highest = Some(seq);
            return true;
        }
        let age = highest - seq;
        if age >= Self::SIZE {
            return false;
        }
        let bit = 1u64 << age;
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_and_duplicates() {
        let mut w = ReplayWindow::new();
        for s in 0..200 {
            assert!(w.accept(s));
            assert!(!w.accept(s));
        }
    }

    #[test]
    fn reordering_inside_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(10));
        assert!(w.accept(12));
        assert!(w.accept(11));
        assert!(!w.accept(11));
        assert!(w.accept(5));
        assert!(!w.accept(12));
    }

    #[test]
    fn too_old_is_rejected() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        assert!(w.accept(37));
        assert!(!w.accept(36));
    }

    #[test]
    fn large_jump_clears_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1));
        assert!(w.accept(1000));
        assert!(!w.accept(1));
        assert!(w.accept(999));
    }

    #[test]
    fn first_packet_need_not_be_zero() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(u64::MAX - 1));
        assert!(w.accept(u64::MAX));
        assert!(!w.accept(u64::MAX - 1));
    }
}
