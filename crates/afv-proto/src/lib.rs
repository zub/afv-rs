//! Wire protocol for Audio for VATSIM (AFV).
//!
//! This crate performs no I/O. It contains:
//!
//! - [`frame`]: the CryptoDTO datagram format (length-prefixed MessagePack header,
//!   ChaCha20-Poly1305 body) and [`Channel`], which adds sequence numbering and replay protection.
//! - [`voice`]: the DTOs exchanged over UDP (`H`, `HA`, `AT`, `AR`).
//! - [`api`]: request/response types and paths for the REST control plane.
//! - [`consts`]: timing and audio parameters clients are expected to follow.
//!
//! See `docs/PROTOCOL.md` in the repository for the full protocol description.

pub mod api;
pub mod consts;
mod error;
pub mod frame;
mod msgpack;
mod replay;
pub mod voice;

pub use error::Error;
pub use frame::{Channel, ChannelConfig, Codec, Opened};
pub use replay::ReplayWindow;
pub use voice::Message;

pub type Result<T, E = Error> = std::result::Result<T, E>;
