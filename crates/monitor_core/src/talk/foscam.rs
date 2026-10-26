//! The channel for a camera that has no back channel of its own on RTSP.
//!
//! Eight of the nine FOSCAM cameras in this fleet answer an RTSP `DESCRIBE` with
//! two tracks whatever header it carries - only the MGS5030 (`.42`) offers the
//! Profile T media, and it plays nothing into it. All of them take talk-back
//! anyway, over a protocol of their own: a plain TCP connection to the camera's
//! port 88, an HTTP-shaped `SERVERPUSH` request, a login block with the
//! credentials in clear, and then 976-byte frames every 60 ms carrying 480
//! samples of 16-bit PCM. The protocol is documented byte for byte in
//! `docs/foscam_probe.md` in the private tree, and everything below was measured
//! against the fleet on 2026-10-26.
//!
//! Three things about it shape this module:
//!
//! - **No resampling.** [`super::TalkHub`] already produces 8 kHz mono `i16`,
//!   which is exactly the frame's payload. This only groups 480 samples into a
//!   frame and writes it.
//! - **Its own cadence.** 60 ms a packet, not 20 - which is why
//!   [`super::TalkChannel`] asks the channel rather than assuming.
//! - **It says nothing.** The camera answers the login and then never speaks
//!   again: audio frames are not acknowledged, a wrong password gets the same
//!   reply as the right one, and an idle connection is not dropped. So there is
//!   no keep-alive to send, and a read that returns nothing means the camera
//!   closed the connection - not that the call is quiet. What this cannot tell
//!   the viewer is whether the speaker behind it is connected; that is what the
//!   test tone in the settings is for.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::TalkChannel;
use crate::error::CoreError;
use crate::Result;

/// How often the camera wants a frame: 480 samples of 8 kHz audio, 60 ms.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(60);

/// Samples in one frame.
pub const FRAME_SAMPLES: usize = 480;

/// Bytes of audio in one frame.
const PCM_BYTES: usize = FRAME_SAMPLES * 2;

/// A whole frame: sixteen bytes of header and the audio behind them.
const FRAME_BYTES: usize = 16 + PCM_BYTES;

/// How long the camera is given to answer the login.
///
/// Measured at 30-60 ms on the fleet. The wait exists so that a socket that is
/// open but is not this protocol - a web server on the same port, a camera with
/// the feature off - fails the call instead of swallowing audio forever.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(2);

/// One talk connection to a Foscam camera.
pub struct FoscamTalkChannel {
    stream: TcpStream,
    /// A frame that did not fit in the socket, kept so that the next one is not
    /// written into the middle of it: the protocol has no length prefix to
    /// resynchronise on.
    pending: Vec<u8>,
}

impl FoscamTalkChannel {
    /// Connects, asks for the push session, and logs in.
    ///
    /// The login is not checked by the camera - a wrong password is answered
    /// exactly like the right one - so what is checked here is that *something*
    /// speaking this protocol answered at all. See the module note.
    pub async fn open(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|err| CoreError::transport(format!("talk: {host}:{port}: {err}")))?;
        // Audio leaves in 60 ms bursts and the picture is on another socket
        // entirely; Nagle would only add latency to speech.
        let _ = stream.set_nodelay(true);
        let mut channel = Self { stream, pending: Vec::new() };
        channel.write_all(&request(host, port)).await?;
        channel.write_all(&login(user, password)).await?;
        channel.await_login().await?;
        Ok(channel)
    }

    /// Reads the camera's answer to the login.
    ///
    /// Forty-eight bytes on most of the fleet, fifty-two on the one camera whose
    /// firmware differs - so the test is the magic and not the length. A wrong
    /// password is answered the same way, which is why this cannot be a
    /// credential check.
    async fn await_login(&mut self) -> Result<()> {
        let mut reply = [0u8; 64];
        let read = tokio::time::timeout(LOGIN_TIMEOUT, self.stream.read(&mut reply))
            .await
            .map_err(|_| CoreError::transport("talk: the camera did not answer the login"))?;
        let read = read.map_err(|err| CoreError::transport(format!("talk: login: {err}")))?;
        if read < 8 || &reply[4..8] != b"FOSC" {
            return Err(CoreError::transport(
                "talk: the camera answered, but not with this protocol",
            ));
        }
        Ok(())
    }

    /// Reads whatever the camera says, and reports the connection dying.
    ///
    /// It normally says nothing: the only answer in this protocol is the one to
    /// the login. So a read that returns zero bytes is the camera hanging up,
    /// which is the one thing the loop above has to know.
    pub async fn watch(&mut self) -> Result<()> {
        let mut buffer = [0u8; 256];
        match self.stream.read(&mut buffer).await {
            Ok(0) => Err(CoreError::transport("talk: the camera closed the connection")),
            Ok(_) => Ok(()),
            Err(err) => Err(CoreError::transport(format!("talk: {err}"))),
        }
    }

    /// Ends the call. There is nothing to say: the protocol has no teardown, and
    /// the camera notices the socket closing.
    pub async fn close(self) {}

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream
            .write_all(bytes)
            .await
            .map_err(|err| CoreError::transport(format!("talk: {err}")))
    }

    /// Writes as much of `pending` as the socket will take.
    ///
    /// A full socket is not an error - the frame is simply late, and the next
    /// `send` finishes it before starting another. A closed one is.
    fn flush(&mut self) -> Result<()> {
        while !self.pending.is_empty() {
            match self.stream.try_write(&self.pending) {
                Ok(0) => {
                    return Err(CoreError::transport("talk: the camera closed the connection"))
                }
                Ok(written) => {
                    self.pending.drain(..written);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(CoreError::transport(format!("talk: {err}"))),
            }
        }
        Ok(())
    }
}

