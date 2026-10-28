use std::io;

/// Why the API server refused to authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// HTTP 400: malformed request or an incompatible client version.
    #[error("request rejected as malformed or from an incompatible client")]
    BadRequest,
    /// HTTP 401.
    #[error("wrong username or password")]
    BadPassword,
    /// HTTP 403: valid credentials that are not allowed to connect (e.g. a suspended account).
    #[error("credentials rejected")]
    Rejected,
    /// The token expires within the refresh margin, which usually means the local clock is wrong.
    #[error("bearer token is already expired; check the system clock")]
    TokenExpired,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("authentication failed: {0}")]
    Auth(#[from] AuthError),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{endpoint} returned HTTP {status}")]
    Status { endpoint: String, status: u16 },
    #[error("unexpected response from {endpoint}: {message}")]
    InvalidResponse { endpoint: String, message: String },
    #[error("UDP error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] afv_proto::Error),
    #[error("not connected")]
    NotConnected,
}

impl Error {
    /// Whether retrying (e.g. reconnecting) cannot help.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Auth(_))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
