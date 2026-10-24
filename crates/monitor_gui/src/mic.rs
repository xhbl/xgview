//! The microphone the viewer talks into.
//!
//! The mirror of [`crate::audio`]: that one owns the speaker and plays what a
//! camera sends, this one owns the default input device and captures what the
//! viewer says, so that the magnified camera's speaker can play it. Both share
//! the same `cpal` host - WASAPI on Windows, ALSA on Linux, CoreAudio on macOS,
//! Oboe on Android.
//!
//! The device runs at whatever rate and channel count it prefers, and the
//! camera's back channel expects 8 kHz mono G.711. The conversion is therefore
//! done here, on the audio thread: mix the channels down, average each output
//! sample out of the input samples that fall inside its 125 µs window, and hand
//! the 16 bit result to the [`TalkHub`] the channel worker reads. Averaging is
//! the anti-alias filter - a box of six samples at 48 kHz, which is plenty for
//! speech and needs no state beyond a running sum.
//!
//! The stream is `!Send`, like the output one, so it is parked on the thread
//! that opened it and only stopped by [`shutdown`] on the way out of the window.
//! Leaving it to the thread-local destructor has the same price it does there:
//! a join of an audio thread that is already gone, inside a destructor that
//! cannot unwind, which aborts the process instead of exiting it.

use std::cell::RefCell;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

use monitor_core::talk::TalkHub;

thread_local! {
    /// The live input stream, parked on the thread that opened the device, for
    /// the reason [`crate::audio`] parks the output one.
    static INPUT_STREAM: RefCell<Option<cpal::Stream>> = const { RefCell::new(None) };
}

/// Captures audio into the talk hub.
///
/// The device and its stream live in [`INPUT_STREAM`]; this is the handle that
/// says the microphone was opened, kept by the application for the window's
/// life. There is nothing to stop through it: the stream is `!Send` and is only
/// taken back out by [`shutdown`].
pub struct Mic {
    /// Rate the device runs at, in Hz.
    rate: u32,
}

/// Why the microphone could not be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicError {
    /// The machine has no input device, or the platform refused to open it. A
    /// television, a box with no microphone, a device whose audio service is
    /// busy.
    NoDevice,
    /// Android: the record permission is not granted yet. The system dialog has
    /// been asked for; the switch is pressed again once it has been answered.
    Permission,
}

impl Mic {
    /// Opens the default input device, or says why it could not.
    ///
    /// On Android the record permission comes first: Oboe refuses an input
    /// stream without it, and a refused stream looks exactly like a machine with
    /// no microphone - so the two are told apart here, where the dialog can be
    /// asked for, rather than reported as one failure further up.
    ///
    /// Missing is not fatal anywhere: the picture carries on and the talk switch
    /// simply stays off.
    pub fn open(hub: Arc<TalkHub>) -> Result<Arc<Self>, MicError> {
        #[cfg(target_os = "android")]
        if !crate::android::request_record_audio() {
            return Err(MicError::Permission);
        }
        let device = cpal::default_host()
            .default_input_device()
            .ok_or(MicError::NoDevice)?;
        let supported = device.default_input_config().map_err(|err| {
            tracing::warn!(target: "xgview::mic", error = %err, "no usable input configuration");
            MicError::NoDevice
        })?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        let rate = config.sample_rate.0.max(1);
        let channels = (config.channels as usize).max(1);

        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, hub, rate, channels),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, hub, rate, channels),
            SampleFormat::U16 => build_stream::<u16>(&device, &config, hub, rate, channels),
            SampleFormat::I32 => build_stream::<i32>(&device, &config, hub, rate, channels),
            other => {
                tracing::warn!(target: "xgview::mic", ?other, "unsupported input sample format");
                None
            }
        }
        .ok_or(MicError::NoDevice)?;
        if let Err(err) = stream.play() {
            tracing::warn!(target: "xgview::mic", error = %err, "could not start the microphone");
            return Err(MicError::NoDevice);
        }
        INPUT_STREAM.with(|slot| *slot.borrow_mut() = Some(stream));
        tracing::debug!(target: "xgview::mic", rate, channels, "microphone opened");
        Ok(Arc::new(Self { rate }))
    }

    /// Rate the device was opened at, in Hz.
    pub fn rate(&self) -> u32 {
        self.rate
    }
}

