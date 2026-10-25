//! The channel for a camera that has no back channel of its own on RTSP.
//!
//! **Empty on purpose.** Nothing here is written yet; this is the place the
//! work lands when it starts, and the notes below are what was established
//! before it could.
//!
//! Eight of the nine FOSCAM cameras measured answer an RTSP `DESCRIBE` with two
//! tracks whatever header it carries - only the MGS5030 (`.42`) offers the
//! Profile T media. All of them take talk-back anyway, over a protocol of their
//! own: a plain TCP connection to the camera's port 88, an HTTP-shaped
//! `SERVERPUSH` request, a login block with the credentials in clear, and then
//! 976-byte frames every 60 ms carrying 480 samples of 16-bit PCM. A
//! reimplementation of that was replayed against a camera and answered, so the
//! route is known - see `docs/foscam_probe.md` in the private tree.
//!
//! What that means for this module when it is written:
//!
//! - **No resampling.** [`super::TalkHub`] already produces 8 kHz mono `i16`,
//!   which is exactly the frame's payload. The packetiser here only groups 480
//!   samples into a frame and writes it.
//! - **Its own cadence.** 60 ms a packet, not 20 - which is why
//!   [`super::TalkChannel`] asks the channel rather than assuming.
//! - **No probe.** Asking costs a login with the password in clear and a
//!   session on the camera, so capability is decided from the model where it is
//!   known, and otherwise by the first press: it works, or it does not.
//! - **One call at a time.** A camera is likely to accept a single talk session;
//!   the bridge has to serialise.
//!
//! It is deliberately not started yet: the standard route works on the fleet
//! (measured, 2026-10-25), and a second protocol brings its own unknowns -
//! token lifetime, whether a session survives many pushes, latency with 60 ms
//! frames over TCP. Those are cheaper to answer once this one is finished.
