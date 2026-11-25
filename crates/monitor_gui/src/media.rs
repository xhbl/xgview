//! Media keys, from whatever source, before they reach the application.
//!
//! The keys a remote sends do not all arrive through the window system. On a
//! kiosk the media and volume keys may be grabbed by the display server, and
//! the zoom keys have keycodes too high for X11 to name at all. Each platform
//! therefore has its own reader - `evdev` on Linux, the system media controls
//! on Windows and macOS, the activity's key events on Android - and each
//! reader puts what it finds in one channel. This module is the vocabulary
//! they share; the application drains the channel in one place.
//!
//! Nothing here depends on a platform: a command means the same thing wherever
//! it came from, so the actions themselves are written once too.

use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};

/// One action a media key asks for.
///
/// These are not egui keys and are never put in egui's event queue: a text
/// field cannot receive one by accident, and a modal can drop them all with a
/// single decision at the drain point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaCommand {
    StepBack,
    StepForward,
    PrevEvent,
    NextEvent,
    PlayPause,
    VolUp,
    VolDown,
    Mute,
    PictureZoomIn,
    PictureZoomOut,
}

impl MediaCommand {
    /// Whether holding the key should keep repeating the action.
    ///
    /// Seeking and jumping events fire once on the press: a held rewind would
    /// walk back through a recording faster than anyone could aim at a moment.
    /// Volume and picture zoom are continuous, so holding them is useful.
    pub fn repeats_while_held(self) -> bool {
        matches!(
            self,
            Self::VolUp | Self::VolDown | Self::PictureZoomIn | Self::PictureZoomOut
        )
    }
}

/// The physical device a key came from, reduced to what distinguishes one
/// receiver from another.
///
/// Two receivers can send the same code at once - a remote's volume-up and a
/// keyboard's volume-up are two deliberate presses, not one repeated event.
/// Holding state and short-window de-duplication are both keyed by this group,
/// so neither ever merges the two. The string form is built by the readers
/// from the device's phys path and ids; Android has one input source and uses
/// a fixed value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceGroup(String);

impl DeviceGroup {
    /// The single source Android's surfaces front end reports.
    pub const ANDROID_SURFACES: &'static str = "android-surfaces";

    pub fn named(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a platform reader hands to the application.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// A press that acts once. Readers de-duplicate their own repeats before
    /// sending this, so no group is needed here.
    Command(MediaCommand),
    /// The down or up of a holdable key, with the moment it arrived.
    ///
    /// `at` is the reader's `Instant::now()`, not the UI's: a frame that is
    /// late must not start a fresh 400ms wait and make a held key feel stuck.
    Hold {
        group: DeviceGroup,
        command: MediaCommand,
        pressed: bool,
        at: Instant,
    },
    /// A device went away. Any key held on it will never send its release, so
    /// the UI must stop repeating it now.
    DeviceLost(DeviceGroup),
}

/// The channel's ends, named so the readers and the application agree on the
/// event type without each importing `crossbeam_channel`.
pub type MediaTx = Sender<MediaEvent>;
pub type MediaRx = Receiver<MediaEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_continuous_actions_repeat() {
        for command in [
            MediaCommand::VolUp,
            MediaCommand::VolDown,
            MediaCommand::PictureZoomIn,
            MediaCommand::PictureZoomOut,
        ] {
            assert!(command.repeats_while_held(), "{command:?} should repeat");
        }
        for command in [
            MediaCommand::StepBack,
            MediaCommand::StepForward,
            MediaCommand::PrevEvent,
            MediaCommand::NextEvent,
            MediaCommand::PlayPause,
            MediaCommand::Mute,
        ] {
            assert!(!command.repeats_while_held(), "{command:?} should not repeat");
        }
    }

    #[test]
    fn device_groups_with_different_names_stay_apart() {
        let remote = DeviceGroup::named("usb-1/input0:1915:1025");
        let keyboard = DeviceGroup::named("usb-2/input0:040a:2814");
        assert_ne!(remote, keyboard);
        assert_eq!(remote, DeviceGroup::named("usb-1/input0:1915:1025"));
    }
}
