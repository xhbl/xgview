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

/// The task that owns the channels, and what it carries from one command to the
/// next.
///
/// It was one function with ten arguments and a `match` whose arms each reached
/// into the same five locals. They are fields now, so a command is a method that
/// says what it does and nothing else - and the three places that start a
/// channel are one call.
pub(super) struct Supervisor {
    channels: HashMap<usize, ChannelRuntime>,
    /// The channel the viewer may talk to. Kept beside the channels because it
    /// outlives the sessions it opens: a channel reopened for another reason
    /// gets the back channel back if it is still the one magnified.
    talk_target: Option<usize>,
    /// What is known about each camera's back channel, keyed by the address a
    /// call would be placed to. A camera sitting in two slots is one entry, and
    /// a channel that moves between streams asks about the stream it is on.
    talk_support: HashMap<String, TalkEntry>,
    /// The call in progress, if any. One at a time: the hub has a single target,
    /// and two workers would send the same microphone twice.
    call: Option<JoinHandle<()>>,
    /// The decoding preference a channel is opened with. A change reopens the
    /// live ones, because the decoder is chosen when a session opens.
    prefer_hardware: bool,
    /// The command channel, read for commands and written back for the answer a
    /// probe gives itself.
    commands: UnboundedReceiver<ChannelCommand>,
    command_tx: UnboundedSender<ChannelCommand>,
    /// Where a session publishes what it produces.
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    stats: Arc<StatsStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
    /// What a channel reads afresh before each attempt.
    settings: Arc<Mutex<SessionSettings>>,
    handle: Handle,
}

impl Supervisor {
    pub(super) fn new(
        commands: UnboundedReceiver<ChannelCommand>,
        command_tx: UnboundedSender<ChannelCommand>,
        events: Sender<StreamEvent>,
        frames: Arc<FrameStore>,
        stats: Arc<StatsStore>,
        audio: Arc<AudioHub>,
        talk: Arc<TalkHub>,
        settings: Arc<Mutex<SessionSettings>>,
        prefer_hardware: bool,
        handle: Handle,
    ) -> Self {
        Self {
            channels: HashMap::new(),
            talk_target: None,
            talk_support: HashMap::new(),
            call: None,
            prefer_hardware,
            commands,
            command_tx,
            events,
            frames,
            stats,
            audio,
            talk,
            settings,
            handle,
        }
    }

    /// Reads commands until the manager says to stop.
    pub(super) async fn run(mut self) {
        while let Some(command) = self.commands.recv().await {
            match command {
                ChannelCommand::Activate { index, source, stream, previous_stream } => {
                    self.activate(index, source, stream, previous_stream)
                }
                ChannelCommand::Suspend { index } => self.suspend(index),
                ChannelCommand::Wake => self.wake(),
                ChannelCommand::PreferHardware(prefer) => self.prefer_hardware(prefer),
                ChannelCommand::Talk(target) => self.set_talk_target(target),
                ChannelCommand::ProbeTalk { index } => self.probe_talk(index),
                ChannelCommand::TalkProbed { uri, route, mode } => {
                    self.talk_probed(uri, route, mode)
                }
                ChannelCommand::Call { index } => self.call(index),
                ChannelCommand::TalkFallback { index } => self.talk_fallback(index),
                ChannelCommand::SetPolicy(policy) => self.set_policy(policy),
                ChannelCommand::SetHandshakeTimeout(timeout) => self.set_handshake_timeout(timeout),
                ChannelCommand::Shutdown => break,
            }
        }

        for (_, runtime) in std::mem::take(&mut self.channels) {
            runtime.task.abort();
        }
        tracing::debug!(target: "xgview::pipeline", "supervisor stopped");
    }

