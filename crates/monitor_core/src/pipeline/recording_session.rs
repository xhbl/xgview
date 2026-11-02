//! A recording, played back at the speed it was recorded.
//!
//! Two shapes of it, one session. A window is one request for one `clip.mp4`;
//! a range is a playlist of fragments fetched one at a time. What differs is
//! where the bytes come from, which is [`Recording`]; everything after them -
//! the framing, the parser, the clock, the decoder, the speaker - is the same,
//! and a recording that seeked by fetching another fragment would otherwise be
//! a second copy of all of it.
//!
//! Frigate answers either with fragmented MP4 and no `-re`: ffmpeg pours the
//! whole stretch out as fast as the network takes it, five to ten times faster
//! than real time. So this session is the only one in the pipeline that has to
//! time itself - everything else is paced by the camera.
//!
//! The clock is the presentation timestamp. A clip is cut where it was asked
//! for, so its first sample is that moment; a playlist's fragments count from
//! the playlist's start, so a sample's own moment is already measured from
//! there. Either way the session adds one offset and plays the distances
//! between the samples as they are.
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
//!
//! What a playlist costs is the seam between two fragments: the next one is
//! fetched once the one before has been played out, so a read of a hundred-odd
//! milliseconds sits between them. It is a fraction of a fragment and the clock
//! swallows it - the pictures after it play without waiting until they are back
//! on the pace they were recorded at.

use std::time::{Duration, Instant};

use bytes::Bytes;

use super::shared::{
    BaselineFix, Sinks, StatsWindow, announce_streaming, frigate_http, publish_frames, uses_hardware,
    video_decoder,
};
use super::ws_session::frigate_api_login;
use super::*;

use crate::discovery::frigate::VodPlaylist;

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

/// How far behind its clock a picture may be before the stretch in between is
/// dropped instead of played through.
///
/// The clock is the wall clock, so a read that took its time - a fragment the
/// box was still cutting, a link that stalled - leaves it ahead of the pictures.
/// Playing the backlog at full speed is a stretch of fast-forward nobody asked
/// for; past this much, the missed stretch is skipped and the picture joins the
/// clock where it is.
const MAX_LAG: Duration = Duration::from_millis(250);

/// How far before the moment asked for a decoder may start.
///
/// A recording asked for part-way in begins at the start of the fragment the
/// moment falls in, which can be twenty seconds early, and a decoder can only
/// start on a keyframe. Decoding from this far before the moment keeps the
/// stretch that is decoded and not shown down to the distance between two
/// keyframes rather than to a whole fragment. The keyframes measured on the box
/// of 2026-11-01 are two seconds apart.
const SEEK_HORIZON: i64 = 4;

