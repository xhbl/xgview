//! XGView core library.
//!
//! This crate is platform independent (pure Rust) and contains:
//!
//! * [`model`]      – camera / stream data model.
//! * [`layout`]     – grid layouts (1x1 … 4x4) and pagination math.
//! * [`scheduler`]  – channel scheduler that decides which channel is decoded,
//!   with which stream, on which page.
//! * [`config`]     – cross platform JSON configuration persistence.
//! * [`discovery`]  – ONVIF (WS-Discovery / GetProfiles / GetStreamUri),
//!   TCP fallback port probing and Synology Surveillance Station integration.
//! * [`digest`]     – RFC 2617 / RFC 7616 credentials, shared by the ONVIF HTTP
//!   calls and the RTSP session.
//! * [`h264`]       – H.264 RTP depacketization and access unit reassembly.
//! * [`mjpeg`]      – HTTP MJPEG (`multipart/x-mixed-replace`) client.
//! * [`rtsp`]       – asynchronous RTSP (TCP interleaved) client.
//! * [`pipeline`]   – non blocking per channel streaming supervisor with
//!   exponential backoff reconnection.
//! * [`autostart`]  – start-on-boot helpers for Windows / Linux / Android.
//! * [`power`]      – keep the machine awake and the screen on for a wall that
//!   is meant to stay visible.

pub mod autostart;
pub mod config;
pub mod digest;
pub mod discovery;
pub mod error;
pub mod h264;
pub mod layout;
pub mod mjpeg;
pub mod model;
pub mod pipeline;
pub mod power;
pub mod rtcp;
pub mod rtsp;
pub mod scheduler;

pub use error::{CoreError, Result};
pub use h264::{AccessUnit, H264Depacketizer};
pub use layout::{Direction, GridLayout, NavigateOutcome, PageInfo};
pub use model::{CameraSource, ConnectionState, StreamKind};
pub use scheduler::{ChannelMode, ChannelPlan, Schedule, ScheduleChange, Scheduler};

/// Application identifier used for configuration folders, registry values and
/// start-on-boot registration.
pub const APP_NAME: &str = "xgview";

/// Human readable application name, used for window titles and UI labels.
pub const APP_DISPLAY_NAME: &str = "XGView";

/// Year the project started.
///
/// The copyright shows it on its own while the binary is built in that year,
/// and as a range "start-build" once it is built in a later one. See
/// [`copyright_years`].
pub const APP_START_YEAR: i32 = 2026;

/// The application author, split into the name and the address the manifest
/// declares - `("XHBL", "newxhbl@hotmail.com")` for `XHBL <newxhbl@hotmail.com>`.
///
/// `CARGO_PKG_AUTHORS` joins several authors with `:`; the first one is used,
/// which is the only one this project has. An entry without a `<address>` comes
/// back with an empty address, so callers can fall back to plain text.
pub fn author() -> (&'static str, &'static str) {
    let first = env!("CARGO_PKG_AUTHORS").split(':').next().unwrap_or_default();
    match first.split_once('<') {
        Some((name, rest)) => (name.trim(), rest.trim().trim_end_matches('>').trim()),
        None => (first.trim(), ""),
    }
}

/// The `mailto:` link of the author, with a subject that names the program:
/// `mailto:newxhbl@hotmail.com?subject=%5BXGView%5D%20Inquiry`.
///
/// The subject is `[XGView] Inquiry` with the brackets and the space
/// percent-encoded. The display name it carries is plain, so it needs no
/// escaping of its own.
pub fn author_mailto() -> String {
    let (_, email) = author();
    format!("mailto:{email}?subject=%5B{APP_DISPLAY_NAME}%5D%20Inquiry")
}

/// The copyright years of this build, as one string.
///
/// The start year alone while the binary is built in it - `2026` - and a range
/// once it is built later - `2026-2027`. The build year comes from `build.rs`,
/// through `XGVIEW_BUILD_YEAR`; a build earlier than the start year (a skewed
/// clock) still shows the start year alone.
pub fn copyright_years() -> String {
    let start = APP_START_YEAR;
    let built: i32 = env!("XGVIEW_BUILD_YEAR").parse().unwrap_or(start);
    if built <= start {
        start.to_string()
    } else {
        format!("{start}-{built}")
    }
}

/// Name of the JSON configuration file.
pub const CONFIG_FILE_NAME: &str = "config.json";
