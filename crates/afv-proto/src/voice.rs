//! DTOs carried inside encrypted voice datagrams.
//!
//! Every DTO is a MessagePack array with positional fields. Decoders ignore trailing extra
//! elements so newer servers can append fields.

use crate::Result;
use crate::msgpack::{self, Reader};

/// A message type that can travel inside a CryptoDTO datagram.
pub trait Dto: Sized {
    /// The type name sent in front of the body (`"H"`, `"AT"`, ...).
    const NAME: &'static str;
    fn encode(&self, out: &mut Vec<u8>) -> Result<()>;
    fn decode(body: &[u8]) -> Result<Self>;
}

/// `H`: client → server keep-alive, sent every 3 seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub callsign: String,
}

impl Dto for Heartbeat {
    const NAME: &'static str = "H";

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        msgpack::write_array_len(out, 1)?;
        msgpack::write_str(out, &self.callsign)
    }

    fn decode(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let len = r.array(1)?;
        let callsign = r.str()?.to_owned();
        r.skip_rest(len, 1)?;
        Ok(Self { callsign })
    }
}

/// `HA`: server → client heartbeat acknowledgement. Its body carries no information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeartbeatAck;

impl Dto for HeartbeatAck {
    const NAME: &'static str = "HA";

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        msgpack::write_array_len(out, 0)
    }

    fn decode(_body: &[u8]) -> Result<Self> {
        Ok(Self)
    }
}

/// `AT`: one 20 ms Opus frame transmitted on the listed transceivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTx {
    pub callsign: String,
    /// Counts 20 ms frames, including frames where nothing was sent.
    pub sequence: u32,
    /// One Opus packet.
    pub audio: Vec<u8>,
    /// `true` on the final frame after PTT is released.
    pub last_packet: bool,
    /// IDs from the most recent transceiver update.
    pub transceivers: Vec<u16>,
}

impl Dto for AudioTx {
    const NAME: &'static str = "AT";

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        encode_audio_header(out, &self.callsign, self.sequence, &self.audio, self.last_packet)?;
        msgpack::write_array_len(out, self.transceivers.len())?;
        for &id in &self.transceivers {
            msgpack::write_array_len(out, 1)?;
            msgpack::write_uint(out, id.into());
        }
        Ok(())
    }

    fn decode(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let len = r.array(5)?;
        let (callsign, sequence, audio, last_packet) = decode_audio_header(&mut r)?;
        let count = r.array(0)?;
        let mut transceivers = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let fields = r.array(1)?;
            transceivers.push(r.uint("transceiver id")?);
            r.skip_rest(fields, 1)?;
        }
        r.skip_rest(len, 5)?;
        Ok(Self { callsign, sequence, audio, last_packet, transceivers })
    }
}

/// One of the receiving client's transceivers that picked up a transmission.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RxTransceiver {
    /// The receiving client's own transceiver ID.
    pub id: u16,
    /// Frequency of that transceiver in Hz.
    pub frequency: u32,
    /// Signal strength, 0.0–1.0; about 0.1 at the edge of range.
    pub distance_ratio: f32,
}

/// `AR`: one 20 ms Opus frame received from another station.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioRx {
    /// The transmitting station.
    pub callsign: String,
    pub sequence: u32,
    pub audio: Vec<u8>,
    pub last_packet: bool,
    pub transceivers: Vec<RxTransceiver>,
}

impl Dto for AudioRx {
    const NAME: &'static str = "AR";

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        encode_audio_header(out, &self.callsign, self.sequence, &self.audio, self.last_packet)?;
        msgpack::write_array_len(out, self.transceivers.len())?;
        for t in &self.transceivers {
            msgpack::write_array_len(out, 3)?;
            msgpack::write_uint(out, t.id.into());
            msgpack::write_uint(out, t.frequency.into());
            msgpack::write_f32(out, t.distance_ratio);
        }
        Ok(())
    }

    fn decode(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let len = r.array(5)?;
        let (callsign, sequence, audio, last_packet) = decode_audio_header(&mut r)?;
        let count = r.array(0)?;
        let mut transceivers = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let fields = r.array(3)?;
            transceivers.push(RxTransceiver {
                id: r.uint("transceiver id")?,
                frequency: r.uint("frequency")?,
                distance_ratio: r.float()? as f32,
            });
            r.skip_rest(fields, 3)?;
        }
        r.skip_rest(len, 5)?;
        Ok(Self { callsign, sequence, audio, last_packet, transceivers })
    }
}

