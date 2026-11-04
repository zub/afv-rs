//! Authenticated input: what a (buggy or hostile) server can put inside a valid datagram. Seals
//! an arbitrary payload with the peer's keys and opens it, so the decoder sees every byte.
#![no_main]
use afv_proto::voice::{AudioRx, AudioTx, Dto, Heartbeat};
use afv_proto::{Channel, ChannelConfig, Codec, Message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else { return };
    let name = match selector % 5 {
        0 => "H",
        1 => "HA",
        2 => "AT",
        3 => "AR",
        _ => "??",
    };
    // Direct DTO decoders.
    let _ = Message::decode(name, body);
    // Re-encoding what decoded must round-trip.
    if let Ok(rx) = AudioRx::decode(body) {
        let mut out = Vec::new();
        if rx.encode(&mut out).is_ok() {
            let again = AudioRx::decode(&out).expect("re-encoded AR decodes");
            assert_eq!(again.callsign, rx.callsign);
            assert_eq!(again.audio, rx.audio);
            assert_eq!(again.transceivers.len(), rx.transceivers.len());
        }
    }
    let _ = AudioTx::decode(body);
    let _ = Heartbeat::decode(body);

    // Through the full channel: server seals, client opens.
    let config = ChannelConfig::new("tag", [1; 32], [2; 32]);
    let server = Codec::new(&config.peer());
    if let Ok(datagram) = server.seal_raw(selector as u64, name, body) {
        let mut client = Channel::new(&config);
        let _ = client.open(&datagram);
    }
});
