use afv_proto::voice::{AudioRx, AudioTx, Heartbeat, RxTransceiver};
use afv_proto::{Channel, ChannelConfig, Codec, Error, Message};
use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};

fn config() -> ChannelConfig {
    let rx: [u8; 32] = std::array::from_fn(|i| i as u8);
    let tx: [u8; 32] = std::array::from_fn(|i| 0x80 | i as u8);
    ChannelConfig::new("abc", rx, tx)
}

/// The worked example from docs/PROTOCOL.md §3.7, assembled by hand.
#[test]
fn heartbeat_golden_datagram() {
    let cfg = config();
    let datagram = Codec::new(&cfg).seal(0, &Heartbeat { callsign: "TEST".into() }).unwrap();

    let aad = [0x07, 0x00, 0x93, 0xa3, b'a', b'b', b'c', 0x00, 0x02];
    let mut plain = vec![0x01, 0x00, b'H', 0x06, 0x00, 0x91, 0xa4, b'T', b'E', b'S', b'T'];
    let nonce = [0u8; 12];
    let tag = ChaCha20Poly1305::new(&cfg.transmit_key.into())
        .encrypt_inout_detached(&nonce.into(), &aad, plain.as_mut_slice().into())
        .unwrap();

    let mut expected = aad.to_vec();
    expected.extend_from_slice(&plain);
    expected.extend_from_slice(&tag);
    assert_eq!(datagram, expected);
    assert_eq!(datagram.len(), 36);
}

#[test]
fn nonce_is_sequence_little_endian_after_four_zero_bytes() {
    let cfg = config();
    let seq = 0x0102_0304_0506_0708u64;
    let datagram = Codec::new(&cfg).seal(seq, &Heartbeat { callsign: "X".into() }).unwrap();

    let header_len = u16::from_le_bytes([datagram[0], datagram[1]]) as usize;
    let (aad, sealed) = datagram.split_at(2 + header_len);
    let (cipher, tag) = sealed.split_last_chunk::<16>().unwrap();
    let nonce = [0, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1];
    let mut plain = cipher.to_vec();
    ChaCha20Poly1305::new(&cfg.transmit_key.into())
        .decrypt_inout_detached(&nonce.into(), aad, plain.as_mut_slice().into(), &(*tag).into())
        .expect("nonce layout");
}

#[test]
fn client_and_server_roundtrip() {
    let cfg = config();
    let mut client = Channel::new(&cfg);
    let mut server = Channel::new(&cfg.peer());

    let tx = AudioTx {
        callsign: "SAS123".into(),
        sequence: 42,
        audio: vec![7; 40],
        last_packet: false,
        transceivers: vec![0, 3],
    };
    let dg = client.seal(&tx).unwrap();
    assert_eq!(server.open(&dg).unwrap(), Message::AudioTx(tx));

    let rx = AudioRx {
        callsign: "ENGM_TWR".into(),
        sequence: 9,
        audio: vec![1, 2, 3],
        last_packet: true,
        transceivers: vec![RxTransceiver { id: 0, frequency: 118_300_000, distance_ratio: 0.25 }],
    };
    let dg = server.seal(&rx).unwrap();
    assert_eq!(client.open(&dg).unwrap(), Message::AudioRx(rx));
}

#[test]
fn transmit_sequence_increments() {
    let cfg = config();
    let mut client = Channel::new(&cfg);
    let server = Codec::new(&cfg.peer());
    for expected in 0..3 {
        let dg = client.seal(&Heartbeat { callsign: "X".into() }).unwrap();
        assert_eq!(server.open(&dg).unwrap().sequence, expected);
    }
}

#[test]
fn a_continuing_channel_picks_up_the_sequence() {
    let cfg = config();
    let mut first = Channel::new(&cfg);
    for _ in 0..3 {
        first.seal(&Heartbeat { callsign: "X".into() }).unwrap();
    }
    assert_eq!(first.next_sequence(), 3);
    let mut next = Channel::continuing(&cfg, first.next_sequence());
    let dg = next.seal(&Heartbeat { callsign: "X".into() }).unwrap();
    assert_eq!(Codec::new(&cfg.peer()).open(&dg).unwrap().sequence, 3);
    assert_eq!(next.next_sequence(), 4);
}

#[test]
fn replay_is_rejected() {
    let cfg = config();
    let mut client = Channel::new(&cfg);
    let mut server = Channel::new(&cfg.peer());
    let dg = client.seal(&Heartbeat { callsign: "X".into() }).unwrap();
    server.open(&dg).unwrap();
    assert!(matches!(server.open(&dg), Err(Error::Replay(0))));
}

#[test]
fn tampering_is_detected() {
    let cfg = config();
    let dg = Codec::new(&cfg).seal(5, &Heartbeat { callsign: "X".into() }).unwrap();
    let server = Codec::new(&cfg.peer());

    // Flipping the sequence in the (authenticated) header must fail.
    let mut bad = dg.clone();
    bad[7] ^= 0x01;
    assert!(matches!(server.open(&bad), Err(Error::Decrypt)));

    // Flipping a ciphertext byte must fail.
    let mut bad = dg.clone();
    let last = bad.len() - 20;
    bad[last] ^= 0x01;
    assert!(matches!(server.open(&bad), Err(Error::Decrypt)));

    // Wrong direction key (client decrypting its own packet) must fail.
    assert!(matches!(Codec::new(&cfg).open(&dg), Err(Error::Decrypt)));
}

#[test]
fn foreign_channel_tag_is_rejected() {
    let cfg = config();
    let other = ChannelConfig::new("xyz", cfg.receive_key, cfg.transmit_key);
    let dg = Codec::new(&other).seal(0, &Heartbeat { callsign: "X".into() }).unwrap();
    assert!(matches!(Codec::new(&cfg.peer()).open(&dg), Err(Error::ChannelTagMismatch)));
}

#[test]
fn plaintext_mode_is_rejected() {
    // header [ "abc", 0, 1 ] followed by a plaintext payload
    let dg = [0x07, 0x00, 0x93, 0xa3, b'a', b'b', b'c', 0x00, 0x01, 0x02, 0x00, b'H', b'A', 0x00, 0x00];
    assert!(matches!(Codec::new(&config()).open(&dg), Err(Error::UnsupportedMode(1))));
}

#[test]
fn short_datagrams_are_errors_not_panics() {
    let codec = Codec::new(&config());
    for len in 0..40 {
        let dg = vec![0x07; len];
        assert!(codec.open(&dg).is_err());
    }
}
