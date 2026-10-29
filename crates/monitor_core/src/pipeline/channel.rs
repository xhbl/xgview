//! One channel's reconnection state machine, and the one attempt it runs.

use super::*;

use super::shared::{Sinks, describe, emit};
use super::{mjpeg_session::run_mjpeg_session, rtsp_session::run_session, ws_session::run_ws_session};

/// Runs one attempt on one endpoint, opening the session its address calls for.
///
/// The scheme is what decides: an `http://` address is an MJPEG stream, a
/// `ws://`/`wss://` one go2rtc's MSE, and anything else an RTSP session.
async fn run_once(
    endpoint: &Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    sinks: Sinks<'_>,
    audio: &AudioHub,
    talk: &TalkHub,
) -> Result<()> {
    if endpoint.is_mjpeg() {
        run_mjpeg_session(endpoint.clone(), handshake_timeout, sinks).await
    } else if endpoint.is_ws() {
        run_ws_session(endpoint.clone(), prefer_hardware, handshake_timeout, sinks, audio).await
    } else {
        run_session(endpoint.clone(), prefer_hardware, handshake_timeout, sinks, audio, talk).await
    }
}

/// Reconnection state machine of one channel.
pub(super) async fn run_channel(
    endpoint: Endpoint,
    prefer_hardware: bool,
    settings: Arc<Mutex<SessionSettings>>,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    stats: Arc<StatsStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
    wake: Arc<Notify>,
) {
    let index = endpoint.index;
    let mut attempt: u32 = 0;
    // Reason of the last failure, kept so that the countdown does not hide it:
    // a rejected password or an unreachable device is the only explanation the
    // user has, and the grid tile truncates the detail line.
    let mut failure = "connection attempt".to_string();
    // The stores this channel's sessions publish into. Built once: every attempt
    // writes to the same three, and the channel's own events go through
    // `events` directly.
    let sinks = Sinks { events: &events, frames: &frames, stats: &stats };

    loop {
        attempt += 1;
        // Read afresh every attempt, so a setting the user edits while this
        // channel is running is picked up at its next attempt rather than at
        // the next reopen: retry timing is not worth interrupting a healthy
        // stream for, and a handshake that keeps failing wants the new budget
        // now rather than after a restart.
        let settings = match settings.lock() {
            Ok(settings) => settings.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let state = if attempt == 1 {
            ConnectionState::Connecting
        } else {
            ConnectionState::Reconnecting
        };
        emit(
            &events,
            StreamEvent::State {
                index,
                camera_id: endpoint.camera_id.clone(),
                state,
                detail: format!("#{attempt} {failure}"),
            },
        );

        // Pictures already on screen when the attempt starts, so that an
        // attempt which delivered can be told from one that never did.
        let published = frames.published(index);

        // Each attempt opens its own session, so it is handed its own copy. The
        // sessions have nothing in common beyond the backoff around them, so the
        // endpoint picks the one it needs rather than a single session branching
        // on it statement by statement.
        let started = Instant::now();
        let mut outcome = run_once(&endpoint, prefer_hardware, settings.handshake_timeout, sinks, &audio, &talk).await;

        // `Auto`: RTSP is preferred, and an RTSP that could not be reached at
        // all is answered with the MSE address at once rather than with a
        // backoff - which is the whole point of keeping two addresses. Only when
        // the attempt put nothing on screen: a stream that worked and was then
        // cut keeps to RTSP, so a healthy channel does not flap between the two.
        if outcome.is_err() && endpoint.fallback.is_some() && frames.published(index) == published {
            let fallback = endpoint.fallback.clone().expect("checked above");
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                "rtsp was not reachable, trying the mse address"
            );
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    state,
                    detail: format!("#{attempt} rtsp unreachable, trying mse"),
                },
            );
            let mse = Endpoint { uri: fallback.uri, credentials: fallback.credentials, fallback: None, ..endpoint.clone() };
            outcome = run_once(&mse, prefer_hardware, settings.handshake_timeout, sinks, &audio, &talk).await;
        }

        // The attempt is over, so its measurements are too: whatever the next
        // one opens, it has its own codec, resolution and rate, and a renderer
        // reading the numbers of the session that just ended would show them as
        // if they still described the channel.
        stats.clear(index);

        let lived = started.elapsed();
        // What the attempt failed with: as text, which is what the viewer reads
        // on the tile, and as a kind, which is what decides whether waiting can
        // change the answer. `None` while the attempt ended cleanly.
        let mut kind = None;
        match outcome {
            Ok(()) => failure = "stream closed by peer".to_string(),
            Err(err) => {
                // A failing session is the only clue the user gets about a
                // rejected password or an unreachable device, so it is
                // reported at `warn` rather than hidden behind `debug`. The
                // causes are walked too: the outermost layer of an HTTP error
                // names only the stage, not what the peer or the transport did.
                tracing::warn!(
                    target: "xgview::pipeline",
                    index,
                    attempt,
                    lived = ?lived,
                    err = %describe(&err),
                    "session failed"
                );
                kind = Some(err.kind());
                failure = err.to_string();
            }
        }

        // A session that put pictures on screen is a stream that worked and was
        // then cut, not one that never came up, so the next attempt is worth the
        // shortest delay again. Without this the counter is only ever raised, and
        // a channel that keeps being cut - an MJPEG stream its server resets, say
        // - ends up waiting the ceiling delay between two pictures long after a
        // single quick retry would have brought it back.
        if frames.published(index) > published {
            attempt = 0;
        }

        // A rejected password is not a slow network. The countdown below exists
        // because the next attempt might meet something different; where it
        // cannot, waiting only delays the one line that says what is wrong, and
        // the attempt that followed would reopen the same session with the same
        // credentials and be refused the same way.
        //
        // The channel is left failed until something reopens it: an edit to the
        // camera, which `Endpoint::is_same_stream` sees as a different stream,
        // or the scheduler parking the channel and putting it back - a page
        // away and back is enough, because `Suspend` drops the runtime and
        // `Activate` then starts a fresh attempt.
        if kind.is_some_and(FailureKind::is_permanent) {
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    state: ConnectionState::Failed,
                    detail: failure,
                },
            );
            return;
        }

        // The snapshot taken at the top of the attempt is what this reads: one
        // policy for the whole attempt, and the newest one the user set.
        if !settings.reconnect.should_retry(attempt) {
            // Nothing will publish again on this channel, so the sample of the
            // attempt that has just ended is dropped here rather than left for
            // a renderer to read as if the channel were still running.
            stats.clear(index);
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    state: ConnectionState::Failed,
                    detail: format!("{failure} (gave up after {attempt} attempts)"),
                },
            );
            return;
        }

        let first_frame = frames.published(index) == 0;
        let delay = settings.reconnect.delay_for(attempt, first_frame, index as u32);
        emit(
            &events,
            StreamEvent::State {
                index,
                camera_id: endpoint.camera_id.clone(),
                state: ConnectionState::Reconnecting,
                detail: format!("{failure} | retry in {} ms", delay.as_millis()),
            },
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = wake.notified() => {
                // The window came back: the attempts counted while it was in
                // the background are not a reason to keep the viewer waiting.
                attempt = 0;
                tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    "woken for a resume, retrying now"
                );
            }
        }
    }
}
