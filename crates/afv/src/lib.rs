//! Audio for VATSIM (AFV) client.
//!
//! [`Client`] is the entry point: radios, push-to-talk, events and audio on top of a
//! [`Session`]. Feed its [`Audio`] from your own audio code through [`realtime::bridge`], or
//! enable the `cpal` feature and use `devices::attach`.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use afv::radio::{Position, RadioConfig};
//!
//! let client = afv::Client::builder().credentials("1234567", "password").callsign("SAS123").connect().await?;
//! client.set_position(Position { lat_deg: 60.19, lon_deg: 11.1, height_msl_m: 200.0, height_agl_m: 0.0 });
//! client.add_radio(RadioConfig { tx: true, ..RadioConfig::pilot("122.800".parse()?) });
//! let mut events = client.events();
//! while let Some(event) = events.next().await {
//!     println!("{event:?}");
//! }
//! # Ok(()) }
//! ```

mod api;
pub mod audio;
mod client;
mod config;
#[cfg(feature = "cpal")]
pub mod devices;
mod error;
mod events;
mod frequency;
pub mod radio;
pub mod realtime;
mod session;

pub use afv_audio::{EffectSamples, HfConditions, MicProcessor, Samples};
pub use afv_proto as proto;
pub use audio::Audio;
pub use client::{Client, ClientBuilder};
pub use config::{Credentials, Timing};
pub use error::{AuthError, Error, Result};
pub use events::{Event, Events};
pub use frequency::{Frequency, ParseFrequencyError};
pub use session::{LinkLoss, Session, SessionBuilder, SessionState};