impl TalkChannel for FoscamTalkChannel {
    fn interval(&self) -> Duration {
        FRAME_INTERVAL
    }

    fn samples(&self) -> usize {
        FRAME_SAMPLES
    }

    fn send(&mut self, samples: &[i16]) -> Result<()> {
        // Whatever is left of the last frame goes first, and a frame that cannot
        // be framed yet is dropped rather than queued: the camera's speaker wants
        // the present, which is the rule the rest of the audio path follows.
        self.flush()?;
        if !self.pending.is_empty() {
            return Ok(());
        }
        self.pending = frame(samples);
        self.flush()
    }
}

/// The `SERVERPUSH` request that opens the connection.
fn request(host: &str, port: u16) -> Vec<u8> {
    format!(
        "SERVERPUSH / HTTP/1.1\r\nHost: {host}:{port}\r\nAccept:*/*\r\nConnection: Close\r\n\r\n"
    )
    .into_bytes()
}

/// The login block: the credentials in clear, padded to 172 bytes.
///
/// The eight bytes after the password are a token the camera's own app sends; an
/// all-zero one is accepted, which was measured rather than assumed.
fn login(user: &str, password: &str) -> Vec<u8> {
    let mut block = Vec::with_capacity(172);
    block.extend_from_slice(&[0x04, 0, 0, 0]);
    block.extend_from_slice(b"FOSC");
    block.extend_from_slice(&[0xa0, 0, 0, 0]);
    block.extend_from_slice(&padded(user));
    block.extend_from_slice(&padded(password));
    block.extend_from_slice(&[0u8; 8]);
    block.resize(172, 0);
    block
}

/// One 976-byte audio frame.
///
/// Sixteen bytes of header and then the audio: the type, the magic, the frame's
/// length less its first twelve bytes (964) and the audio's (960), all little
/// endian. The two lengths agreeing with the frame's real size is what the test
/// below pins - the private capture notes show a fifth word, but 964 + 12 and
/// 960 only add up to a 976-byte frame when the header is these sixteen bytes,
/// and what looked like a fifth word is the first four bytes of a frame whose
/// audio began with silence.
fn frame(samples: &[i16]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_BYTES);
    frame.extend_from_slice(&[0x06, 0, 0, 0]);
    frame.extend_from_slice(b"FOSC");
    frame.extend_from_slice(&(FRAME_BYTES as u32 - 12).to_le_bytes());
    frame.extend_from_slice(&(PCM_BYTES as u32).to_le_bytes());
    for sample in samples.iter().take(FRAME_SAMPLES) {
        frame.extend_from_slice(&sample.to_le_bytes());
    }
    // A short read is padded rather than refused: silence is what the camera
    // would have played anyway, and a frame of the wrong length would desync it.
    frame.resize(FRAME_BYTES, 0);
    frame
}

/// A user name or password, as the block carries it: 32 bytes, NUL padded.
fn padded(text: &str) -> [u8; 32] {
    let mut slot = [0u8; 32];
    let bytes = text.as_bytes();
    let take = bytes.len().min(slot.len());
    slot[..take].copy_from_slice(&bytes[..take]);
    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes measured coming out of the camera's own app, from
    /// `docs/foscam_probe.md` §7.
    #[test]
    fn a_frame_is_the_one_the_protocol_describes() {
        let samples = [1i16, -2, 3];
        let frame = frame(&samples);
        assert_eq!(frame.len(), 976);
        assert_eq!(&frame[..4], &[0x06, 0, 0, 0]);
        assert_eq!(&frame[4..8], b"FOSC");
        assert_eq!(&frame[8..12], &[0xc4, 0x03, 0, 0], "964: the frame less its first twelve");
        assert_eq!(&frame[12..16], &[0xc0, 0x03, 0, 0], "960: the audio");
        assert_eq!(&frame[16..22], &[0x01, 0x00, 0xfe, 0xff, 0x03, 0x00]);
        assert!(frame[22..].iter().all(|byte| *byte == 0), "padded with silence");
    }

    #[test]
    fn the_login_block_is_172_bytes_with_the_credentials_in_clear() {
        let block = login("admin", "secret");
        assert_eq!(block.len(), 172);
        assert_eq!(&block[..8], &[0x04, 0, 0, 0, b'F', b'O', b'S', b'C']);
        assert_eq!(&block[8..12], &[0xa0, 0, 0, 0]);
        assert_eq!(&block[12..17], b"admin");
        assert_eq!(&block[44..50], b"secret");
        assert!(block[84..].iter().all(|byte| *byte == 0), "token and padding");
    }

    /// A credential too long for the block is cut, not refused: the field is
    /// fixed width and the camera reads the first 32 bytes.
    #[test]
    fn a_long_credential_is_cut_to_the_field() {
        let block = login(&"u".repeat(40), &"p".repeat(40));
        assert_eq!(block.len(), 172);
        assert!(block[12..44].iter().all(|byte| *byte == b'u'));
        assert!(block[44..76].iter().all(|byte| *byte == b'p'));
    }

    #[test]
    fn the_request_names_the_camera_in_its_host_header() {
        let request = String::from_utf8(request("192.168.27.40", 88)).unwrap();
        assert!(request.starts_with("SERVERPUSH / HTTP/1.1\r\n"));
        assert!(request.contains("Host: 192.168.27.40:88\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
    }
}
