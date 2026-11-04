//! CryptoDTO datagram framing.
//!
//! ```text
//! u16 LE header_len | msgpack [ChannelTag, Sequence, Mode] | ciphertext | tag(16)
//! plaintext = u16 LE name_len | name | u16 LE dto_len | msgpack dto
//! nonce     = 00 00 00 00 | Sequence as u64 LE
//! AAD       = header_len field + header
//! ```

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::msgpack::{self, Reader};
use crate::replay::ReplayWindow;
use crate::voice::{Dto, Message};
use crate::{Error, Result};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// The header, the DTO name and the DTO body are each preceded by their length as a
/// little-endian u16.
const LEN_PREFIX: usize = 2;
/// The header is `[ChannelTag, Sequence, Mode]`.
const HEADER_FIELDS: u32 = 3;
/// The nonce is zeros followed by the sequence number as a little-endian u64.
const NONCE_SEQUENCE_AT: usize = NONCE_LEN - size_of::<u64>();

/// Value of the header `Mode` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Plaintext body. Defined by the protocol, never used; rejected on receive.
    None = 1,
    ChaCha20Poly1305 = 2,
}

/// Keys and tag for one voice session, as returned by the API server in `channelConfig`.
///
/// Key names are from the client's point of view: the client encrypts with
/// `transmit_key` and decrypts with `receive_key`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelConfig {
    pub channel_tag: String,
    #[serde(rename = "aeadReceiveKey", with = "base64_key")]
    pub receive_key: [u8; KEY_LEN],
    #[serde(rename = "aeadTransmitKey", with = "base64_key")]
    pub transmit_key: [u8; KEY_LEN],
    // The server also sends `hmacKey`, which is always null and unused.
}

impl ChannelConfig {
    pub fn new(channel_tag: impl Into<String>, receive_key: [u8; KEY_LEN], transmit_key: [u8; KEY_LEN]) -> Self {
        Self { channel_tag: channel_tag.into(), receive_key, transmit_key }
    }

    /// The same channel seen from the other end (keys swapped). Useful for servers and tests.
    pub fn peer(&self) -> Self {
        Self::new(self.channel_tag.clone(), self.transmit_key, self.receive_key)
    }
}

impl fmt::Debug for ChannelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelConfig")
            .field("channel_tag", &self.channel_tag)
            .field("receive_key", &"<redacted>")
            .field("transmit_key", &"<redacted>")
            .finish()
    }
}

mod base64_key {
    use super::*;

    pub fn serialize<S: Serializer>(key: &[u8; KEY_LEN], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&BASE64.encode(key))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; KEY_LEN], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = BASE64.decode(s.trim()).map_err(serde::de::Error::custom)?;
        bytes.try_into().map_err(|_| serde::de::Error::custom(Error::InvalidKey("expected 32 bytes")))
    }
}

/// A successfully decrypted datagram.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub sequence: u64,
    pub message: Message,
}

/// Stateless encoder/decoder for one channel. See [`Channel`] for the stateful wrapper.
#[derive(Clone)]
pub struct Codec {
    tag: String,
    tx: ChaCha20Poly1305,
    rx: ChaCha20Poly1305,
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Codec").field("tag", &self.tag).finish_non_exhaustive()
    }
}

fn nonce(sequence: u64) -> Nonce {
    let mut n = [0u8; NONCE_LEN];
    n[NONCE_SEQUENCE_AT..].copy_from_slice(&sequence.to_le_bytes());
    n.into()
}

fn u16_len(len: usize, what: &'static str) -> Result<[u8; 2]> {
    u16::try_from(len).map(u16::to_le_bytes).map_err(|_| Error::TooLarge(what))
}

impl Codec {
    pub fn new(config: &ChannelConfig) -> Self {
        Self {
            tag: config.channel_tag.clone(),
            tx: ChaCha20Poly1305::new(&config.transmit_key.into()),
            rx: ChaCha20Poly1305::new(&config.receive_key.into()),
        }
    }

    pub fn channel_tag(&self) -> &str {
        &self.tag
    }

    /// Encrypts `dto` into a complete datagram with the given sequence number.
    pub fn seal<D: Dto>(&self, sequence: u64, dto: &D) -> Result<Vec<u8>> {
        let mut body = Vec::with_capacity(64);
        dto.encode(&mut body)?;
        self.seal_raw(sequence, D::NAME, &body)
    }

