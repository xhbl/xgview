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
//! * [`rtsp`]       – asynchronous RTSP (TCP interleaved) client.
//! * [`pipeline`]   – non blocking per channel streaming supervisor with
//!   exponential backoff reconnection.
//! * [`autostart`]  – start-on-boot helpers for Windows / Linux / Android.

pub mod autostart;
pub mod config;
pub mod digest;
pub mod discovery;
pub mod error;
pub mod h264;
pub mod layout;
pub mod model;
pub mod pipeline;
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

/// Application author, shown in the about panel.
pub const APP_AUTHOR: &str = "XHBL";

/// Name of the JSON configuration file.
pub const CONFIG_FILE_NAME: &str = "config.json";
