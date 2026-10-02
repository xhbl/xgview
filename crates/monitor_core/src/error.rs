use thiserror::Error;

/// Errors produced by the core crate.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("protocol parse error: {0}")]
    Parse(String),

    #[error("rtsp error: {0}")]
    Rtsp(String),

    /// The server refused the transport a request asked for, without saying
    /// anything is wrong with the request itself - a `461` to a `SETUP`. It is
    /// apart from every other failure because the caller has somewhere to go
    /// with it: the other transport.
    #[error("transport refused: {0}")]
    Transport(String),

    #[error("xml error: {0}")]
    Xml(String),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
}

impl CoreError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::Network(message.into())
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse(message.into())
    }

    pub fn rtsp(message: impl Into<String>) -> Self {
        Self::Rtsp(message.into())
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
    }

    pub fn xml(message: impl Into<String>) -> Self {
        Self::Xml(message.into())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }
}

/// Convenience result alias used across the crate.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
