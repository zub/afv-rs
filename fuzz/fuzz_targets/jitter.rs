//! Peer-controlled input: sequence numbers and payloads of relayed audio packets.
#![no_main]
use afv_audio::jitter::{JitterBuffer, Pop};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Op {
    Push { sequence: u32, len: u16, last: bool },
    Pop,
    Reset,
}

fuzz_target!(|ops: Vec<Op>| {
    let mut jb = JitterBuffer::new();
    let payload = vec![0xa5u8; 1400];
    for op in ops {
        match op {
            Op::Push { sequence, len, last } => {
                let len = (len as usize) % payload.len();
                jb.push(sequence, &payload[..len], last);
            }
            Op::Pop => {
                if let Pop::Packet(p) = jb.pop() {
                    assert!(p.len() <= afv_audio::codec::MAX_PACKET);
                }
            }
            Op::Reset => jb.reset(),
        }
        assert!(jb.len() <= 64, "buffered {} > capacity", jb.len());
    }
});
