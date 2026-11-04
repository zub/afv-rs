/// Errors produced while encoding or decoding protocol data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("input ended unexpectedly")]
    Truncated,
    #[error("expected msgpack {expected}, found marker {marker:#04x}")]
    UnexpectedType { expected: &'static str, marker: u8 },
    #[error("msgpack array has {found} elements, expected at least {expected}")]
    ArrayTooShort { expected: u32, found: u32 },
    #[error("integer out of range for {0}")]
    IntegerOutOfRange(&'static str),
    #[error("msgpack value nested too deeply")]
    TooDeep,
    #[error("string is not valid UTF-8")]
    InvalidUtf8,
    #[error("{0} is too large to encode")]
    TooLarge(&'static str),
    #[error("length field does not match payload size")]
    LengthMismatch,
    #[error("unsupported crypto mode {0}")]
    UnsupportedMode(i64),
    #[error("channel tag does not match")]
    ChannelTagMismatch,
    #[error("authentication failed (wrong key or tampered datagram)")]
    Decrypt,
    #[error("sequence {0} already received or too old")]
    Replay(u64),
    #[error("invalid key: {0}")]
    InvalidKey(&'static str),
    #[error("invalid bearer token: {0}")]
    InvalidToken(&'static str),
}
