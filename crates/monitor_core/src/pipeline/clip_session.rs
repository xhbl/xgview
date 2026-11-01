//! One stretch of a recording, played back at the speed it was recorded.
//!
//! Frigate answers `clip.mp4` with fragmented MP4 and no `-re`: ffmpeg pours the
//! whole stretch out as fast as the network takes it, five to ten times faster
//! than real time. So this session is the only one in the pipeline that has to
//! time itself - everything else is paced by the camera.
//!
//! The clock is the presentation timestamp, and it is exact because a window
//! never spans a gap: the request is built from adjacent segments, so `pts` 0 is
//! the window's first wall-clock second and every sample after it is a known
//! distance from it. See the playback plan's §4.4.
//!
//! What it shares with the MSE session is everything after the bytes arrive: the
//! same [`MseParser`](crate::mse::MseParser), the same decoder, the same
//! `FrameStore`. What it cannot share is the framing - a WebSocket message is a
//! whole segment, a chunk of an HTTP body is not.
//!
//! The reading is what has to be held back. `FrameStore` keeps the newest
//! picture and drops the older ones, which is right for a camera and wrong for a
//! recording: pouring the whole response in would play a fifteen-minute window
//! as fast as the network allows. Reading one chunk and playing it out before
//! reading the next bounds the lead to a chunk, and lets TCP's own window hold
//! the server back from there.

use std::time::{Duration, Instant};

use super::shared::{
    BaselineFix, Sinks, StatsWindow, announce_streaming, publish_frames, uses_hardware,
    video_decoder,
};
use super::ws_session::frigate_api_login;
use super::*;

/// How long the response may go quiet before the session is treated as broken.
///
/// A recording is sent as fast as the network allows, so a pause this long is
/// not buffering - it is a server that stopped. Failing is what lets the
/// channel ask for the window again, rather than sit on a picture that will
/// never move with nothing on screen to say why.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest a sample is waited for.
///
/// A timestamp that jumps - a stream whose first fragment carries a time far
/// from the rest - would otherwise hold the picture still for as long as the
/// jump lasts, with nothing on screen to say why.
const LONGEST_WAIT: Duration = Duration::from_secs(2);

