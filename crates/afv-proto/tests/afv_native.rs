//! Compares sealing and opening against datagrams sealed by afv-native's own CryptoDTO code.
//!
//! `tests/data/afv_native_datagrams.txt` was produced by `tests/data/afv_native_datagrams.cpp`,
//! compiled against afv-native (commit fc7d216), msgpack-cxx 9.0.0 and OpenSSL. Each vector
//! below builds the same message; afv-proto must seal it to the same bytes and open afv-native's
//! datagram to the same message.

use afv_proto::voice::{AudioRx, AudioTx, Dto, Heartbeat, RxTransceiver};
use afv_proto::{ChannelConfig, Codec, Message};

const TAG: &str = "a1b2c3d4-tag";

/// The client transmits with `k1` and receives with `k2`, as in the C++ generator.
fn client() -> ChannelConfig {
    let k1: [u8; 32] = std::array::from_fn(|i| i as u8);
    let k2: [u8; 32] = std::array::from_fn(|i| 0x80 + i as u8);
    ChannelConfig::new(TAG, k2, k1)
}

/// Whether a vector was sealed by the client or by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sender {
    Client,
    Server,
}

struct Vector {
    label: &'static str,
    sender: Sender,
    sequence: u64,
    message: Message,
}

fn vectors() -> Vec<Vector> {
    let at = AudioTx {
        callsign: "SAS123".into(),
        sequence: 300,
        audio: vec![0xde, 0xad, 0xbe, 0xef],
        last_packet: false,
        transceivers: vec![0, 1],
    };
    let at_last = AudioTx { sequence: 301, last_packet: true, transceivers: vec![0], ..at.clone() };
    let at_big = AudioTx {
        callsign: "X".repeat(40),
        sequence: 70_000,
        audio: (0..300).map(|i| i as u8).collect(),
        last_packet: false,
        transceivers: (0..20).map(|i| i * 1000).collect(),
    };
    let at_edge = AudioTx {
        callsign: "ENGM_ATIS".into(),
        sequence: u32::MAX,
        audio: vec![],
        last_packet: true,
        transceivers: vec![],
    };
    let ar = AudioRx {
        callsign: "ENGM_TWR".into(),
        sequence: 5,
        audio: vec![1, 2, 3],
        last_packet: false,
        transceivers: vec![
            RxTransceiver { id: 0, frequency: 122_800_000, distance_ratio: 0.5 },
            RxTransceiver { id: 3, frequency: 118_300_000, distance_ratio: 0.123 },
        ],
    };
    let v = |label, sender, sequence, message: Message| Vector { label, sender, sequence, message };
    vec![
        v("hb", Sender::Client, 0, Heartbeat { callsign: "TEST".into() }.into()),
        v("at", Sender::Client, 1, at.into()),
        v("at_last", Sender::Client, 2, at_last.into()),
        v("at_big", Sender::Client, 1 << 40, at_big.into()),
        v("at_edge", Sender::Client, u64::MAX - 1, at_edge.into()),
        v("ar", Sender::Server, 7, ar.into()),
    ]
}

/// The datagrams afv-native sealed, by label.
fn afv_native() -> Vec<(String, Sender, Vec<u8>)> {
    let text = include_str!("data/afv_native_datagrams.txt");
    text.lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let [label, sender, hex] = fields[..] else { panic!("bad line {line:?}") };
            let sender = match sender {
                "client" => Sender::Client,
                "server" => Sender::Server,
                other => panic!("bad sender {other:?}"),
            };
            let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
            (label.to_owned(), sender, bytes)
        })
        .collect()
}

fn sealer(sender: Sender) -> Codec {
    match sender {
        Sender::Client => Codec::new(&client()),
        Sender::Server => Codec::new(&client().peer()),
    }
}

fn seal(codec: &Codec, sequence: u64, message: &Message) -> Vec<u8> {
    fn go<D: Dto>(codec: &Codec, sequence: u64, dto: &D) -> Vec<u8> {
        codec.seal(sequence, dto).unwrap()
    }
    match message {
        Message::Heartbeat(m) => go(codec, sequence, m),
        Message::AudioTx(m) => go(codec, sequence, m),
        Message::AudioRx(m) => go(codec, sequence, m),
        other => panic!("no vector for {other:?}"),
    }
}

#[test]
fn every_vector_has_an_afv_native_datagram() {
    let labels: Vec<_> = afv_native().into_iter().map(|(label, ..)| label).collect();
    let expected: Vec<_> = vectors().iter().map(|v| v.label.to_owned()).collect();
    assert_eq!(labels, expected);
}

#[test]
fn seals_the_same_bytes_as_afv_native() {
    for (v, (_, sender, datagram)) in vectors().iter().zip(afv_native()) {
        assert_eq!(sender, v.sender, "{}", v.label);
        assert_eq!(seal(&sealer(v.sender), v.sequence, &v.message), datagram, "{}", v.label);
    }
}

#[test]
fn opens_afv_native_datagrams() {
    for (v, (_, sender, datagram)) in vectors().iter().zip(afv_native()) {
        let receiver = match sender {
            Sender::Client => Sender::Server,
            Sender::Server => Sender::Client,
        };
        let opened = sealer(receiver).open(&datagram).unwrap_or_else(|e| panic!("{}: {e}", v.label));
        assert_eq!(opened.sequence, v.sequence, "{}", v.label);
        assert_eq!(opened.message, v.message, "{}", v.label);
    }
}
