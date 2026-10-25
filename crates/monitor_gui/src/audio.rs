//! The audio sink the interface owns.
//!
//! The core decodes a channel's audio and hands it to an [`AudioOutput`]; this is
//! that output, built on `cpal`, which reaches WASAPI on Windows, ALSA / JACK on
//! Linux, CoreAudio on macOS and Oboe on Android through one API. The device is
//! opened once, at whatever rate and channel count it prefers, and the samples a
//! camera sends are resampled into it: the camera is 8 kHz or 16 kHz mono, the
//! device is almost always 48 kHz and often stereo.
//!
//! Everything here is behind `&self` and shared by every channel, because there
//! is one device and one thing being listened to. Decoding happens on a channel
//! worker; the platform's own audio thread drains the ring buffer, so no lock is
//! held for long.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

use monitor_codec::audio::AudioStreamInfo;
use monitor_core::audio::AudioOutput;

/// The sink this platform plays through.
///
/// `cpal`'s, except on Android, where the sound has to leave on the
/// voice-communication path for the platform's echo canceller to have a
/// reference signal: see [`JavaOutput`].
#[cfg(not(target_os = "android"))]
pub type Sink = CpalOutput;
#[cfg(target_os = "android")]
pub type Sink = JavaOutput;

/// The sink on Android: the activity's `AudioTrack`, not a `cpal` stream.
///
/// The reason is the echo canceller. `cpal` opens a media stream, and the
/// platform's canceller only knows what the device is playing when that playing
/// goes out on the voice-communication path - `USAGE_VOICE_COMMUNICATION`, with
/// the audio mode in `MODE_IN_COMMUNICATION` and the speakerphone forced on
/// (that last one is what keeps a communication track out of the earpiece). The
/// capture is already an `AudioRecord` on `VOICE_COMMUNICATION`; this is the
/// other half.
///
/// The track is opened by the activity at the source's own rate - a camera's
/// audio is 8 or 16 kHz mono and `AudioTrack` takes either - so no resampling is
/// done here. Writes are non-blocking: a full track drops the block rather than
/// stalling the channel worker that pushed it, because that worker is also
/// pumping the picture.
#[cfg(target_os = "android")]
pub struct JavaOutput {
    /// Whether sound is wanted; the sound switch drives this.
    active: AtomicBool,
    /// Volume as `0.0..=1.0`, kept as bits so the atomic carries an `f32`.
    volume: AtomicU32,
    /// Rate the track was opened at, 0 while it is not open.
    rate: AtomicU32,
    /// Channel count the track was opened at.
    channels: AtomicU32,
}

#[cfg(target_os = "android")]
impl JavaOutput {
    /// The sink. There is nothing to probe on the device side: the track is
    /// opened when the first block is configured, and a device without a speaker
    /// simply never plays. The name is taken and ignored: Android has no device
    /// to choose, and the call site is the same one the desktops use.
    pub fn open(_name: Option<&str>) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            active: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
        }))
    }

    /// Turns the sound on or off, the switch in the magnified bar driving this.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
        if !active {
            self.close();
        }
    }

    /// Sets the volume, `0.0..=1.0`.
    pub fn set_volume(&self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.volume.store(volume.to_bits(), Ordering::Relaxed);
        crate::android::set_playback_volume(volume);
    }

    /// The last volume set.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Closes the track, so a resume does not play stale audio.
    fn close(&self) {
        self.rate.store(0, Ordering::Relaxed);
        self.channels.store(0, Ordering::Relaxed);
        crate::android::stop_playback();
    }
}

#[cfg(target_os = "android")]
impl AudioOutput for JavaOutput {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    fn configure(&self, info: &AudioStreamInfo) {
        // A track opened while the sound is off would be a call's audio path
        // left open with nothing to play - the mute switch and the talk-back's
        // mute both land here. Nothing to configure until it is wanted again,
        // and the next block after that opens it.
        if !self.is_active() {
            return;
        }
        let rate = info.sample_rate.max(1);
        let channels = info.channels.max(1) as u32;
        if self.rate.load(Ordering::Relaxed) == rate
            && self.channels.load(Ordering::Relaxed) == channels
        {
            return;
        }
        self.close();
        if crate::android::start_playback(rate, channels) {
            self.rate.store(rate, Ordering::Relaxed);
            self.channels.store(channels, Ordering::Relaxed);
            crate::android::set_playback_volume(self.volume());
        }
    }

    fn push(&self, samples: &[i16]) {
        if !self.is_active() || samples.is_empty() || self.rate.load(Ordering::Relaxed) == 0 {
            return;
        }
        crate::android::push_playback(samples);
    }