/// Plays one window of a recording on one channel.
///
/// Returns when the response ends, which is the recording reaching its last
/// frame: the caller turns that into [`super::channel::SessionEnd::Finished`]
/// rather than a reconnection.
pub(super) async fn run_clip_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    sinks: Sinks<'_>,
    audio: &AudioHub,
) -> Result<()> {
    let Sinks { events, frames, stats } = sinks;
    let Endpoint { index, camera_id, uri, stream, credentials, .. } = endpoint;
    let camera_id_str = camera_id.as_str();

    let mut response = open_clip(&uri, credentials.as_ref()).await?;

    let mut parser = crate::mse::MseParser::new();
    let mut decoder: Option<Box<dyn VideoDecoder>> = None;
    let mut audio_decoder: Option<(Box<dyn AudioDecoder>, AudioStreamInfo)> = None;
    let mut codec: Option<Codec> = None;
    let mut baseline_fix = BaselineFix::new();
    let mut codec_label: Option<String> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut sequence: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut window = StatsWindow::new();
    let mut announced = false;

    // The bytes read and not yet played. A chunk of a body can end anywhere: in
    // the middle of a box, or between a `moof` and the `mdat` it describes.
    let mut pending: Vec<u8> = Vec::new();

    // The clock: `started` is the wall-clock instant `first` is due, and every
    // sample after it waits its own distance from `first`.
    let started = Instant::now();
    let mut first: Option<i64> = None;

    loop {
        let chunk = match tokio::time::timeout(STALL_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            // The end of the response is the end of the recording.
            Ok(Ok(None)) => break,
            Ok(Err(err)) => {
                return Err(crate::error::CoreError::network(format!(
                    "the recording stopped being sent: {err}"
                )));
            }
            Err(_) => {
                return Err(crate::error::CoreError::timeout(format!(
                    "the recording went quiet for {} s",
                    STALL_TIMEOUT.as_secs()
                )));
            }
        };
        window.counters.bytes += chunk.len() as u64;
        pending.extend_from_slice(&chunk);

        // Everything up to the last complete group is played, and the rest is
        // kept for the next read. A `moof` handed over without its `mdat` is
        // dropped by the parser - it keeps the run it is filling in a local -
        // so the framing has to keep them together. See §6.2 of the plan.
        let whole = complete_groups(&pending);
        if whole == 0 {
            continue;
        }
        let parsed = parser.parse_message(&pending[..whole])?;
        pending.drain(..whole);

        if let Some(audio_init) = parsed.audio {
            let mut info =
                AudioStreamInfo::new(audio_init.codec, audio_init.sample_rate, audio_init.channels);
            info.extradata = audio_init.extradata.clone();
            let mut decoder = create_audio_decoder(audio_init.codec);
            match decoder.configure(&info) {
                Ok(()) => audio_decoder = Some((decoder, info)),
                Err(err) => tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    codec = %audio_init.codec,
                    error = %err,
                    "no audio backend for this recording, the channel stays silent"
                ),
            }
        }

        if let Some(mut init) = parsed.init {
            codec = Some(init.codec);
            codec_label = Some(match init.codec {
                Codec::H264 => "H264".to_string(),
                Codec::H265 => "H265".to_string(),
                other => other.as_str().to_string(),
            });
            width = if init.width > 0 { Some(init.width) } else { width };
            height = if init.height > 0 { Some(init.height) } else { height };
            decoder = video_decoder(init.codec, Some(init.width), Some(init.height), index, prefer_hardware);
            baseline_fix.observe(index, init.codec, &mut init.param_sets);
            if let Some(dec) = decoder.as_mut() {
                if !init.param_sets.is_empty() {
                    if let Err(err) = dec.decode(&init.param_sets, 0, true) {
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index,
                            %err,
                            "recording param sets rejected by decoder (non-fatal, may arrive with the first IDR)",
                        );
                    }
                }
            }
            stats.publish(StreamStats {
                index,
                camera_id: camera_id_str.to_string(),
                stream,
                codec: codec_label.clone(),
                width,
                height,
                hardware: uses_hardware(&decoder),
                audio: audio_decoder.is_some(),
                fps: 0.0,
                bitrate_kbps: 0.0,
                total_frames: 0,
            });
        }

        // Picture and sound are played in one order, by the only thing they
        // agree on: the time they are meant to appear. Taking them one stream
        // at a time would run the sound of the whole chunk ahead of the picture
        // it belongs to - a chunk holds seconds of it.
        let mut steps = Vec::with_capacity(parsed.samples.len() + parsed.audio_samples.len());
        steps.extend(parsed.samples.into_iter().map(|sample| (sample.pts_us, Step::Video(sample))));
        steps.extend(
            parsed.audio_samples.into_iter().map(|sample| (sample.pts_us, Step::Audio(sample))),
        );
        steps.sort_by_key(|(pts, _)| *pts);

        for (pts, step) in steps {
            let base = *first.get_or_insert(pts);
            let due = Duration::from_micros(u64::try_from(pts - base).unwrap_or(0));
            let now = started.elapsed();
            if due > now {
                tokio::time::sleep((due - now).min(LONGEST_WAIT)).await;
            }

            match step {
                Step::Audio(sample) => {
                    if audio.wants(index) {
                        if let Some(output) = audio.output().filter(|output| output.is_active()) {
                            if let Some((decoder, info)) = audio_decoder.as_mut() {
                                let mut pcm: Vec<i16> = Vec::new();
                                if decoder.decode(&sample.data, &mut pcm).is_ok() && !pcm.is_empty()
                                {
                                    output.configure(info);
                                    output.push(&pcm);
                                }
                            }
                        }
                    }
                }
                Step::Video(mut sample) => {
                    window.counters.frames += 1;
                    total_frames += 1;
                    announce_streaming(events, index, camera_id_str, stream, &mut announced);

                    if let Some(codec) = codec {
                        baseline_fix.observe(index, codec, &mut sample.data);
                    }

                    // Decoding happens here, after the wait: a picture decoded
                    // early is a picture held in memory for as long as the wait
                    // lasts, and 1080p is three megabytes apiece.
                    if let Some(dec) = decoder.as_mut() {
                        match dec.decode(&sample.data, sample.pts_us, sample.keyframe) {
                            Ok(decoded) => {
                                window.counters.decoded += decoded.len() as u64;
                                if let Some((w, h)) = publish_frames(frames, index, &mut sequence, decoded) {
                                    width = Some(w);
                                    height = Some(h);
                                }
                            }
                            Err(err) => {
                                window.counters.errors += 1;
                                tracing::debug!(
                                    target: "xgview::pipeline",
                                    index,
                                    len = sample.data.len(),
                                    %err,
                                    "recording sample rejected by decoder",
                                );
                            }
                        }
                    }
                }
            }
        }

        if window.due() {
            let seconds = window.seconds();
            stats.publish(StreamStats {
                index,
                camera_id: camera_id_str.to_string(),
                stream,
                codec: codec_label.clone(),
                width,
                height,
                hardware: uses_hardware(&decoder),
                audio: audio_decoder.is_some(),
                fps: window.counters.fps(seconds),
                bitrate_kbps: window.counters.kbps(seconds),
                total_frames,
            });
            window.close();
        }
    }

    tracing::debug!(
        target: "xgview::pipeline",
        index,
        frames = total_frames,
        "the recording was played to its end"
    );
    Ok(())
}

