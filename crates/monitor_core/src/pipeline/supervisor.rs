//! The task that owns the channels: which stream each is pulling, which one may
//! be talked to, and the call in progress.

use super::*;

use super::channel::run_channel;
use super::rtsp_session::talk_worker;

/// What the supervisor knows about how to reach one camera's speaker.
#[derive(Default)]
pub(super) struct TalkEntry {
    pub(super) route: TalkRoute,
    /// The mode the answer was given for. A camera whose `talk` setting changed
    /// has a different answer waiting, and the cached one is not it.
    mode: TalkMode,
    /// When the answer was given; `None` until one is.
    pub(super) answered: Option<Instant>,
    /// A probe is in flight for this address.
    probing: bool,
    /// This camera would not take a call on a connection of its own, and the
    /// media has to ride on the picture's session instead. Kept for the run:
    /// asking again costs a reopen of that session every time.
    refuses_call: bool,
}

impl TalkEntry {
    /// Whether the answer is still worth answering with.
    ///
    /// A "no" is asked again after a while - a firmware or an account can
    /// change it - while a "yes" is kept: it is the answer the microphone is
    /// drawn on, and a camera that offered a back channel a minute ago still
    /// does.
    pub(super) fn fresh(&self) -> bool {
        let Some(at) = self.answered else { return false };
        match self.route {
            TalkRoute::Standard | TalkRoute::Vendor(_) => true,
            TalkRoute::None => at.elapsed() < UNSUPPORTED_TTL,
            TalkRoute::Unknown => false,
        }
    }
}

