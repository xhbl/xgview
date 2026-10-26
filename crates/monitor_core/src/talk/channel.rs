//! Every way the viewer can speak to a camera, behind one name.
//!
//! The loop in `pipeline` that feeds a call must not know whether the audio
//! leaves as G.711 over an RTSP session or as raw PCM over a socket of the
//! camera's own: it asks how often, how much, and sends. [`TalkChannel`] states
//! that much, and this enum is what the loop actually holds - one variant per
//! protocol the viewer speaks.
//!
//! **Why an enum and not `Box<dyn>`.** The lifecycle below is asynchronous -
//! opening waits on the network, and so does the read that notices a camera
//! hanging up - and an `async fn` in a public trait is not object safe. The
//! project's answer elsewhere is a synchronous trait, which is why
//! [`TalkChannel`] has no `async` in it at all; the parts that must wait are
//! here, on the enum. The second reason is the compiler: a `match` on an enum is
//! exhaustive, so a protocol that forgets one of these methods does not compile,
//! where a trait with defaults would run with the default.
//!
//! **Adding a protocol** is a variant, an arm in each method below, and a file
//! beside `rtsp.rs`. Nothing above this type changes, and nothing in the
//! interface either.
//!
//! [`TalkChannel`]: super::TalkChannel

use std::time::Duration;

use super::rtsp::RtspTalkChannel;
use super::TalkChannel;
use crate::Result;

/// One open channel to one camera's speaker.
pub enum Channel {
    /// The camera's own back channel: an RTSP session carrying `a=sendonly`
    /// audio, as ONVIF Profile T describes it.
    Rtsp(RtspTalkChannel),
}

impl Channel {
    /// Opens the channel a call will speak on.
    ///
    /// `Ok(None)` is a camera that offers nothing this build can speak to, which
    /// is an answer rather than a failure. An `Err` is the camera refusing the
    /// attempt - the transport broke, or the session it was asked for was
    /// refused - and is what the caller reports and, where there is another
    /// protocol to try, falls back from.
    pub async fn open(uri: &str, credentials: Option<(String, String)>) -> Result<Option<Self>> {
        let Some(channel) = RtspTalkChannel::open(uri, credentials).await? else {
            return Ok(None);
        };
        // The interleaved channel is the server's choice and the one fact about
        // this session worth a line: everything after it is audio.
        tracing::debug!(
            target: "xgview::talk",
            interleaved = channel.channel(),
            "a talk session is open"
        );
        Ok(Some(Self::Rtsp(channel)))
    }

    /// What this channel is, for the log line that says a call is live.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Rtsp(_) => "rtsp",
        }
    }

    /// How often the camera's speaker wants a packet.
    pub fn interval(&self) -> Duration {
        match self {
            Self::Rtsp(channel) => channel.interval(),
        }
    }

    /// How many samples one packet carries.
    pub fn samples(&self) -> usize {
        match self {
            Self::Rtsp(channel) => channel.samples(),
        }
    }

    /// Sends one packet. `samples` is exactly [`Channel::samples`] long.
    pub fn send(&mut self, samples: &[i16]) -> Result<()> {
        match self {
            Self::Rtsp(channel) => channel.send(samples),
        }
    }

    /// How often [`Channel::keep_alive`] is due, or `None` when the audio is
    /// itself enough to keep the far end from giving up.
    ///
    /// The difference is the protocol's, not the caller's: RTSP drops a session
    /// that sends it no request however much audio is going the other way, and a
    /// camera's own protocol may treat every packet as proof of life.
    pub fn keep_alive_interval(&self) -> Option<Duration> {
        match self {
            Self::Rtsp(channel) => Some(channel.keep_alive_interval()),
        }
    }

    /// Says one thing to the camera, so the session is not dropped for silence.
    pub async fn keep_alive(&mut self) -> Result<()> {
        match self {
            Self::Rtsp(channel) => channel.keep_alive().await,
        }
    }

    /// Reads whatever the camera says, and reports the connection dying.
    ///
    /// Nothing worth acting on usually arrives - the media is `sendonly` - but
    /// the socket still has to be read: a camera that has closed the session is
    /// only visible as a read that fails.
    pub async fn watch(&mut self) -> Result<()> {
        match self {
            Self::Rtsp(channel) => channel.watch().await,
        }
    }

    /// Ends the call without waiting for the camera to agree.
    pub async fn close(self) {
        match self {
            Self::Rtsp(channel) => channel.close().await,
        }
    }
}