    fn stop(&self) {
        self.close();
    }
}

/// How much audio the ring buffer is allowed to hold, in seconds. Past this the
/// oldest samples are dropped rather than growing the delay: a live view wants
/// to stay close to the picture, not to a backlog.
const RING_SECONDS: f32 = 1.0;

/// How long a freshly started stream is given to ask for its first block.
///
/// One period is twenty-odd milliseconds on every device measured, so this is
/// only ever spent in full by a device that is not going to work at all.
const CALLBACK_GRACE: Duration = Duration::from_millis(500);

/// How long the stream is left alone to prime itself before the counting starts.
const CALLBACK_SETTLE: Duration = Duration::from_millis(100);

thread_local! {
    /// A live cpal stream is `!Send` - the crate marks it so on purpose - and so
    /// cannot sit in [`CpalOutput`], which is shared with the channel workers.
    /// It is parked on the thread that opened the device instead: the interface
    /// opens the sink on its own thread and keeps it for the life of the window,
    /// which is exactly how long the stream has to run. Dropping it would stop
    /// the sound, so it is only taken back out by [`shutdown`], on the way out.
    static OUTPUT_STREAM: RefCell<Option<Stream>> = const { RefCell::new(None) };
}

/// Stops the sound and lets the output device go, on the way out of the window.
///
/// This exists because of what happens when it is *not* called. The stream is
/// `!Send` and lives in a thread local, so leaving it there hands it to the
/// thread-local destructor at thread exit - and dropping a cpal stream joins its
/// audio thread. By then that thread can already be gone, so the join fails with
/// "threads should not terminate unexpectedly". A panic inside a thread-local
/// destructor cannot unwind - `std` calls `abort_on_dtor_unwind` - so the process
/// dies with `0xC0000409` (`STATUS_STACK_BUFFER_OVERRUN`, `__fastfail` code 7,
/// `FAST_FAIL_FATAL_APP_EXIT`) instead of exiting cleanly. Nothing in the log
/// points at the real culprit: the abort is the *last* thing that happens, and it
/// replaces whatever exit code the viewer meant to return.
///
/// Dropping it here, while the process is still healthy, keeps that panic on an
/// ordinary stack - where it is caught and logged instead of aborting the process.
pub fn shutdown() {
    OUTPUT_STREAM.with(|slot| {
        let stream = match slot.borrow_mut().take() {
            Some(stream) => stream,
            None => return,
        };
        // The join inside this drop is what panics when the audio thread has
        // already terminated. Catching it keeps the exit quiet; the stream is
        // consumed either way and the process is on its way out.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(stream))).is_err() {
            tracing::debug!(
                target: "xgview::audio",
                "the output stream did not stop cleanly; ignoring it on the way out"
            );
        }
    });
}

/// The sink, alive as long as the window is.
pub struct CpalOutput {
    /// Whether sound is wanted. A channel does no audio work while this is false.
    active: AtomicBool,
    /// Volume as `0.0..=1.0`, kept as bits so the atomic carries an `f32`.
    volume: AtomicU32,
    /// Sample rate and channel count of the source being pushed.
    source_rate: AtomicU32,
    source_channels: AtomicU32,
    /// What the device was opened at.
    device_rate: u32,
    device_channels: usize,
    /// Interleaved f32 at the device's rate and channel count.
    buffer: Arc<Mutex<VecDeque<f32>>>,
}

/// The output devices this machine offers, named the way the platform names
/// them.
///
/// The list is what the settings panel shows: a kiosk with a USB dongle and a
/// television on HDMI has two, and the system's default is card 0, which is not
/// always the one with a speaker behind it. Names are the platform's own and are
/// not translated - `plughw:CARD=PCH,DEV=3` is a fact, not a word.
pub fn output_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else { return Vec::new() };
    let mut names: Vec<String> = devices.filter_map(|device| device.name().ok()).collect();
    names.sort();
    names.dedup();
    let devices = labelled(useful_devices(names), true);
    // Once, at the first ask: what this machine offers and what each entry reads
    // as, which is the line a report about "the list is wrong" needs.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let listing: Vec<(&str, &str)> =
            devices.iter().map(|device| (device.name.as_str(), device.label.as_str())).collect();
        tracing::debug!(target: "xgview::audio", ?listing, "the output devices this machine offers");
    });
    devices
}

/// One entry of the settings panel's device list.
///
/// `name` is what the configuration stores - the platform's own string, which is
/// what opens the device again. `label` is what a person reads: ALSA's own words
/// where it has them, the name itself elsewhere.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub name: String,
    pub label: String,
}