    /// Encrypts an already-encoded DTO body.
    pub fn seal_raw(&self, sequence: u64, name: &str, body: &[u8]) -> Result<Vec<u8>> {
        let mut header = Vec::with_capacity(self.tag.len() + 16);
        msgpack::write_array_len(&mut header, HEADER_FIELDS as usize)?;
        msgpack::write_str(&mut header, &self.tag)?;
        msgpack::write_uint(&mut header, sequence);
        msgpack::write_uint(&mut header, Mode::ChaCha20Poly1305 as u64);

        let plain_len = LEN_PREFIX + name.len() + LEN_PREFIX + body.len();
        let mut out = Vec::with_capacity(LEN_PREFIX + header.len() + plain_len + TAG_LEN);
        out.extend_from_slice(&u16_len(header.len(), "header")?);
        out.extend_from_slice(&header);
        let aad_len = out.len();

        out.extend_from_slice(&u16_len(name.len(), "DTO name")?);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&u16_len(body.len(), "DTO body")?);
        out.extend_from_slice(body);

        let (aad, plain) = out.split_at_mut(aad_len);
        let tag =
            self.tx.encrypt_in_place_detached(&nonce(sequence), aad, plain).map_err(|_| Error::TooLarge("datagram"))?;
        out.extend_from_slice(&tag);
        Ok(out)
    }

    /// Authenticates and decrypts a datagram. Does not check for replays; see [`Channel`].
    pub fn open(&self, datagram: &[u8]) -> Result<Opened> {
        let prefix = datagram.get(..LEN_PREFIX).ok_or(Error::Truncated)?;
        let header_len = u16::from_le_bytes(prefix.try_into().unwrap()) as usize;
        let aad = datagram.get(..LEN_PREFIX + header_len).ok_or(Error::Truncated)?;

        let mut r = Reader::new(&aad[LEN_PREFIX..]);
        r.array(HEADER_FIELDS)?;
        let tag = r.str()?;
        let sequence: u64 = r.uint("sequence")?;
        let mode: i64 = r.uint("mode")?;

        if mode != Mode::ChaCha20Poly1305 as i64 {
            return Err(Error::UnsupportedMode(mode));
        }
        if tag != self.tag {
            return Err(Error::ChannelTagMismatch);
        }

        let sealed = &datagram[aad.len()..];
        if sealed.len() <= TAG_LEN {
            return Err(Error::Truncated);
        }
        let (cipher, auth_tag) = sealed.split_at(sealed.len() - TAG_LEN);
        let mut plain = cipher.to_vec();
        self.rx
            .decrypt_in_place_detached(&nonce(sequence), aad, &mut plain, Tag::from_slice(auth_tag))
            .map_err(|_| Error::Decrypt)?;

        let (name, body) = split_payload(&plain)?;
        Ok(Opened { sequence, message: Message::decode(name, body)? })
    }
}

fn split_payload(plain: &[u8]) -> Result<(&str, &[u8])> {
    let read_len = |at: usize| -> Result<usize> {
        let b = plain.get(at..at + LEN_PREFIX).ok_or(Error::Truncated)?;
        Ok(u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let name_len = read_len(0)?;
    let name = plain.get(LEN_PREFIX..LEN_PREFIX + name_len).ok_or(Error::Truncated)?;
    let name = std::str::from_utf8(name).map_err(|_| Error::InvalidUtf8)?;
    let body_len = read_len(LEN_PREFIX + name_len)?;
    let body = &plain[2 * LEN_PREFIX + name_len..];
    if body.len() != body_len {
        return Err(Error::LengthMismatch);
    }
    Ok((name, body))
}

/// One end of a voice channel: a [`Codec`] plus the transmit sequence counter and
/// receive replay window.
#[derive(Debug, Clone)]
pub struct Channel {
    codec: Codec,
    next_sequence: u64,
    replay: ReplayWindow,
}

impl Channel {
    pub fn new(config: &ChannelConfig) -> Self {
        Self::continuing(config, 0)
    }

    /// A channel whose transmit sequence starts at `next_sequence`, for a client reconnecting
    /// with new keys. Continuing where the previous channel stopped means the server never sees
    /// the sequence go backwards, whether or not it keeps state from the previous session.
    pub fn continuing(config: &ChannelConfig, next_sequence: u64) -> Self {
        Self { codec: Codec::new(config), next_sequence, replay: ReplayWindow::new() }
    }

    pub fn codec(&self) -> &Codec {
        &self.codec
    }

    /// The sequence number the next sealed datagram will carry.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Encrypts `dto` using the next transmit sequence number.
    pub fn seal<D: Dto>(&mut self, dto: &D) -> Result<Vec<u8>> {
        let datagram = self.codec.seal(self.next_sequence, dto)?;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        Ok(datagram)
    }

    /// Decrypts a datagram and rejects replays.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Message> {
        let opened = self.codec.open(datagram)?;
        if !self.replay.accept(opened.sequence) {
            return Err(Error::Replay(opened.sequence));
        }
        Ok(opened.message)
    }
}
