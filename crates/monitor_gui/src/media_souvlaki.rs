//! Media keys on Windows and macOS, through the system's media controls.
//!
//! Neither platform sends a remote's play or next key as an ordinary key
//! event to a window that has no focus. Both have a media session instead: an
//! application registers as a player, and the OS routes the remote's commands
//! to that session. `souvlaki` is the small cross-platform wrapper over SMTC
//! (Windows) and MPRemoteCommandCenter (macOS).
//!
//! Volume is deliberately absent. A volume key on these systems belongs to
//! the box, and changing the wall's internal slider from it would leave the
//! system volume untouched - the viewer would press the key and hear nothing.

use souvlaki::{MediaControlEvent, MediaControls, MediaPlayback, PlatformConfig};

use crate::media::{MediaCommand, MediaEvent, MediaTx};

const APP_NAME: &str = "XGView";

/// The media session, kept alive for as long as the application runs.
///
/// `attach` registers a callback that outlives this value, but the controls
/// themselves must not be dropped: on Windows that would disable the buttons
/// the OS is watching. Keeping the value in the application is also what lets
/// playback state be published, so the OS knows which session to route to.
pub struct Souvlaki {
    controls: MediaControls,
}

impl Souvlaki {
    /// Opens the media session. On Windows the hwnd is the bare Win32 handle
    /// extracted from the frame; macOS does not need one.
    pub fn new(hwnd: Option<*mut std::ffi::c_void>, tx: MediaTx) -> Result<Self, String> {
        let config = PlatformConfig {
            display_name: APP_NAME,
            dbus_name: "com.xhbl.xgview",
            hwnd,
        };
        let mut controls = MediaControls::new(config).map_err(|err| err.to_string())?;
        controls
            .attach(move |event| {
                // Log every command the OS actually routes here. Whether a
                // remote's rewind / fast-forward keys become SMTC buttons at
                // all is a Windows-side decision, so the log is what tells a
                // dead key from a missing mapping.
                tracing::info!(target: "xgview::media", ?event, "system media command");
                let command = match event {
                    MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
                        Some(MediaCommand::PlayPause)
                    }
                    MediaControlEvent::Next => Some(MediaCommand::NextEvent),
                    MediaControlEvent::Previous => Some(MediaCommand::PrevEvent),
                    MediaControlEvent::Seek(souvlaki::SeekDirection::Forward)
                    | MediaControlEvent::SeekBy(souvlaki::SeekDirection::Forward, _) => {
                        Some(MediaCommand::StepForward)
                    }
                    MediaControlEvent::Seek(souvlaki::SeekDirection::Backward)
                    | MediaControlEvent::SeekBy(souvlaki::SeekDirection::Backward, _) => {
                        Some(MediaCommand::StepBack)
                    }
                    // Volume, position, raise and quit are not ours: the box
                    // owns the first, and the wall has no player to seek in the
                    // sense these commands mean.
                    _ => None,
                };
                if let Some(command) = command {
                    let _ = tx.send(MediaEvent::Command(command));
                }
            })
            .map_err(|err| err.to_string())?;
        if let Err(err) = controls.set_metadata(souvlaki::MediaMetadata {
            title: Some(APP_NAME),
            ..Default::default()
        }) {
            tracing::debug!(target: "xgview::media", %err, "cannot publish media metadata");
        }
        Ok(Self { controls })
    }

    /// Publishes whether a recording is currently open, so the OS routes keys
    /// to a session whose state matches what is on screen.
    pub fn set_playing(&mut self, playing: bool) {
        let state = if playing {
            MediaPlayback::Playing { progress: None }
        } else {
            MediaPlayback::Stopped
        };
        if let Err(err) = self.controls.set_playback(state) {
            tracing::debug!(target: "xgview::media", %err, "cannot publish media playback state");
        }
    }
}