    /// Starts a channel and keeps it under the index it belongs to.
    ///
    /// Three commands used to spell this out - the nine arguments, the notify and
    /// the insert - and each had to remember all nine.
    fn spawn_channel(&mut self, endpoint: Endpoint, wake: Arc<Notify>) {
        let index = endpoint.index;
        let task = self.handle.spawn(run_channel(
            endpoint.clone(),
            self.prefer_hardware,
            self.settings.clone(),
            self.events.clone(),
            self.frames.clone(),
            self.stats.clone(),
            self.audio.clone(),
            self.talk.clone(),
            wake.clone(),
        ));
        self.channels.insert(index, ChannelRuntime { endpoint, task, wake });
    }

    fn activate(
        &mut self,
        index: usize,
        source: CameraSource,
        stream: StreamKind,
        previous_stream: Option<StreamKind>,
    ) {
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
        if let Some(existing) = self.channels.get(&index) {
            if existing.endpoint.is_same_stream(&endpoint) {
                return;
            }
        }
        if let Some(existing) = self.channels.remove(&index) {
            existing.task.abort();
            // The sample belongs to the session being replaced, and it
            // is dropped before the transition is announced: read after
            // it, the numbers of the stream that is going away would
            // overwrite the state the switch just set - including the
            // held frame it is meant to keep on screen.
            self.stats.clear(index);
        }
        if previous_stream.is_some() {
            let _ = self.events.send(StreamEvent::Transition {
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
        self.spawn_channel(endpoint, Arc::new(Notify::new()));
    }

    fn suspend(&mut self, index: usize) {
        if let Some(existing) = self.channels.remove(&index) {
            existing.task.abort();
            // The numbers belonged to the session that was just stopped.
            self.stats.clear(index);
            tracing::debug!(target: "xgview::pipeline", index, "channel suspended");
        }
    }

    fn wake(&mut self) {
        // Only the channels waiting out a backoff care. `notify_waiters`
        // leaves no permit behind, so one that is streaming is untouched
        // and one that is connecting finishes its attempt.
        for runtime in self.channels.values() {
            runtime.wake.notify_waiters();
        }
        tracing::debug!(
            target: "xgview::pipeline",
            channels = self.channels.len(),
            "woken for a resume"
        );
    }

    fn prefer_hardware(&mut self, prefer: bool) {
        if prefer == self.prefer_hardware {
            return;
        }
        self.prefer_hardware = prefer;
        tracing::debug!(
            target: "xgview::pipeline",
            hardware = prefer,
            channels = self.channels.len(),
            "decoding preference changed, reopening the channels"
        );
        // The decoder is chosen when a session opens, so the live
        // channels have to be reopened for the new preference to reach
        // them. They are started again from here because the scheduler
        // has no reason to see a change: nothing about the cameras did.
        for (index, runtime) in self.channels.drain().collect::<Vec<_>>() {
            runtime.task.abort();
            // Reopened, not continued: the sample described the decoder
            // this channel is leaving behind, and `hardware` in it would
            // be the answer that has just been changed.
            self.stats.clear(index);
            self.spawn_channel(runtime.endpoint, runtime.wake);
        }
    }

    fn set_talk_target(&mut self, target: Option<usize>) {
        if self.talk_target == target {
            return;
        }
        self.talk_target = target;
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
        self.talk.set_target(target);
        debug_assert_eq!(
            self.talk.target(),
            target,
            "the hub has to follow the session's talk target"
        );
        tracing::debug!(
            target: "xgview::pipeline",
            camera = ?target,
            "the talk target moved"
        );
    }

    fn probe_talk(&mut self, index: usize) {
        let Some(runtime) = self.channels.get(&index) else { return };
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
            let _ = self.events.send(answer(TalkRoute::None));
            return;
        }
        let entry = self.talk_support.entry(uri.clone()).or_default();
        if entry.fresh() && entry.mode == endpoint.talk_mode {
            let _ = self.events.send(answer(entry.route));
            return;
        }
        // One probe per address at a time: the second caller would only
        // ask the same camera the same question.
        if entry.probing {
            return;
        }
        entry.probing = true;
        let timeout = {
            let settings = self.settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            settings.handshake_timeout
        };
        let credentials = endpoint.credentials.clone();
        let camera_id = endpoint.camera_id.clone();
        let commands = self.command_tx.clone();
        self.handle.spawn(async move {
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

    fn talk_probed(&mut self, uri: String, route: TalkRoute, mode: TalkMode) {
        let entry = self.talk_support.entry(uri.clone()).or_default();
        entry.probing = false;
        if route != TalkRoute::Unknown {
            entry.route = route;
            entry.mode = mode;
            entry.answered = Some(Instant::now());
        }
        // The answer is the camera's, so every channel showing that
        // address hears it - not only the one that asked.
        for (index, runtime) in &self.channels {
            if runtime.endpoint.uri == uri {
                let _ = self.events.send(StreamEvent::Talk {
                    index: *index,
                    camera_id: runtime.endpoint.camera_id.clone(),
                    uri: uri.clone(),
                    route: entry.route,
                });
            }
        }
    }

    fn call(&mut self, index: usize) {
        let Some(runtime) = self.channels.get(&index) else {
            // The channel went away between the press and this command.
            // Saying so beats a microphone that does nothing.
            let _ = self.events.send(StreamEvent::CallFailed {
                index,
                camera_id: String::new(),
                reason: CallFailure::Unreachable,
            });
            return;
        };
        let endpoint = runtime.endpoint.clone();
        // MJPEG and the MSE fallback carry no media the client sends.
        // The probe already answers `Unsupported` for them, so this is
        // only the case where the press beat the answer.
        if endpoint.is_mjpeg() || endpoint.is_ws() {
            let _ = self.events.send(StreamEvent::CallFailed {
                index,
                camera_id: endpoint.camera_id.clone(),
                reason: CallFailure::NoBackChannel,
            });
            return;
        }
        // This camera has already said it will not take a call of its
        // own. Going straight to the session that can carry the media
        // spares the viewer a wait for a refusal that is already known.
        if self.talk_support.get(&endpoint.uri).is_some_and(|entry| entry.refuses_call) {
            let _ = self.command_tx.send(ChannelCommand::TalkFallback { index });
            return;
        }
        if let Some(previous) = self.call.take() {
            previous.abort();
        }
        let timeout = {
            let settings = self.settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            settings.handshake_timeout
        };
        // How this camera is reached, as its probe answered. A press
        // that beat the answer falls back to the standard route, which
        // is what every camera was before there was a choice.
        let route = self.talk_support
            .get(&runtime.endpoint.uri)
            .map(|entry| entry.route)
            .filter(|route| route.reachable())
            .unwrap_or(TalkRoute::Standard);
        self.call = Some(self.handle.spawn(talk_worker(
            index,
            endpoint,
            route,
            self.talk.clone(),
            self.events.clone(),
            self.command_tx.clone(),
            timeout,
        )));
    }

    fn talk_fallback(&mut self, index: usize) {
        let Some(runtime) = self.channels.remove(&index) else { return };
        runtime.task.abort();
        // The session is reopened to carry the microphone, and what it
        // measured before is not what it will measure now.
        self.stats.clear(index);
        let endpoint = runtime.endpoint.with_talk(true);
        self.talk_support.entry(endpoint.uri.clone()).or_default().refuses_call = true;
        tracing::debug!(
            target: "xgview::talk",
            camera = %endpoint.camera_id,
            "this camera takes the back channel on the picture's session"
        );
        self.spawn_channel(endpoint, runtime.wake);
    }

    fn set_policy(&mut self, new_policy: ReconnectPolicy) {
        match self.settings.lock() {
            Ok(mut current) => current.reconnect = new_policy,
            Err(poisoned) => poisoned.into_inner().reconnect = new_policy,
        }
        tracing::debug!(target: "xgview::pipeline", "reconnection policy replaced");
    }

    fn set_handshake_timeout(&mut self, timeout: Duration) {
        match self.settings.lock() {
            Ok(mut current) => current.handshake_timeout = timeout,
            Err(poisoned) => poisoned.into_inner().handshake_timeout = timeout,
        }
        tracing::debug!(
            target: "xgview::pipeline",
            timeout_ms = timeout.as_millis() as u64,
            "handshake timeout replaced"
        );
    }
}