/// Stops the microphone and lets the input device go, on the way out of the
/// window - see [`crate::audio::shutdown`] for why leaving it to the thread
/// local is not enough.
pub fn shutdown() {
    INPUT_STREAM.with(|slot| {
        let stream = match slot.borrow_mut().take() {
            Some(stream) => stream,
            None => return,
        };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(stream))).is_err() {
            tracing::debug!(
                target: "xgview::mic",
                "the microphone did not stop cleanly; ignoring it on the way out"
            );
        }
    });
}

/// Mixes `channels` interleaved samples down to mono and resamples the result
/// to [`monitor_core::talk::RATE`], appending 16 bit samples.
///
/// One output sample is the mean of the input samples whose time falls inside
/// its window. At 48 kHz that is six samples, which is both the anti-alias
/// filter and the downmix; at the 8 kHz a device may already run at, it is the
/// input sample itself.
struct Downsampler {
    /// Input samples per output sample.
    step: f64,
    /// Position inside the current window, in input samples.
    phase: f64,
    /// Running sum and count of the samples falling inside the window.
    sum: f32,
    count: f32,
}

impl Downsampler {
    fn new(from_rate: u32) -> Self {
        let step = (f64::from(from_rate) / f64::from(monitor_core::talk::RATE)).max(1.0);
        Self { step, phase: 0.0, sum: 0.0, count: 0.0 }
    }

    /// Feeds one mono sample, pushing a converted sample for every output
    /// window it completes.
    fn push(&mut self, sample: f32, out: &mut Vec<i16>) {
        self.sum += sample;
        self.count += 1.0;
        self.phase += 1.0;
        while self.phase >= self.step {
            self.phase -= self.step;
            if self.count > 0.0 {
                let value = (self.sum / self.count).clamp(-1.0, 1.0);
                out.push((value * 32_767.0) as i16);
                self.sum = 0.0;
                self.count = 0.0;
            }
        }
    }
}

/// Builds the input stream for one device sample type.
///
/// The callback runs on the platform's own audio thread, so it does as little
/// as it can: convert, downmix, downsample, push. The hub's lock is held for
/// that push and released immediately afterwards.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    hub: Arc<TalkHub>,
    rate: u32,
    channels: usize,
) -> Option<cpal::Stream>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let error = |err| tracing::warn!(target: "xgview::mic", error = %err, "microphone stream error");
    let mut downsampler = Downsampler::new(rate);
    let mut mono: Vec<i16> = Vec::new();
    let data = move |input: &[T], _: &cpal::InputCallbackInfo| {
        // Nothing is captured while the viewer is not talking: the device keeps
        // running (so its start-up delay is paid once), the samples are dropped.
        if !hub.is_talking() {
            return;
        }
        mono.clear();
        let frames = input.len() / channels;
        for frame in 0..frames {
            let mut sum = 0.0f32;
            for channel in 0..channels {
                sum += f32::from_sample(input[frame * channels + channel]);
            }
            downsampler.push(sum / channels as f32, &mut mono);
        }
        if !mono.is_empty() {
            hub.push(&mono);
        }
    };
    match device.build_input_stream(config, data, error, None) {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::warn!(target: "xgview::mic", error = %err, "could not build the microphone stream");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_rate_is_averaged_down_to_8khz() {
        // 48 kHz mono: six input samples to one output sample.
        let mut downsampler = Downsampler::new(48_000);
        let mut out = Vec::new();
        for _ in 0..6 {
            downsampler.push(0.5, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], 16_383);

        // A full second of 48 kHz gives a full second of 8 kHz.
        let mut downsampler = Downsampler::new(48_000);
        let mut out = Vec::new();
        for _ in 0..48_000 {
            downsampler.push(0.0, &mut out);
        }
        assert_eq!(out.len(), 8_000);
    }

    #[test]
    fn a_device_already_at_8khz_passes_through() {
        let mut downsampler = Downsampler::new(8_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            downsampler.push(0.25, &mut out);
        }
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|sample| *sample == 8_191));
    }
}
