//! Unauthenticated input: whatever arrives on the UDP socket goes through `Channel::open`.
#![no_main]
use afv_proto::{Channel, ChannelConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let config = ChannelConfig::new("tag", [1; 32], [2; 32]);
    let mut channel = Channel::new(&config);
    let _ = channel.open(data);
});
