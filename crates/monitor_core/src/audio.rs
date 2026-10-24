//! Where a channel's decoded audio goes, and the little hub that tells a channel
//! whether it is the one being listened to.
//!
//! The output is owned by the interface, not by the core: the desktop opens an
//! audio device through the platform API, and the core only knows the
//! [`AudioOutput`] trait. A channel worker asks the hub two questions before it
//! spends anything on audio - is this the channel the user is listening to, and
//! is the sink open - so a wall of nine grid tiles decodes exactly one
//! microphone, or none while the sound is off.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use monitor_codec::audio::AudioStreamInfo;

/// The index stored when no channel is the audible one.
pub const NO_AUDIO: usize = usize::MAX;

/// The sink a channel hands its decoded audio to.
///
/// Everything is behind `&self` because the interface owns one sink shared by
/// every channel; the audio thread of the platform API is what really drives
/// it, and `push` only copies into a ring buffer.
pub trait AudioOutput: Send + Sync {
    /// Whether the sink is open and sound is wanted right now.
    ///
    /// A channel skips decoding entirely while this is false, so a muted
    /// application costs no audio work at all.
    fn is_active(&self) -> bool;

    /// Points the open stream at a source's rate, channels and codec.
    ///
    /// Called before the samples of a stream are pushed; an implementation
    /// compares with what it already holds and reopens the device only when
    /// something really changed, so calling it per packet is cheap.
    fn configure(&self, info: &AudioStreamInfo);

    /// Hands one block of interleaved `i16` samples to the device.
    fn push(&self, samples: &[i16]);

    /// No channel is audible any more; the device may be released.
    fn stop(&self);
}

/// Shared state that tells each channel whether it is the audible one.
///
/// A channel index is the position in the enabled camera list, the same one the
/// [`crate::scheduler`] uses. The current index changes when the user zooms or
/// turns the sound off; the output is set once, when the interface opens it.
pub struct AudioHub {
    index: AtomicUsize,
    output: Mutex<Option<Arc<dyn AudioOutput>>>,
}

impl AudioHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { index: AtomicUsize::new(NO_AUDIO), output: Mutex::new(None) })
    }

    /// Installs the sink, or clears it. Called once by the interface.
    pub fn set_output(&self, output: Option<Arc<dyn AudioOutput>>) {
        match self.output.lock() {
            Ok(mut slot) => *slot = output,
            Err(poisoned) => *poisoned.into_inner() = output,
        }
    }

    /// The current sink, if one is installed.
    pub fn output(&self) -> Option<Arc<dyn AudioOutput>> {
        match self.output.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Marks `index` as the channel whose audio should play, `None` for silence.
    pub fn set_index(&self, index: Option<usize>) {
        self.index.store(index.unwrap_or(NO_AUDIO), Ordering::Relaxed);
    }

    /// The channel currently set to play, if any.
    pub fn index(&self) -> Option<usize> {
        match self.index.load(Ordering::Relaxed) {
            NO_AUDIO => None,
            index => Some(index),
        }
    }

    /// Whether the channel `index` is the one that should play its audio now.
    pub fn wants(&self, index: usize) -> bool {
        self.index.load(Ordering::Relaxed) == index
    }
}

impl std::fmt::Debug for AudioHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioHub")
            .field("index", &self.index())
            .field("output", &self.output().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(bool);

    impl AudioOutput for Fake {
        fn is_active(&self) -> bool {
            self.0
        }
        fn configure(&self, _info: &AudioStreamInfo) {}
        fn push(&self, _samples: &[i16]) {}
        fn stop(&self) {}
    }

    #[test]
    fn the_hub_names_one_channel_at_a_time() {
        let hub = AudioHub::new();
        assert_eq!(hub.index(), None);
        assert!(!hub.wants(0));

        hub.set_index(Some(3));
        assert_eq!(hub.index(), Some(3));
        assert!(hub.wants(3));
        assert!(!hub.wants(0));

        hub.set_index(None);
        assert_eq!(hub.index(), None);
        assert!(!hub.wants(3));
    }

    #[test]
    fn the_output_is_installed_and_cleared() {
        let hub = AudioHub::new();
        assert!(hub.output().is_none());
        let output: Arc<dyn AudioOutput> = Arc::new(Fake(true));
        hub.set_output(Some(output.clone()));
        assert!(hub.output().is_some());
        hub.set_output(None);
        assert!(hub.output().is_none());
    }
}