/// Plays a recording on one channel.
///
/// Returns when the recording reaches its last frame - the response ends, or the
/// playlist's last fragment does: the caller turns that into
/// [`super::channel::SessionEnd::Finished`] rather than a reconnection.
pub(super) async fn run_recording_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    sinks: Sinks<'_>,
    audio: &AudioHub,
) -> Result<()> {
    let Sinks { events, frames, stats } = sinks;
    let Endpoint { index, camera_id, uri, stream, credentials, window, control, vod, begin, .. } =
        endpoint;
    let camera_id_str = camera_id.as_str();
    // A playback channel is always given the stretch it plays: it is what the
    // pictures it publishes are dated from, and what says whether the answer was
    // all of it.
    let Some((from, to)) = window else {
        return Err(crate::error::CoreError::config("a playback channel without a window"));
    };

    let mut source = match vod {
        Some(playlist) => Recording::vod(playlist, credentials.clone(), begin),
        None => Recording::clip(&uri, credentials.as_ref()).await?,
    };

    // The moment the picture is to begin at, when the recording was asked for
    // part-way in. Everything before it is read and not played: a recording
    // begins at a keyframe, and a seek into the middle of one would otherwise
    // show up to a fragment of the wrong moment before reaching the right one.
    let mut begin = begin.map(|at| at as f64);

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

    // The clock: a sample is due at `began` plus its distance from the moment the
    // picture started (`base_elapsed`). A pause moves `began` on by however long
    // it lasted, and a seek sets both to where the picture really began - so a
    // stretch that was read and not played is not time the recording owes.
    // `origin` is what `first` is of on the wall clock, which depends on where
    // the bytes came from - see [`Recording::origin`].
    let mut began = Instant::now();
    let mut base_elapsed: i64 = 0;
    let mut first: Option<i64> = None;
    let mut origin: Option<f64> = None;
    let mut held = control.as_ref().map(|control| control.subscribe());
    let mut jumps = control.as_ref().map(|control| control.subscribe_jumps());
    // The last jump acted on. A jump is taken as it is seen, and the count is
    // what tells one from the next.
    let mut jumped: u64 = 0;

    loop {
        // A jump the viewer asked for is taken before the next read: what is
        // open is the fragment being left, and its bytes are the stretch being
        // left with it. The decoder, the parser and the login are untouched.
        take_jump(&mut jumps, &mut jumped, &mut source, &mut begin, &mut pending);
        let chunk = match tokio::time::timeout(STALL_TIMEOUT, source.next_chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            // The end of the recording: the response ended, or the playlist had
            // no fragment left to fetch.
            Ok(Ok(None)) => break,
            Ok(Err(err)) => return Err(err),
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
            // A jump is taken between two samples rather than between two
            // reads: a read holds seconds of pictures, and the viewer's finger
            // is not going to wait for them. What is left of the chunk belongs
            // to the stretch being left.
            if take_jump(&mut jumps, &mut jumped, &mut source, &mut begin, &mut pending) {
                break;
            }
            let base = *first.get_or_insert(pts);
            let origin = *origin.get_or_insert_with(|| source.origin(from, base));
            // Signed, because a jump backwards moves the reading to a fragment
            // whose moments are behind the one the session opened on. Read as
            // unsigned, that distance became zero for every sample: the picture
            // kept the moment it had when the drag began, and the clock had
            // nothing left to wait for - so the session played the stretch
            // between the two at whatever speed the network allowed, and the
            // recording appeared to race back to where it had been.
            let elapsed = pts - base;
            // Where this sample belongs on the wall clock.
            let at = origin + elapsed as f64 / 1_000_000.0;
            // Too far before the moment asked for to be worth decoding: a
            // decoder that starts mid-run has nothing to predict from, so it
            // starts at the keyframe inside the horizon.
            if begin.is_some_and(|target| at + SEEK_HORIZON as f64 <= target) {
                continue;
            }
            // Held, the clock stops with the picture. The wait is the picture's
            // own pause, so nothing is decoded and nothing is published while
            // the viewer has a finger on the timeline.
            if let Some(paused) = held.as_mut() {
                if *paused.borrow() {
                    let since = Instant::now();
                    loop {
                        if !*paused.borrow() {
                            break;
                        }
                        if paused.changed().await.is_err() {
                            break;
                        }
                    }
                    // The clock stopped with the picture, so the wait is not
                    // time the recording owes.
                    began += since.elapsed();
                    // A finger lifted into a seek: taken here rather than at the
                    // top of the next sample, which would publish the frame the
                    // picture was stopped on first - the moment the drag began
                    // from, arriving after the place the viewer pointed at.
                    if take_jump(&mut jumps, &mut jumped, &mut source, &mut begin, &mut pending) {
                        break;
                    }
                }
            }
            // The moment asked for is where the picture and its clock begin, so
            // the stretch read on the way is not time the recording owes.
            if begin.is_some_and(|target| at >= target) {
                begin = None;
                began = Instant::now();
                base_elapsed = elapsed;
            }
            if begin.is_none() {
                let played = played_since(elapsed, base_elapsed);
                let due = began + played;
                let now = Instant::now();
                if now > due + MAX_LAG {
                    // Behind by more than a hiccup: the pictures in between are
                    // dropped rather than played at speed. Moving the clock on
                    // is what keeps a slow read from being repaid as a stretch
                    // of fast-forward.
                    began = now.checked_sub(played).unwrap_or(now);
                } else if due > now {
                    tokio::time::sleep((due - now).min(LONGEST_WAIT)).await;
                }
            }

            match step {
                Step::Audio(sample) => {
                    // Sound heard before the picture begins is the seek's own
                    // pre-roll being listened to.
                    if begin.is_none() && audio.wants(index) {
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
                    // The stretch decoded on the way to the moment asked for is
                    // not part of what this session played, and counting it
                    // would show a rate for a second that nobody saw.
                    if begin.is_none() {
                        window.counters.frames += 1;
                        total_frames += 1;
                    }
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
                                // Decoded on the way to the moment asked for and
                                // not shown: it is there for the pictures after
                                // it to be predicted from.
                                if begin.is_none() {
                                    if let Some((w, h)) =
                                        publish_frames(frames, index, &mut sequence, decoded, Some(at))
                                    {
                                        width = Some(w);
                                        height = Some(h);
                                    }
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

    // A moment asked for that nothing was ever played from is past everything
    // the recording held - a playlist whose last fragment ends before it. Said
    // as the empty answer it is, rather than reported as a recording played to
    // its end that nobody saw a frame of.
    if begin.is_some() {
        return Err(crate::error::CoreError::empty("nothing recorded at that moment"));
    }

    // The response ended. That is the recording's own end when the last picture
    // reached the window's last second - and a server that stopped sending
    // short of it looks the same from here. Said out loud rather than reported
    // as a clean finish, so a truncated answer is not mistaken for a short one.
    let reached = frames.latest(index).and_then(|frame| frame.at).unwrap_or(from as f64);
    if reached + 3.0 < to as f64 {
        tracing::warn!(
            target: "xgview::pipeline",
            index,
            reached = %reached,
            asked_for = to,
            "the recording stopped before the end of the window it was asked for"
        );
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

/// Where a recording's bytes come from.
///
/// A window is one request for one `clip.mp4`. A range is a playlist: an init
/// segment and then a fragment at a time, each fetched when the one before has
/// been read. Both hand the session a stream of bytes, and after that the
/// session cannot tell them apart - which is the point.
enum Recording {
    /// One request for the whole window.
    Clip { response: reqwest::Response },
    /// A playlist, read a fragment at a time.
    ///
    /// The playlist is kept whole rather than turned into a list of addresses
    /// when the reading starts: a jump moves to another fragment, and the one it
    /// moves to is worked out from the playlist every time.
    Vod {
        playlist: VodPlaylist,
        /// Where the init segment is, resolved once. Every fragment is read
        /// through it, and it is fetched once per session.
        init: Option<String>,
        /// Whether the init segment has been read, or there was none.
        init_read: bool,
        /// The fragment being read. It is the one the session began at, and the
        /// one a jump moves.
        next: usize,
        credentials: Option<(String, String)>,
        response: Option<reqwest::Response>,
        /// The login this playlist is being read with, once one has been asked
        /// for. One box, one token: the fragments after the first are asked for
        /// with it rather than logging in again.
        token: Option<String>,
    },
}

impl Recording {
    /// Opens the one request a window is.
    async fn clip(uri: &str, credentials: Option<&(String, String)>) -> Result<Self> {
        let (response, _) = open_request(uri, credentials, None).await?;
        Ok(Self::Clip { response })
    }

    /// A playlist, whose first request is made when its first bytes are asked
    /// for. A moment asked for part-way in begins at the fragment it falls in:
    /// the ones before it are a stretch nothing is going to play.
    fn vod(
        playlist: VodPlaylist,
        credentials: Option<(String, String)>,
        begin: Option<i64>,
    ) -> Self {
        let next = begin.map_or(0, |at| fragment_at(&playlist, at));
        let init = playlist.init_uri();
        Self::Vod {
            playlist,
            init,
            init_read: false,
            next,
            credentials,
            response: None,
            token: None,
        }
    }

    /// Moves the reading to the fragment a moment falls in, and says whether it
    /// could.
    ///
    /// What is open is dropped - its bytes are the stretch the jump is leaving -
    /// and nothing else changes: the init segment has either been read already
    /// or is still to be, the decoder is reading through it either way, and the
    /// login is the one the playlist has been asked with. A window cannot do
    /// this: it is one stream from one start, and the way to play another moment
    /// of it is to ask for another window.
    fn jump_to(&mut self, at: i64) -> bool {
        match self {
            Self::Clip { .. } => false,
            Self::Vod { playlist, next, response, .. } => {
                *next = fragment_at(playlist, at);
                *response = None;
                true
            }
        }
    }

    /// The next bytes of the recording, or `None` when it has all been read.
    ///
    /// The end of a fragment is not the end of the recording: the next one is
    /// the recording going on, and it is fetched here so that the session above
    /// sees one stream from first picture to last.
    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        match self {
            Self::Clip { response } => read(response).await,
            Self::Vod { playlist, init, init_read, next, credentials, response, token } => loop {
                if response.is_none() {
                    // The init segment first, then the fragment being read. The
                    // cursor moves as the request is made: what has been asked
                    // for is not asked for again.
                    let uri = match init {
                        Some(uri) if !*init_read => {
                            *init_read = true;
                            uri.clone()
                        }
                        _ => {
                            let Some(fragment) = playlist.fragments.get(*next) else {
                                return Ok(None);
                            };
                            let uri = playlist.fragment_uri(fragment);
                            *next += 1;
                            uri
                        }
                    };
                    // The login is kept for the fragments after this one: a
                    // playlist is a request every ten seconds, and a login for
                    // each of them would be three round trips where one will do.
                    let (body, fresh) = open_request(&uri, credentials.as_ref(), token.as_deref()).await?;
                    *response = Some(body);
                    *token = fresh;
                }
                let body = response.as_mut().expect("opened above");
                match read(body).await? {
                    Some(chunk) => return Ok(Some(chunk)),
                    // This request is read; the next one is the recording going
                    // on.
                    None => *response = None,
                }
            },
        }
    }

    /// Where the recording's own clock starts. See [`origin_of`].
    fn origin(&self, from: i64, first_pts_us: i64) -> f64 {
        match self {
            Self::Clip { .. } => origin_of(None, from, first_pts_us),
            Self::Vod { playlist, .. } => origin_of(Some(playlist.start), from, first_pts_us),
        }
    }
}

/// The fragment a moment falls in: the last one that begins at or before it.
///
/// The fragments are listed in the order they play and their lengths are what
/// says where each begins, which is the same arithmetic [`VodPlaylist::starts_at`]
/// does - walked once, because a range can be an hour and a seek is a drag.
fn fragment_at(playlist: &VodPlaylist, at: i64) -> usize {
    let mut start = playlist.start as f64;
    let mut chosen = 0;
    for (index, fragment) in playlist.fragments.iter().enumerate() {
        if start > at as f64 {
            break;
        }
        chosen = index;
        start += fragment.duration;
    }
    chosen
}

/// Takes a jump the viewer asked for, and says whether there was one.
///
/// The reading moves to the fragment the moment falls in and the picture begins
/// there; the caller drops whatever is left of the chunk it was playing, because
/// those samples are the stretch being left.
///
/// A jump asked for before this session was listening is taken too, and so is
/// one left over from a session the channel has reopened: both are the viewer
/// saying where they want to be, which is the same thing a fresh jump says.
fn take_jump(
    jumps: &mut Option<tokio::sync::watch::Receiver<Option<Jump>>>,
    jumped: &mut u64,
    source: &mut Recording,
    begin: &mut Option<f64>,
    pending: &mut Vec<u8>,
) -> bool {
    let Some(receiver) = jumps.as_mut() else {
        return false;
    };
    // Read every time rather than asking whether it changed: a jump asked for
    // before the session took its handle is not a change to that handle, and it
    // is exactly the seek a viewer makes the instant playback opens.
    let Some(jump) = *receiver.borrow_and_update() else {
        return false;
    };
    if jump.asked == *jumped {
        return false;
    }
    *jumped = jump.asked;
    if !source.jump_to(jump.at) {
        return false;
    }
    pending.clear();
    *begin = Some(jump.at as f64);
    tracing::debug!(
        target: "xgview::pipeline",
        at = jump.at,
        "the recording jumped"
    );
    true
}

/// Where a recording's own clock starts, in seconds since the epoch.
///
/// A window is cut where it was asked for, so its first sample is that moment. A
/// playlist's fragments count from the playlist's start, so the first sample's
/// own moment is already measured from there - rebasing it to zero, the way a
/// window's is, would add the offset twice.
fn origin_of(start: Option<i64>, from: i64, first_pts_us: i64) -> f64 {
    match start {
        None => from as f64,
        Some(start) => start as f64 + first_pts_us as f64 / 1_000_000.0,
    }
}

/// How far into the picture a sample is, measured from where playback began.
///
/// Both moments are distances from the session's first sample, and both are
/// signed: a jump backwards moves the reading to a fragment whose moments are
/// behind that first one, so the distance is negative. What is played is never
/// behind the moment playback began at, and the answer is floored at zero.
fn played_since(elapsed: i64, base_elapsed: i64) -> Duration {
    Duration::from_micros(u64::try_from(elapsed - base_elapsed).unwrap_or(0))
}

/// One read from a response, with the one message a broken body deserves.
async fn read(response: &mut reqwest::Response) -> Result<Option<Bytes>> {
    match response.chunk().await {
        Ok(chunk) => Ok(chunk),
        Err(err) => Err(crate::error::CoreError::network(format!(
            "the recording stopped being sent: {err}"
        ))),
    }
}

/// Opens one request for one piece of a recording, and answers the login it was
/// read with.
///
/// Frigate's own port serves a self-signed certificate, so the client accepts
/// one - the same decision the MSE WebSocket makes, for the same reason, and the
/// same client: see [`frigate_http`].
///
/// The request goes out without a token first, and a `401` is what asks for a
/// login. Asking first would tie every recording to an account: Frigate's
/// internal port serves the same recordings without one, and a login it does
/// not need - refused, because the box has authentication switched off - is a
/// permanent failure, which would leave that box with no playback at all.
///
/// `token` is the login an earlier request already made, and the one answered is
/// the caller's to keep: a playlist is one request per fragment, and a login for
/// each of them would be three round trips where one will do.
async fn open_request(
    uri: &str,
    credentials: Option<&(String, String)>,
    token: Option<&str>,
) -> Result<(reqwest::Response, Option<String>)> {
    let http = frigate_http();

    let mut response = match token {
        Some(token) => http.get(uri).bearer_auth(token).send().await?,
        None => http.get(uri).send().await?,
    };
    let mut token = token.map(str::to_string);
    if let (reqwest::StatusCode::UNAUTHORIZED, Some((user, pass))) = (response.status(), credentials) {
        let base = api_base(uri)
            .ok_or_else(|| crate::error::CoreError::network("not an http address"))?;
        let fresh = frigate_api_login(base, user, pass, Duration::from_secs(10)).await?;
        response = http.get(uri).bearer_auth(&fresh).send().await?;
        token = Some(fresh);
    }

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(crate::error::CoreError::auth(
            "Frigate refused the account for this recording",
        ));
    }
    if status == reqwest::StatusCode::BAD_REQUEST {
        // The range is real and Frigate read it; there is simply nothing
        // recorded in it. Worth its own words: it is the one answer the viewer
        // can act on, by picking another time.
        return Err(crate::error::CoreError::empty("no recording in that range"));
    }
    // Any other refusal of this address is one the address will give again: the
    // window is what it is, and the same URL cannot start working. A `408` and a
    // `429` are the exceptions - the first says try again, the second says try
    // again later - and both are worth the backoff.
    if status.is_client_error()
        && status != reqwest::StatusCode::REQUEST_TIMEOUT
        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return Err(crate::error::CoreError::empty(format!(
            "the recording answered {status}"
        )));
    }
    if !status.is_success() {
        return Err(crate::error::CoreError::network(format!(
            "the recording answered {status}"
        )));
    }
    Ok((response, token))
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
    use crate::discovery::frigate::Fragment;

    /// A box, as the framing sees it.
    fn boxed(kind: &[u8; 4], body: usize) -> Vec<u8> {
        let size = 8 + body;
        let mut out = (size as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend(std::iter::repeat(0u8).take(body));
        out
    }

    /// A playlist over 100..150 in three fragments, with an init segment.
    fn three_fragments() -> VodPlaylist {
        VodPlaylist {
            base: "https://box/vod/cam/start/100/end/200".to_string(),
            start: 100,
            init: Some("init.mp4".to_string()),
            fragments: vec![
                Fragment { name: "seg-1.m4s".into(), duration: 20.0 },
                Fragment { name: "seg-2.m4s".into(), duration: 10.0 },
                Fragment { name: "seg-3.m4s".into(), duration: 20.0 },
            ],
        }
    }

    /// A playlist is read from its init segment and its first fragment, and the
    /// clock it is dated from is the playlist's own start plus a sample's own
    /// moment.
    #[test]
    fn a_playlist_begins_at_its_init_segment_and_its_first_fragment() {
        let source = Recording::vod(three_fragments(), None, None);
        let Recording::Vod { playlist, init, init_read, next, .. } = &source else {
            panic!("a playlist is a playlist");
        };
        assert_eq!(playlist.start, 100);
        assert_eq!(
            init.as_deref(),
            Some("https://box/vod/cam/start/100/end/200/init.mp4"),
            "resolved once, and fetched before anything else"
        );
        assert!(!init_read);
        assert_eq!(*next, 0, "the first fragment");
    }

    /// A jump backwards lands on a fragment whose moments are behind the one
    /// the session opened on. Everything that is played after it is measured
    /// from the moment it landed on, not from the session's first sample - and
    /// the stretch before that moment owes nothing, or it would be played at
    /// whatever speed the network allowed.
    #[test]
    fn a_jump_backwards_is_played_from_the_moment_it_landed_on() {
        // The session opened 535 s into the playlist and the jump took it back
        // to a fragment at 100 s.
        let base = 535_000_000i64;
        let landed = 100_000_000i64;
        assert_eq!(played_since(landed, landed), Duration::ZERO, "the moment it landed on");
        assert_eq!(played_since(landed + 40_000, landed), Duration::from_millis(40));
        assert_eq!(played_since(landed + 1_000_000, landed), Duration::from_secs(1));
        // And a sample still behind where playback began owes nothing.
        assert_eq!(played_since(landed - 5_000_000, landed), Duration::ZERO);
        // What the distance from the session's first sample is, for comparison:
        // negative, which is what the clock used to read as zero.
        assert!(landed - base < 0);
    }

    /// A moment part-way into a range begins at the fragment it falls in, and
    /// the ones before it are not even asked for.
    #[test]
    fn a_moment_part_way_in_skips_the_fragments_before_it() {
        let playlist = three_fragments();
        // The fragments run 100..120, 120..130 and 130..150.
        assert_eq!(fragment_at(&playlist, 100), 0, "the first moment it holds");
        assert_eq!(fragment_at(&playlist, 119), 0);
        assert_eq!(fragment_at(&playlist, 120), 1, "a fragment's own start");
        assert_eq!(fragment_at(&playlist, 129), 1);
        assert_eq!(fragment_at(&playlist, 130), 2);
        assert_eq!(fragment_at(&playlist, 900), 2, "past the end is the last one");
        assert_eq!(fragment_at(&playlist, 50), 0, "before the range is the first");

        let source = Recording::vod(playlist, None, Some(125));
        let Recording::Vod { playlist, next, .. } = &source else {
            panic!("a playlist is a playlist");
        };
        assert_eq!(playlist.start, 100, "the range's own start, not the fragment's");
        assert_eq!(*next, 1, "the fragment that holds 125");
    }

    /// A jump moves the reading to another fragment and never makes it skip the
    /// init segment: one that has not been read yet still is, and one that has is
    /// not fetched twice.
    #[test]
    fn a_jump_moves_the_reading_and_never_skips_the_init_segment() {
        let mut source = Recording::vod(three_fragments(), None, None);
        assert!(source.jump_to(135), "a playlist can jump");
        let Recording::Vod { next, init_read, .. } = &source else {
            panic!("a playlist is a playlist");
        };
        assert_eq!(*next, 2, "the fragment that holds 135");
        assert!(!*init_read, "a jump before the first read still fetches it");

        // And the reading the session has already done is not undone: the init
        // segment stays read, and only the fragment moves.
        let mut source = Recording::vod(three_fragments(), None, None);
        if let Recording::Vod { init_read, .. } = &mut source {
            *init_read = true;
        }
        assert!(source.jump_to(101));
        let Recording::Vod { next, init_read, .. } = &source else {
            panic!("a playlist is a playlist");
        };
        assert_eq!(*next, 0, "backwards is the same move");
        assert!(*init_read, "and the init segment is not fetched again");
    }

    /// A window's pictures are dated from the moment it was asked for; a
    /// playlist's from its own start plus the moment the sample carries, because
    /// its fragments count from there and rebasing would add the offset twice.
    #[test]
    fn a_recording_is_dated_from_where_its_bytes_count_from() {
        assert_eq!(origin_of(None, 100, 30_000_000), 100.0);
        assert_eq!(origin_of(Some(100), 100, 30_000_000), 130.0);
        // A fragment that begins where the playlist does is dated from it.
        assert_eq!(origin_of(Some(100), 100, 0), 100.0);
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
