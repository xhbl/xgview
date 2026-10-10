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

    #[error("http error: {}", .0.with_cause())]
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

/// A failure and everything under it.
///
/// A `reqwest` error's own `Display` stops at the outermost message - "error
/// sending request for url (…)" - which says nothing about *why*, and the why
/// is the only part anyone can act on: a refused connection, an unaccepted
/// certificate, a name that does not resolve. The causes are all in the
/// `source` chain, so they are gathered here rather than left behind.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        cause = inner.source();
    }
    text
}

/// Lets the `Http` variant carry that whole chain.
trait HttpCause {
    fn with_cause(&self) -> String;
}

impl HttpCause for reqwest::Error {
    fn with_cause(&self) -> String {
        with_causes(self)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct Cause;

    impl fmt::Display for Cause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "connection refused")
        }
    }

    impl Error for Cause {}

    #[derive(Debug)]
    struct Failure(Cause);

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "error sending request for url (https://192.168.1.10:8971/api/config)")
        }
    }

    impl Error for Failure {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    /// The reason is what a viewer needs; the outer message alone is not enough
    /// to know whether to check the address, the certificate or the network.
    #[test]
    fn a_failure_is_reported_with_the_cause_under_it() {
        assert_eq!(
            with_causes(&Failure(Cause)),
            "error sending request for url (https://192.168.1.10:8971/api/config): connection refused"
        );
    }

    #[test]
    fn a_failure_without_a_cause_is_left_as_it_is() {
        assert_eq!(with_causes(&Cause), "connection refused");
    }
}
