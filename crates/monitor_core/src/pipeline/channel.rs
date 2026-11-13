//! One channel's reconnection state machine, and the one attempt it runs.

use super::*;

use super::recording_session::run_recording_session;
use super::shared::{Sinks, describe, emit};
use super::{mjpeg_session::run_mjpeg_session, rtsp_session::run_session, ws_session::run_ws_session};

/// How one attempt on one endpoint ended.
///
/// The three are not the same thing to the loop around them: a live stream that
/// closes is a stream to reopen, a broken one is a stream to reopen after a
/// wait, and a recording that reached its last frame is neither - asking for it
/// again would replay it from the start, for ever.
enum SessionEnd {
    /// Played to its end. Nothing is retried.
    Finished,
    /// The peer closed a live stream. Reopened, as it always has been.
    Closed,
    /// The session broke, with the reason the viewer is shown.
    Failed(crate::error::CoreError),
}

/// Runs one attempt on one endpoint, opening the session its kind calls for.
///
/// A playback channel is HTTP requests for one stretch of a recording - one, or
/// one per fragment of a playlist - and the recording's end is an answer rather
/// than a failure. Everything else opens by scheme: an `http://` address is an
/// MJPEG stream, a `ws://`/`wss://` one go2rtc's MSE, and anything else an RTSP
/// session.
async fn run_once(
    endpoint: &Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    sinks: Sinks<'_>,
    audio: &AudioHub,
    talk: &TalkHub,
) -> SessionEnd {
    if endpoint.is_recording() {
        return match run_recording_session(endpoint.clone(), prefer_hardware, sinks, audio).await {
            Ok(()) => SessionEnd::Finished,
            Err(err) => SessionEnd::Failed(err),
        };
    }
    let outcome = if endpoint.is_mjpeg() {
        run_mjpeg_session(endpoint.clone(), handshake_timeout, sinks).await
    } else if endpoint.is_ws() {
        run_ws_session(endpoint.clone(), prefer_hardware, handshake_timeout, sinks, audio).await
    } else {
        run_session(endpoint.clone(), prefer_hardware, handshake_timeout, sinks, audio, talk).await
    };
    match outcome {
        Ok(()) => SessionEnd::Closed,
        Err(err) => SessionEnd::Failed(err),
    }
}

/// Where a broken recording is picked up from, if it is picked up at all.
///
/// A recording resumes where its last picture was, which is what stops a hiccup
/// costing the whole range: ten minutes of fragments, replayed from the first.
/// A live stream is never resumed - its picture is of now, and there is no
/// stretch to replay.
///
/// `published` is whether the attempt that just ended put anything on screen.
/// One that did not leaves the frame of the recording before this one, which
/// says nothing about where this one had got to.
fn resume_at(recording: bool, published: bool, frame: Option<f64>) -> Option<i64> {
    if !recording || !published {
        return None;
    }
    frame.map(|at| at as i64)
}