/// Names on every platform but Linux, which knows more about them.
#[cfg(not(target_os = "linux"))]
pub(crate) fn labelled(names: Vec<String>, _playback: bool) -> Vec<AudioDevice> {
    names.into_iter().map(|name| AudioDevice { label: name.clone(), name }).collect()
}

/// The same, with ALSA's own description of each device.
///
/// `/proc/asound/card1/pcm3p/info` carries the name ALSA gives a device, and for
/// an HDMI output that name is the monitor's: `HDMI 0 [E65-C3]` for the port the
/// television is on, `HDMI 1 [HDMI 1]` for one with nothing plugged in. That is
/// the difference a person needs to see in a list, and `plughw:CARD=PCH,DEV=3`
/// is not it. A device whose entry cannot be read keeps its name.
#[cfg(target_os = "linux")]
pub(crate) fn labelled(names: Vec<String>, playback: bool) -> Vec<AudioDevice> {
    names
        .into_iter()
        .map(|name| {
            let label = alsa_label(&name, playback).unwrap_or_else(|| name.clone());
            AudioDevice { name, label }
        })
        .collect()
}

/// What one device reads as, whether or not the platform still lists it.
///
/// A device that is open - the one the sound is going through - can drop out of
/// the enumeration, and the settings panel still has to show what it is: the
/// entry that is not in the list is the one the viewer is listening to.
pub fn device_label(name: &str, playback: bool) -> String {
    #[cfg(target_os = "linux")]
    {
        alsa_label(name, playback).unwrap_or_else(|| name.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = playback;
        name.to_string()
    }
}

/// What ALSA calls one PCM device, read out of `/proc`.
///
/// The name is `plughw:CARD=<card id>,DEV=<n>`. The card's line in
/// `/proc/asound/cards` carries its description - `HDA Intel PCH`, `KT USB Audio`
/// - and `pcm<n>p/info` carries the port's own name, `HDMI 0` or `USB Audio`.
/// Together they read as a device: two cards both reporting `USB Audio` are told
/// apart by the first half, and one card's four HDMI ports by the second.
#[cfg(target_os = "linux")]
fn alsa_label(name: &str, playback: bool) -> Option<String> {
    let rest = name.strip_prefix("plughw:CARD=")?;
    let (card, device) = rest.split_once(",DEV=")?;
    let device: u32 = device.parse().ok()?;
    let cards = std::fs::read_to_string("/proc/asound/cards").ok()?;
    let (index, card_name) = cards.lines().find_map(|line| {
        let (head, tail) = line.split_once('[')?;
        let id = tail.split_once(']')?.0.trim();
        if id != card {
            return None;
        }
        let index = head.trim().parse::<u32>().ok()?;
        // ` 1 [PCH            ]: HDA-Intel - HDA Intel PCH`
        let name = tail.split_once(" - ").map(|(_, name)| name.trim().to_string());
        Some((index, name))
    })?;
    let stream = if playback { 'p' } else { 'c' };
    let info =
        std::fs::read_to_string(format!("/proc/asound/card{index}/pcm{device}{stream}/info")).ok()?;
    let port = info.lines().find_map(|line| line.strip_prefix("name: "))?.trim().to_string();
    Some(match card_name {
        Some(card_name) if !card_name.is_empty() => format!("{card_name} · {port}"),
        _ => port,
    })
}

/// The names a person would recognise in a list.
///
/// ALSA publishes every PCM its configuration defines - `default`, `sysdefault`,
/// `front`, `surround51`, `iec958`, `dmix`, `null`, `pulse`, and an `hw:` and a
/// `plughw:` for every device on every card. Most are aliases of the same
/// hardware, and a list of them is a puzzle rather than a choice. What is kept
/// is one `plughw:` per device: the hardware with its rate and format converted
/// for us. A machine whose ALSA configuration names things differently keeps the
/// unfiltered list rather than losing the setting altogether.
#[cfg(target_os = "linux")]
pub(crate) fn useful_devices(names: Vec<String>) -> Vec<String> {
    let filtered: Vec<String> =
        names.iter().filter(|name| name.starts_with("plughw:")).cloned().collect();
    if filtered.is_empty() {
        names
    } else {
        filtered
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn useful_devices(names: Vec<String>) -> Vec<String> {
    names
}

impl CpalOutput {
    /// Opens an output device by the name the platform gave it, or the system's
    /// default when `name` is `None`.
    ///
    /// `None` back means there is nothing to play through - a headless box, a
    /// kiosk without a sound card, or a device that will not start - and is not
    /// a failure: the picture carries on and the sound switch has nothing to
    /// drive. A name that no longer exists falls back to the default rather than
    /// leaving the viewer silent, because a device can be unplugged between two
    /// runs.
    pub fn open(name: Option<&str>) -> Option<Arc<Self>> {
        let host = cpal::default_host();
        let device = match name {
            Some(wanted) => {
                // One pass, logging what the platform offered: when a saved name
                // does not come back, the list is what says why - a device that
                // was unplugged, or a name this build spells differently.
                let mut found = None;
                let mut names = Vec::new();
                if let Ok(devices) = host.output_devices() {
                    for device in devices {
                        let Ok(name) = device.name() else { continue };
                        if name == wanted {
                            found = Some(device);
                            names.push(name);
                            break;
                        }
                        names.push(name);
                    }
                }
                tracing::debug!(
                    target: "xgview::audio",
                    wanted,
                    ?names,
                    "looking for the configured output device"
                );
                match found {
                    Some(device) => device,
                    None => {
                        tracing::warn!(
                            target: "xgview::audio",
                            device = wanted,
                            "the configured output device is not there, using the default"
                        );
                        host.default_output_device()?
                    }
                }
            }
            None => host.default_output_device()?,
        };
        let name = device.name().unwrap_or_else(|_| "?".to_string());
        let supported = device.default_output_config().ok()?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let device_rate = config.sample_rate.0.max(1);
        let device_channels = (config.channels as usize).max(1);
        let capacity = (device_rate as f32 * RING_SECONDS) as usize * device_channels;
        let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));

        let sink = Arc::new(Self {
            active: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            source_rate: AtomicU32::new(device_rate),
            source_channels: AtomicU32::new(device_channels as u32),
            device_rate,
            device_channels,
            buffer: buffer.clone(),
        });

        // A stream that plays but is never asked for data is a state a device can
        // be left in by a backend that does not recognise the events it reports:
        // `cpal`'s ALSA worker on a dmix PCM spins at 100% CPU and never calls
        // the callback at all (measured on a kiosk, 2026-10-25). Silence with a
        // line in the log is a better answer than a core burnt for nothing, so
        // the stream has to prove itself before it is kept.
        let callbacks = Arc::new(AtomicU64::new(0));
        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, buffer, callbacks.clone()),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, buffer, callbacks.clone()),
            SampleFormat::U16 => build_stream::<u16>(&device, &config, buffer, callbacks.clone()),
            SampleFormat::I32 => build_stream::<i32>(&device, &config, buffer, callbacks.clone()),
            other => {
                tracing::warn!(target: "xgview::audio", ?other, "unsupported output sample format");
                None
            }
        }?;
        if let Err(err) = stream.play() {
            tracing::warn!(target: "xgview::audio", error = %err, "could not start the audio stream");
            return None;
        }
        if !asks_for_data(&callbacks) {
            tracing::warn!(
                target: "xgview::audio",
                device = %name,
                "the output device never asked for data, leaving the sound off"
            );
            return None;
        }
        OUTPUT_STREAM.with(|slot| *slot.borrow_mut() = Some(stream));
        tracing::debug!(
            target: "xgview::audio",
            rate = device_rate,
            channels = device_channels,
            device = %name,
            "audio sink opened"
        );
        Some(sink)
    }

    /// Turns the sound on or off, the switch in the magnified bar driving this.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
        if !active {
            self.clear();
        }
    }

    /// Sets the volume, `0.0..=1.0`.
    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// The last volume set.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Empties the ring buffer so a resume does not play stale audio.
    fn clear(&self) {
        match self.buffer.lock() {
            Ok(mut buffer) => buffer.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
    }
}

