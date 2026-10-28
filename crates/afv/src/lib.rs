//! Audio for VATSIM (AFV) client.
//!
//! ```no_run
//! # async fn example() -> afv::Result<()> {
//! let session = afv::Session::builder()
//!     .credentials("1234567", "password")
//!     .callsign("SAS123")
//!     .connect()
//!     .await?;
//! # Ok(()) }
//! ```

mod api;
mod config;
mod error;
mod liveness;
mod session;

pub use afv_proto as proto;
pub use config::{Credentials, Timing};
pub use error::{AuthError, Error, Result};
pub use session::{ClientBuilder, LinkLoss, Session, SessionState};
