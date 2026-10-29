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

    /// The peer was connected and then went quiet, within a deadline that was
    /// ours to set: a handshake that stopped answering, a picture that stopped
    /// arriving, a stall that went on too long.
    ///
    /// Apart from [`CoreError::Network`] because the two are not the same fault
    /// and do not deserve the same sentence in a log: a network error is a
    /// connection that never came up, a timeout is one that did and then
    /// carried nothing. Both are retried, but only one of them is worth
    /// blaming the address for.
    #[error("timeout: {0}")]
    Timeout(String),

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

    /// The camera rejected the credentials, with what it said about them.
    ///
    /// The one failure whose answer does not change with time: asking again
    /// with the same password draws the same `401`. Kept apart so that a
    /// channel can stop rather than count down to another refusal - and so that
    /// a rejection is never read as a camera refusing the *shape* of a session,
    /// which is what [`CoreError::Transport`] is for.
    ///
    /// Carried without a prefix: this message is what a viewer reads on the
    /// tile, and the tile shows forty-six characters - "401 Unauthorized" has
    /// to be among them.
    #[error("{0}")]
    Auth(String),

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

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Timeout(message.into())
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth(message.into())
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

    /// What kind of failure this is, for the code that decides whether to wait.
    ///
    /// The message says what happened; this says whether asking again can
    /// change the answer. It is read off the variant rather than carried beside
    /// the text, so that rewording a message for the viewer cannot change a
    /// retry policy, and so that the hundred-odd places that build an error do
    /// not each have to name a kind.
    pub fn kind(&self) -> FailureKind {
        match self {
            CoreError::Auth(_) => FailureKind::Auth,
            CoreError::Transport(_) => FailureKind::Refused,
            CoreError::Timeout(_) => FailureKind::Timeout,
            // An `io` error that says `TimedOut` is one of ours: a socket read
            // or write that ran past its deadline. The rest - refused, reset,
            // unreachable - are the address's.
            CoreError::Io(err) if err.kind() == std::io::ErrorKind::TimedOut => FailureKind::Timeout,
            CoreError::Io(_) | CoreError::Http(_) | CoreError::Network(_) => FailureKind::Unreachable,
            // The peer answered, and what it answered was not what a session
            // needs: a `4xx`/`5xx`, an SDP or a body that does not parse, a
            // frame the codec cannot take.
            CoreError::Rtsp(_) | CoreError::Parse(_) | CoreError::Xml(_) | CoreError::Json(_) => {
                FailureKind::Protocol
            }
            CoreError::Config(_) | CoreError::Unsupported(_) => FailureKind::Local,
        }
    }
}

/// What a failure is, as far as retrying is concerned.
///
/// A channel's backoff is worth its wait only when the next attempt could meet
/// something different. The message a failure carries is written for the person
/// reading the tile, so it cannot be the thing the backoff reads: this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The camera rejected the credentials.
    Auth,
    /// The server refused the shape of the request, and the caller has
    /// somewhere else to take it - see [`CoreError::Transport`].
    Refused,
    /// Connected, then silent.
    Timeout,
    /// Never connected: refused, unresolved, unreachable, no route.
    Unreachable,
    /// Answered, but not with anything a session can be built from.
    Protocol,
    /// Nothing to do with the camera: a configuration this build cannot use, or
    /// a feature this platform does not have.
    Local,
}

impl FailureKind {
    /// Whether waiting can change the answer.
    ///
    /// A rejected password stays rejected, and a configuration error stays an
    /// error: both are answered by the viewer, not by time. Everything else is
    /// worth another attempt after a backoff.
    pub fn is_permanent(self) -> bool {
        matches!(self, FailureKind::Auth | FailureKind::Local)
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

    /// The talk worker reads a protocol refusal as "this camera will not take a
    /// call of its own" and reopens the picture's session to carry the media.
    /// A rejected password must not be read that way: it says nothing about the
    /// shape of the session, and reopening only repeats the refusal - on a
    /// session that then cannot be opened either.
    #[test]
    fn a_rejected_password_is_not_a_refusal_of_this_shape() {
        let rejected = CoreError::auth("SETUP failed: 401 Unauthorized (digest)");
        assert_eq!(rejected.kind(), FailureKind::Auth);
        assert_ne!(rejected.kind(), FailureKind::Protocol);

        let refused = CoreError::transport("SETUP refused the transport: 461 Unsupported Transport");
        assert_eq!(refused.kind(), FailureKind::Refused);
    }

    /// A picture that stops arriving is not an address that cannot be reached.
    /// Both are retried, but only the second is worth blaming the address for -
    /// and the log line a viewer is told to read has to say which one it was.
    #[test]
    fn a_silence_after_the_connection_is_a_timeout() {
        assert_eq!(CoreError::timeout("no media received for 10 s").kind(), FailureKind::Timeout);
        assert_eq!(
            CoreError::rtsp("PLAY failed: 500 Internal Server Error").kind(),
            FailureKind::Protocol
        );
        assert_eq!(
            CoreError::network("mjpeg stream closed by the server").kind(),
            FailureKind::Unreachable
        );
    }

    /// Only an answer the viewer can change stops the countdown. Everything
    /// else is worth another attempt, which is what the backoff is for.
    #[test]
    fn only_an_answer_the_viewer_can_change_stops_the_retries() {
        for kind in [FailureKind::Auth, FailureKind::Local] {
            assert!(kind.is_permanent(), "{kind:?} should stop the retries");
        }
        for kind in [
            FailureKind::Refused,
            FailureKind::Timeout,
            FailureKind::Unreachable,
            FailureKind::Protocol,
        ] {
            assert!(!kind.is_permanent(), "{kind:?} should be retried");
        }
    }
}