impl AudioOutput for CpalOutput {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    fn configure(&self, info: &AudioStreamInfo) {
        // The device is never reopened: only the source description changes, and
        // the resampler reads it per push. A rate of 0 would divide by zero.
        self.source_rate.store(info.sample_rate.max(1), Ordering::Relaxed);
        self.source_channels.store(info.channels.max(1) as u32, Ordering::Relaxed);
    }

    fn push(&self, samples: &[i16]) {
        if !self.is_active() || samples.is_empty() {
            return;
        }
        let source_rate = self.source_rate.load(Ordering::Relaxed).max(1) as f32;
        let source_channels = self.source_channels.load(Ordering::Relaxed).max(1) as usize;
        let device_channels = self.device_channels;
        let ratio = self.device_rate as f32 / source_rate;
        let volume = self.volume();
        let frames_in = samples.len() / source_channels;
        if frames_in == 0 {
            return;
        }

        let mut converted: Vec<f32> =
            Vec::with_capacity(((frames_in as f32 * ratio) as usize + 1) * device_channels);
        resample_into(samples, source_channels, device_channels, ratio, volume, &mut converted);

        let max = (self.device_rate as f32 * RING_SECONDS) as usize * device_channels;
        match self.buffer.lock() {
            Ok(mut buffer) => {
                buffer.extend(converted);
                while buffer.len() > max {
                    buffer.pop_front();
                }
            }
            Err(poisoned) => {
                let mut buffer = poisoned.into_inner();
                buffer.extend(converted);
                while buffer.len() > max {
                    buffer.pop_front();
                }
            }
        }
    }