pub(super) async fn supervisor(
    mut commands: UnboundedReceiver<ChannelCommand>,
    command_tx: UnboundedSender<ChannelCommand>,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    stats: Arc<StatsStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
    settings: Arc<Mutex<SessionSettings>>,
    mut prefer_hardware: bool,
    handle: Handle,
) {
    let mut channels: HashMap<usize, ChannelRuntime> = HashMap::new();
    // The channel the viewer may talk to. Kept beside the channels because it
    // outlives the sessions it opens: a channel reopened for another reason
    // gets the back channel back if it is still the one magnified.
    let mut talk_target: Option<usize> = None;
    // What is known about each camera's back channel, keyed by the address a
    // call would be placed to. A camera sitting in two slots is one entry, and
    // a channel that moves between streams asks about the stream it is on.
    let mut talk_support: HashMap<String, TalkEntry> = HashMap::new();
    // The call in progress, if any. One at a time: the hub has a single target,
    // and two workers would send the same microphone twice.
    let mut call: Option<JoinHandle<()>> = None;

    while let Some(command) = commands.recv().await {
        match command {
            ChannelCommand::Activate { index, source, stream, previous_stream } => {
                // A camera whose sub stream is neither configured nor inferable
                // is served by its main stream: report the stream that is really
                // pulled instead of labelling a 1080p picture as `SUB`.
                let stream = match stream {
                    StreamKind::Sub if source.configured_uri(StreamKind::Sub).is_none() => {
                        StreamKind::Main
                    }
                    kind => kind,
                };
                let endpoint = endpoint_for(&source, stream, index);
                if let Some(existing) = channels.get(&index) {
                    if existing.endpoint.is_same_stream(&endpoint) {
                        continue;
                    }
                }
                if let Some(existing) = channels.remove(&index) {
                    existing.task.abort();
                    // The sample belongs to the session being replaced, and it
                    // is dropped before the transition is announced: read after
                    // it, the numbers of the stream that is going away would
                    // overwrite the state the switch just set - including the
                    // held frame it is meant to keep on screen.
                    stats.clear(index);
                }
                if previous_stream.is_some() {
                    let _ = events.send(StreamEvent::Transition {
                        index,
                        camera_id: endpoint.camera_id.clone(),
                        from: previous_stream,
                        to: stream,
                    });
                }
                tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    camera = %source.masked_uri(stream),
                    stream = stream.as_str(),
                    "starting channel"
                );
                let wake = Arc::new(Notify::new());
                let task = handle.spawn(run_channel(
                    endpoint.clone(),
                    prefer_hardware,
                    settings.clone(),
                    events.clone(),
                    frames.clone(),
                    stats.clone(),
                    audio.clone(),
                    talk.clone(),
                    wake.clone(),
                ));
                channels.insert(index, ChannelRuntime { endpoint, task, wake });
            }
            ChannelCommand::Suspend { index } => {
                if let Some(existing) = channels.remove(&index) {
                    existing.task.abort();
                    // The numbers belonged to the session that was just stopped.
                    stats.clear(index);
                    tracing::debug!(target: "xgview::pipeline", index, "channel suspended");
                }
            }
            ChannelCommand::Wake => {
                // Only the channels waiting out a backoff care. `notify_waiters`
                // leaves no permit behind, so one that is streaming is untouched
                // and one that is connecting finishes its attempt.
                for runtime in channels.values() {
                    runtime.wake.notify_waiters();
                }
                tracing::debug!(
                    target: "xgview::pipeline",
                    channels = channels.len(),
                    "woken for a resume"
                );
            }
            ChannelCommand::PreferHardware(prefer) => {
                if prefer == prefer_hardware {
                    continue;
                }
                prefer_hardware = prefer;
                tracing::debug!(
                    target: "xgview::pipeline",
                    hardware = prefer,
                    channels = channels.len(),
                    "decoding preference changed, reopening the channels"
                );
                // The decoder is chosen when a session opens, so the live
                // channels have to be reopened for the new preference to reach
                // them. They are started again from here because the scheduler
                // has no reason to see a change: nothing about the cameras did.
                for (index, runtime) in channels.drain().collect::<Vec<_>>() {
                    runtime.task.abort();
                    // Reopened, not continued: the sample described the decoder
                    // this channel is leaving behind, and `hardware` in it would
                    // be the answer that has just been changed.
                    stats.clear(index);
                    let endpoint = runtime.endpoint;
                    let wake = runtime.wake;
                    let task = handle.spawn(run_channel(
                        endpoint.clone(),
                        prefer_hardware,
                        settings.clone(),
                        events.clone(),
                        frames.clone(),
                        stats.clone(),
                        audio.clone(),
                        talk.clone(),
                        wake.clone(),
                    ));
                    channels.insert(index, ChannelRuntime { endpoint, task, wake });
                }
            }
            ChannelCommand::Talk(target) => {
                if talk_target == target {
                    continue;
                }
                talk_target = target;
                // The hub the captured audio arrives on has to name the same
                // channel, or the worker below keeps writing silence: the mic
                // samples are pushed into the hub, but `TalkHub::talking_on`
                // only answers for its target, and a target nobody sets is
                // `None` for the life of the process. See `TalkHub::set_target`.
                //
                // Nothing about a session changes here, which is the point of
                // the split: the picture never carried a back channel, so a
                // change of viewport no longer reopens the channel it left or
                // the one it arrived at. The call, when there is one, is on its
                // own connection and ends by itself - `talking_on` goes false
                // for the channel it belonged to.
                talk.set_target(target);
                debug_assert_eq!(
                    talk.target(),
                    target,
                    "the hub has to follow the session's talk target"
                );
                tracing::debug!(
                    target: "xgview::pipeline",
                    camera = ?target,
                    "the talk target moved"
                );
            }
            ChannelCommand::ProbeTalk { index } => {
                let Some(runtime) = channels.get(&index) else { continue };
                let endpoint = runtime.endpoint.clone();
                let uri = endpoint.uri.clone();
                let answer = |route| StreamEvent::Talk {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    uri: uri.clone(),
                    route,
                };
                // MJPEG and the MSE fallback carry no media the client sends,
                // so there is nothing to ask: the answer is known without a
                // connection.
                if endpoint.is_mjpeg() || endpoint.is_ws() {
                    let _ = events.send(answer(TalkRoute::None));
                    continue;
                }
                let entry = talk_support.entry(uri.clone()).or_default();
                if entry.fresh() && entry.mode == endpoint.talk_mode {
                    let _ = events.send(answer(entry.route));
                    continue;
                }
                // One probe per address at a time: the second caller would only
                // ask the same camera the same question.
                if entry.probing {
                    continue;
                }
                entry.probing = true;
                let timeout = {
                    let settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    settings.handshake_timeout
                };
                let credentials = endpoint.credentials.clone();
                let camera_id = endpoint.camera_id.clone();
                let commands = command_tx.clone();
                handle.spawn(async move {
                    // Late enough that the picture's own session - reopened for
                    // the other stream at the same moment - is not still
                    // connecting when this one arrives.
                    tokio::time::sleep(PROBE_DELAY).await;
                    let ask = async {
                        let mut client = RtspClient::connect_with_auth(&uri, credentials).await?;
                        client.probe_backchannel().await
                    };
                    let route = match tokio::time::timeout(timeout, ask).await {
                        Ok(Ok(probe)) => {
                            // Whether the standard back channel carries
                            // something this build can send; the vendor route
                            // does not care, which is the point of asking the
                            // maker before the media.
                            let encodable = probe
                                .encoding
                                .as_deref()
                                .map(|encoding| {
                                    TalkStream::new(AudioCodec::from_encoding(encoding)).is_some()
                                })
                                .unwrap_or(false);
                            let route =
                                route_from(endpoint.talk_mode, probe.realm.as_deref(), encodable);
                            tracing::debug!(
                                target: "xgview::talk",
                                camera = %camera_id,
                                realm = probe.realm.as_deref().unwrap_or("-"),
                                encoding = probe.encoding.as_deref().unwrap_or("-"),
                                ?route,
                                "the camera answered the talk probe"
                            );
                            route
                        }
                        // A failure is not an answer: nothing is cached on it,
                        // so the next magnification asks again.
                        Ok(Err(err)) => {
                            tracing::debug!(
                                target: "xgview::talk",
                                camera = %camera_id,
                                error = %err,
                                "the talk-back probe failed"
                            );
                            TalkRoute::Unknown
                        }
                        Err(_) => {
                            tracing::debug!(
                                target: "xgview::talk",
                                camera = %camera_id,
                                timeout_s = timeout.as_secs(),
                                "the talk-back probe timed out"
                            );
                            TalkRoute::Unknown
                        }
                    };
                    let _ = commands.send(ChannelCommand::TalkProbed {
                        uri,
                        route,
                        mode: endpoint.talk_mode,
                    });
                });
            }
            ChannelCommand::TalkProbed { uri, route, mode } => {
                let entry = talk_support.entry(uri.clone()).or_default();
                entry.probing = false;
                if route != TalkRoute::Unknown {
                    entry.route = route;
                    entry.mode = mode;
                    entry.answered = Some(Instant::now());
                }
                // The answer is the camera's, so every channel showing that
                // address hears it - not only the one that asked.
                for (index, runtime) in &channels {
                    if runtime.endpoint.uri == uri {
                        let _ = events.send(StreamEvent::Talk {
                            index: *index,
                            camera_id: runtime.endpoint.camera_id.clone(),
                            uri: uri.clone(),
                            route: entry.route,
                        });
                    }
                }
            }
            ChannelCommand::Call { index } => {
                let Some(runtime) = channels.get(&index) else {
                    // The channel went away between the press and this command.
                    // Saying so beats a microphone that does nothing.
                    let _ = events.send(StreamEvent::CallFailed {
                        index,
                        camera_id: String::new(),
                        reason: CallFailure::Unreachable,
                    });
                    continue;
                };
                let endpoint = runtime.endpoint.clone();
                // MJPEG and the MSE fallback carry no media the client sends.
                // The probe already answers `Unsupported` for them, so this is
                // only the case where the press beat the answer.
                if endpoint.is_mjpeg() || endpoint.is_ws() {
                    let _ = events.send(StreamEvent::CallFailed {
                        index,
                        camera_id: endpoint.camera_id.clone(),
                        reason: CallFailure::NoBackChannel,
                    });
                    continue;
                }
                // This camera has already said it will not take a call of its
                // own. Going straight to the session that can carry the media
                // spares the viewer a wait for a refusal that is already known.
                if talk_support.get(&endpoint.uri).is_some_and(|entry| entry.refuses_call) {
                    let _ = command_tx.send(ChannelCommand::TalkFallback { index });
                    continue;
                }
                if let Some(previous) = call.take() {
                    previous.abort();
                }
                let timeout = {
                    let settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    settings.handshake_timeout
                };
                // How this camera is reached, as its probe answered. A press
                // that beat the answer falls back to the standard route, which
                // is what every camera was before there was a choice.
                let route = talk_support
                    .get(&runtime.endpoint.uri)
                    .map(|entry| entry.route)
                    .filter(|route| route.reachable())
                    .unwrap_or(TalkRoute::Standard);
                call = Some(handle.spawn(talk_worker(
                    index,
                    endpoint,
                    route,
                    talk.clone(),
                    events.clone(),
                    command_tx.clone(),
                    timeout,
                )));
            }
            ChannelCommand::TalkFallback { index } => {
                let Some(runtime) = channels.remove(&index) else { continue };
                runtime.task.abort();
                // The session is reopened to carry the microphone, and what it
                // measured before is not what it will measure now.
                stats.clear(index);
                let endpoint = runtime.endpoint.with_talk(true);
                talk_support.entry(endpoint.uri.clone()).or_default().refuses_call = true;
                tracing::debug!(
                    target: "xgview::talk",
                    camera = %endpoint.camera_id,
                    "this camera takes the back channel on the picture's session"
                );
                let wake = runtime.wake;
                let task = handle.spawn(run_channel(
                    endpoint.clone(),
                    prefer_hardware,
                    settings.clone(),
                    events.clone(),
                    frames.clone(),
                    stats.clone(),
                    audio.clone(),
                    talk.clone(),
                    wake.clone(),
                ));
                channels.insert(index, ChannelRuntime { endpoint, task, wake });
            }
            ChannelCommand::SetPolicy(new_policy) => {
                match settings.lock() {
                    Ok(mut current) => current.reconnect = new_policy,
                    Err(poisoned) => poisoned.into_inner().reconnect = new_policy,
                }
                tracing::debug!(target: "xgview::pipeline", "reconnection policy replaced");
            }
            ChannelCommand::SetHandshakeTimeout(timeout) => {
                match settings.lock() {
                    Ok(mut current) => current.handshake_timeout = timeout,
                    Err(poisoned) => poisoned.into_inner().handshake_timeout = timeout,
                }
                tracing::debug!(
                    target: "xgview::pipeline",
                    timeout_ms = timeout.as_millis() as u64,
                    "handshake timeout replaced"
                );
            }
            ChannelCommand::Shutdown => break,
        }
    }

    for (_, runtime) in channels.drain() {
        runtime.task.abort();
    }
    tracing::debug!(target: "xgview::pipeline", "supervisor stopped");
}