/// One sample of either track, waiting its turn.
enum Step {
    Video(crate::mse::MseSample),
    Audio(crate::mse::MseAudioSample),
}

/// How much of `buf` ends at a group boundary.
///
/// A group is a `moof` and the `mdat` that follows it, or a `moov` on its own.
/// Only whole groups are handed to the parser: it fills in the run it is
/// reading as it walks, so a `moof` whose `mdat` has not arrived yet would be
/// taken and then dropped, and its samples with it. Anything left over waits
/// for the next read.
fn complete_groups(buf: &[u8]) -> usize {
    let mut pos = 0;
    let mut end = 0;
    while pos + 8 <= buf.len() {
        let mut size =
            u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        let header = if size == 1 {
            if pos + 16 > buf.len() {
                break;
            }
            let mut large = [0u8; 8];
            large.copy_from_slice(&buf[pos + 8..pos + 16]);
            size = u64::from_be_bytes(large) as usize;
            16
        } else {
            8
        };
        // A size of 0 runs to the end of the file, which in a stream means "not
        // yet"; anything that does not fit in what has arrived is the same.
        if size == 0 || size < header || pos + size > buf.len() {
            break;
        }
        let kind = &buf[pos + 4..pos + 8];
        pos += size;
        if kind == b"mdat" || kind == b"moov" {
            end = pos;
        }
    }
    end
}

/// Opens the request that carries one window of a recording.
///
/// Frigate's own port serves a self-signed certificate, so the client accepts
/// one - the same decision the MSE WebSocket makes, for the same reason. The
/// login is the same JWT: the WebSocket carries it as a Bearer header, and so
/// does this.
async fn open_clip(uri: &str, credentials: Option<&(String, String)>) -> Result<reqwest::Response> {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        // No overall timeout: a long window is meant to take minutes to play
        // out, and the response is read for as long as the viewer watches it.
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    let mut request = http.get(uri);
    if let Some((user, pass)) = credentials {
        let base = api_base(uri)
            .ok_or_else(|| crate::error::CoreError::network("not an http address"))?;
        let token = frigate_api_login(base, user, pass, Duration::from_secs(10)).await?;
        request = request.bearer_auth(token);
    }

    let response = request.send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(crate::error::CoreError::auth(
            "Frigate refused the account for this recording",
        ));
    }
    if !status.is_success() {
        // A range with no recording in it answers `400`; that is an answer
        // about the recording, not a broken connection.
        return Err(crate::error::CoreError::network(format!(
            "the recording answered {status}"
        )));
    }
    Ok(response)
}

/// The scheme, host and port of an `http`/`https` address.
fn api_base(uri: &str) -> Option<&str> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?']).next()?;
    Some(&uri[..scheme.len() + 3 + authority.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box, as the framing sees it.
    fn boxed(kind: &[u8; 4], body: usize) -> Vec<u8> {
        let size = 8 + body;
        let mut out = (size as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend(std::iter::repeat(0u8).take(body));
        out
    }

    /// Only whole groups are handed over: a `moof` whose `mdat` has not arrived
    /// waits, because the parser would take it and then drop it.
    #[test]
    fn a_group_is_only_complete_once_its_mdat_is_there() {
        let moov = boxed(b"moov", 20);
        let moof = boxed(b"moof", 12);
        let mdat = boxed(b"mdat", 30);

        let mut buf = moov.clone();
        assert_eq!(complete_groups(&buf), moov.len(), "the init segment is a group of its own");

        buf.extend_from_slice(&moof);
        assert_eq!(complete_groups(&buf), moov.len(), "a moof without its mdat is not one");

        buf.extend_from_slice(&mdat);
        assert_eq!(complete_groups(&buf), buf.len(), "both are whole now");

        // And a second group, with only its moof so far.
        buf.extend_from_slice(&moof);
        assert_eq!(complete_groups(&buf), moov.len() + moof.len() + mdat.len());
    }

    /// A box whose declared size runs past what has arrived is not counted, and
    /// neither is anything after it.
    #[test]
    fn a_box_that_runs_past_the_buffer_waits() {
        let moof = boxed(b"moof", 12);
        let mdat = boxed(b"mdat", 30);
        let mut buf = moof.clone();
        buf.extend_from_slice(&mdat[..20]);
        assert_eq!(complete_groups(&buf), 0, "the moof's mdat has not finished arriving");
    }

    /// The base an API login goes to, from the address of a recording.
    #[test]
    fn the_api_base_is_the_scheme_and_the_authority() {
        assert_eq!(
            api_base("https://192.168.17.88:8971/api/cam/start/1/end/2/clip.mp4"),
            Some("https://192.168.17.88:8971")
        );
        assert_eq!(api_base("http://192.168.17.88:8500/api/x"), Some("http://192.168.17.88:8500"));
        assert_eq!(api_base("rtsp://192.168.17.88:554/x"), None);
    }
}