    fn stop(&self) {
        self.clear();
    }
}

/// Resamples one block of interleaved `i16` into interleaved `f32` at the
/// device's rate, channel count and volume.
///
/// A linear interpolation is enough for voice, and the common cases are exact:
/// an 8 kHz or 16 kHz camera into a 48 kHz device is a whole ratio, so no phase
/// is carried between blocks. A source channel count of one spreads over every
/// device channel; otherwise the channels are taken as they come.
fn resample_into(
    samples: &[i16],
    source_channels: usize,
    device_channels: usize,
    ratio: f32,
    volume: f32,
    out: &mut Vec<f32>,
) {
    let frames_in = samples.len() / source_channels;
    if frames_in < 2 {
        return;
    }
    let frames_out = (frames_in as f32 * ratio) as usize;
    for n in 0..frames_out {
        let position = n as f32 / ratio;
        let i0 = position.floor() as usize;
        if i0 + 1 >= frames_in {
            break;
        }
        let frac = position - i0 as f32;
        for channel in 0..device_channels {
            let source = if source_channels == 1 { 0 } else { channel.min(source_channels - 1) };
            let a = samples[i0 * source_channels + source] as f32 / 32768.0;
            let b = samples[(i0 + 1) * source_channels + source] as f32 / 32768.0;
            out.push((a + (b - a) * frac) * volume);
        }
    }
}

/// Builds the output stream for one device sample type.
/// Waits a moment for the stream to keep asking for its data.
///
/// A stream is primed as it starts: a few callbacks arrive before it has settled,
/// and a worker that is not going to work stops after them. So what counts is a
/// callback that arrives *after* the settle - a working device asks every period,
/// twenty-odd milliseconds, and answers within one of them.
fn asks_for_data(callbacks: &AtomicU64) -> bool {
    std::thread::sleep(CALLBACK_SETTLE);
    let before = callbacks.load(Ordering::Relaxed);
    let deadline = Instant::now() + CALLBACK_GRACE;
    while Instant::now() < deadline {
        if callbacks.load(Ordering::Relaxed) > before {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    callbacks: Arc<AtomicU64>,
) -> Option<Stream>
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let error = |err| tracing::warn!(target: "xgview::audio", error = %err, "audio stream error");
    let data = move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
        callbacks.fetch_add(1, Ordering::Relaxed);
        let mut ring = buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for sample in out.iter_mut() {
            // The ring is interleaved the same way the device is, so it is drained
            // flat; a short ring yields silence rather than a stall.
            let value = ring.pop_front().unwrap_or(0.0);
            *sample = T::from_sample(value);
        }
    };
    match device.build_output_stream(config, data, error, None) {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::warn!(target: "xgview::audio", error = %err, "could not build the audio stream");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_widens_a_mono_block_to_the_device() {
        // 8 kHz mono into 48 kHz stereo: six output frames per input frame, each
        // spread over both channels.
        let input: Vec<i16> = (0..8).map(|v| (v * 1000) as i16).collect();
        let mut out = Vec::new();
        resample_into(&input, 1, 2, 6.0, 1.0, &mut out);
        // Every output frame carries two channels.
        assert_eq!(out.len() % 2, 0);
        assert!(!out.is_empty());
        // The first frame is sample 0 on both channels.
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn volume_scales_the_output() {
        let input: Vec<i16> = vec![1000, 1000, 1000, 1000];
        let mut full = Vec::new();
        let mut half = Vec::new();
        resample_into(&input, 1, 1, 1.0, 1.0, &mut full);
        resample_into(&input, 1, 1, 1.0, 0.5, &mut half);
        assert_eq!(full.len(), half.len());
        for (a, b) in full.iter().zip(half.iter()) {
            assert!((a / 2.0 - b).abs() < 1e-6);
        }
    }
}