fn encode_audio_header(out: &mut Vec<u8>, callsign: &str, sequence: u32, audio: &[u8], last: bool) -> Result<()> {
    msgpack::write_array_len(out, 5)?;
    msgpack::write_str(out, callsign)?;
    msgpack::write_uint(out, sequence.into());
    msgpack::write_bin(out, audio)?;
    msgpack::write_bool(out, last);
    Ok(())
}

fn decode_audio_header(r: &mut Reader<'_>) -> Result<(String, u32, Vec<u8>, bool)> {
    Ok((r.str()?.to_owned(), r.uint("sequence counter")?, r.bytes()?.to_vec(), r.bool()?))
}

/// Any message that can be received on a voice channel.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Message {
    Heartbeat(Heartbeat),
    HeartbeatAck,
    AudioTx(AudioTx),
    AudioRx(AudioRx),
    /// A DTO name this crate does not know. Clients should ignore it.
    Unknown {
        name: String,
        body: Vec<u8>,
    },
}

impl Message {
    pub fn decode(name: &str, body: &[u8]) -> Result<Self> {
        Ok(match name {
            Heartbeat::NAME => Self::Heartbeat(Heartbeat::decode(body)?),
            HeartbeatAck::NAME => Self::HeartbeatAck,
            AudioTx::NAME => Self::AudioTx(AudioTx::decode(body)?),
            AudioRx::NAME => Self::AudioRx(AudioRx::decode(body)?),
            _ => Self::Unknown { name: name.to_owned(), body: body.to_vec() },
        })
    }
}

impl From<Heartbeat> for Message {
    fn from(v: Heartbeat) -> Self {
        Self::Heartbeat(v)
    }
}

impl From<HeartbeatAck> for Message {
    fn from(_: HeartbeatAck) -> Self {
        Self::HeartbeatAck
    }
}

impl From<AudioTx> for Message {
    fn from(v: AudioTx) -> Self {
        Self::AudioTx(v)
    }
}

impl From<AudioRx> for Message {
    fn from(v: AudioRx) -> Self {
        Self::AudioRx(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Dto + PartialEq + std::fmt::Debug>(v: T) -> Vec<u8> {
        let mut out = Vec::new();
        v.encode(&mut out).unwrap();
        assert_eq!(T::decode(&out).unwrap(), v);
        out
    }

    #[test]
    fn heartbeat_bytes() {
        let bytes = roundtrip(Heartbeat { callsign: "TEST".into() });
        assert_eq!(bytes, [0x91, 0xa4, b'T', b'E', b'S', b'T']);
    }

    #[test]
    fn audio_tx_bytes() {
        let bytes = roundtrip(AudioTx {
            callsign: "AB".into(),
            sequence: 300,
            audio: vec![0xde, 0xad],
            last_packet: true,
            transceivers: vec![0, 1],
        });
        assert_eq!(
            bytes,
            [
                0x95, // array(5)
                0xa2, b'A', b'B', // "AB"
                0xcd, 0x01, 0x2c, // 300
                0xc4, 0x02, 0xde, 0xad, // bin
                0xc3, // true
                0x92, 0x91, 0x00, 0x91, 0x01, // [[0],[1]]
            ]
        );
    }

    #[test]
    fn audio_rx_roundtrip_and_float64() {
        roundtrip(AudioRx {
            callsign: "SAS123".into(),
            sequence: 7,
            audio: vec![1, 2, 3],
            last_packet: false,
            transceivers: vec![RxTransceiver { id: 1, frequency: 122_800_000, distance_ratio: 0.5 }],
        });

        // Same message with DistanceRatio as float64, audio as str, and an extra trailing field.
        let body = [
            0x96, 0xa1, b'X', 0x07, 0xa1, 0x09, 0xc2, 0x91, 0x93, 0x01, 0xce, 0x07, 0x51, 0xc7, 0x80, 0xcb, 0x3f, 0xe0,
            0, 0, 0, 0, 0, 0, 0xc0,
        ];
        let rx = AudioRx::decode(&body).unwrap();
        assert_eq!(rx.audio, [0x09]);
        assert_eq!(rx.transceivers[0].distance_ratio, 0.5);
        assert_eq!(rx.transceivers[0].frequency, 122_800_000);
    }

    #[test]
    fn unknown_names_are_preserved() {
        let m = Message::decode("XX", &[0x90]).unwrap();
        assert!(matches!(m, Message::Unknown { ref name, .. } if name == "XX"));
    }
}