/// Reconnection state machine of one channel.
pub(super) async fn run_channel(
    mut endpoint: Endpoint,
    prefer_hardware: bool,
    settings: Arc<Mutex<SessionSettings>>,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    stats: Arc<StatsStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    // Set beside `done` when the ending was "no attempts left" rather than
    // "nothing left to try" - the one a resume gives another chance to.
    gave_up: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    let index = endpoint.index;
    // Says when this task stops existing, whether it returned or was aborted.
    // A task that outlives its channel is what two publishers on one channel
    // looked like: the store drops what it publishes now, but a task that never
    // ends is still a session that never stopped, and this is how that is seen.
    struct TaskEnd(usize, &'static str);
    impl Drop for TaskEnd {
        fn drop(&mut self) {
            tracing::debug!(
                target: "xgview::pipeline",
                index = self.0,
                reason = self.1,
                "channel task ended"
            );
        }
    }
    let mut end = TaskEnd(index, "aborted");
    let mut attempt: u32 = 0;
    // Reason of the last failure, kept so that the countdown does not hide it:
    // a rejected password or an unreachable device is the only explanation the
    // user has, and the grid tile truncates the detail line.
    let mut failure = "connection attempt".to_string();
    // The stores this channel's sessions publish into. Built once: every attempt
    // writes to the same three, and the channel's own events go through
    // `events` directly.
    let sinks = Sinks { events: &events, frames: &frames, stats: &stats, stop: &stop };

    loop {
        // The channel has been given to another session, or parked. Nothing
        // below is worth doing any more: see `ChannelRuntime::stop`.
        if stop.load(Ordering::Relaxed) {
            end.1 = "the channel was given to another session";
            return;
        }
        attempt += 1;
        // Every attempt says so: a channel that has stopped retrying and one
        // that is retrying into a server that will not answer look the same from
        // the tile, and only one of them is a fault here.
        tracing::debug!(
            target: "xgview::pipeline",
            index,
            attempt,
            camera = %endpoint.camera_id,
            "an attempt begins"
        );
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
        if matches!(outcome, SessionEnd::Failed(_))
            && endpoint.fallback.is_some()
            && frames.published(index) == published
        {
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

        // A recording that broke part-way through is picked up where the picture
        // had got to. Its range is ten minutes of fragments, and reopening the
        // endpoint as it was asked for would replay them from the first - so a
        // hiccup cost ten minutes of rewind, and the viewer watched the same
        // stretch again.
        //
        // Only when this attempt put something on screen: one that failed before
        // its first frame leaves the frame of the recording before this one,
        // which says nothing about where this one had got to.
        // Only a broken attempt is picked up: one that reached the end of the
        // recording is the recording's own end, and the channel stops there.
        let resumed = if matches!(outcome, SessionEnd::Failed(_)) {
            resume_at(
                endpoint.is_recording(),
                frames.published(index) > published,
                // The moment, not the frame's moment: a recording that played
                // into a surface has no frame to read one from, and resuming
                // without it replays the range from its first fragment.
                frames.moment(index),
            )
        } else {
            None
        };
        if let Some(at) = resumed {
            // A playlist is told by a jump: that is the session's own way of
            // being told where to play from, and it counts, so this one is newer
            // than any jump the viewer asked for before the failure.
            if let Some(control) = endpoint.control.as_ref() {
                control.jump(at);
            }
            // A window has no fragments to move between, so it is told where to
            // begin instead: the session reads the window and skips to it.
            endpoint.begin = Some(at);
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                at,
                "a recording broke; picking it up where the picture had got to"
            );
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
            // The recording reached its last frame. The channel stops here:
            // reopening the same address would replay the same stretch from
            // the beginning, and the viewer would see it loop for ever.
            SessionEnd::Finished => {
                emit(
                    &events,
                    StreamEvent::State {
                        index,
                        camera_id: endpoint.camera_id.clone(),
                        state: ConnectionState::Finished,
                        detail: String::new(),
                    },
                );
                tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    "the recording reached its end"
                );
                done.store(true, Ordering::Relaxed);
                end.1 = "the recording reached its end";
                return;
            }
            SessionEnd::Closed => failure = "stream closed by peer".to_string(),
            SessionEnd::Failed(err) => {
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
        // The box answered, and what it answered was "there is nothing at that
        // moment". A recording that has nothing to give at a moment is not a
        // stream that failed - it is a hole, and the interface carries the
        // picture on at the next stretch (see `recording_stopped`). Reported as
        // a finish rather than as a failure, so a hole is not drawn as a broken
        // camera, and so a seek that lands in one does not have to be undone.
        if kind == Some(FailureKind::Empty) {
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    state: ConnectionState::Finished,
                    detail: failure,
                },
            );
            // A hole is the end of this attempt, not a task that died: without
            // this the supervisor would take the channel for one that ended on
            // its own and start it again every couple of seconds, asking the
            // same server for the same empty moment for ever. See
            // `Supervisor::restart_finished`.
            done.store(true, Ordering::Relaxed);
            return;
        }

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
            done.store(true, Ordering::Relaxed);
            end.1 = "the failure is one waiting cannot fix";
            return;
        }

        // The snapshot taken at the top of the attempt is what this reads: one
        // policy for the whole attempt, and the newest one the user set.
        if !settings.reconnect.should_retry(attempt) {
            // Nothing will publish again on this channel, so the sample of the
            // attempt that has just ended is dropped here rather than left for
            // a renderer to read as if the channel were still running.
            stats.clear(index);
            // Giving up is an ending too. Said only in the event, it was still
            // a task that had ended without being told to - so the supervisor
            // started it again with the count back at zero, and the limit the
            // user set never held. See `Supervisor::restart_finished`.
            done.store(true, Ordering::Relaxed);
            // This is the ending a resume may undo: the attempts went into a
            // network that was gone, not into a camera that refused. See
            // `Supervisor::wake`.
            gave_up.store(true, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording is picked up where its last picture was, and a live stream is
    /// not picked up at all: its picture is of now, and there is no stretch to
    /// replay. An attempt that put nothing on screen has no frame of its own to
    /// read - the one it would find belongs to the recording before it.
    #[test]
    fn a_broken_recording_is_picked_up_where_it_stopped() {
        let at = Some(1_793_642_208.4);
        assert_eq!(resume_at(true, true, at), Some(1_793_642_208));
        assert_eq!(resume_at(true, true, None), None, "a frame with no moment");
        assert_eq!(resume_at(true, false, at), None, "the frame is the last recording's");
        assert_eq!(resume_at(false, true, at), None, "a live stream has no stretch");
    }
}
