//! The Streams tab, the audio devices, and the microphone the magnified bar
//! opens.

use super::*;

impl XgViewApp {
    /// Points the sink at the magnified camera, or at nothing when the grid is
    /// showing, and follows the sound switch. Called whenever the zoom or the
    /// switch changes; the hub's index is the camera the listener is listening
    /// to, and the sink's active flag is whether anything comes out at all.
    ///
    /// The talk-back target moves with it: only the magnified session asks for a
    /// back channel, so naming the camera here is what opens one there - and a
    /// microphone left open on the camera the viewer just left is closed.
    ///
    /// **A call mutes the speaker, unless the platform cancels the echo.** The
    /// speaker and the microphone are one device, and a speaker playing into the
    /// microphone beside it is a loop: measured on a phone, the speaker came back
    /// through the microphone at a peak of 3003 while the room sat at 150-300,
    /// which is the howl an echo canceller exists to break. Where the platform
    /// has one and the capture came up with it - Android's `AudioRecord` on
    /// `VOICE_COMMUNICATION` with the mode in `MODE_IN_COMMUNICATION` and the
    /// speakerphone forced on, playing back through an `AudioTrack` on the same
    /// path - the camera keeps playing and the call is full duplex;
    /// [`Self::call_mutes_speaker`] is what says so. Everywhere else the loop is
    /// broken the other way round - half duplex, the way a walkie-talkie is:
    /// while the microphone is open the camera's sound is muted, and hanging up
    /// brings it back. The sound switch is left as the viewer set it. See
    /// [`Self::set_talking`].
    pub(super) fn apply_audio(&mut self) {
        if let Some(sink) = &self.audio {
            sink.set_active(self.sound_on && !self.call_mutes_speaker());
            sink.set_volume(self.config.volume);
        }
        let index = self.scheduler.zoom();
        self.manager.audio().set_index(index);
        self.manager.set_talk(index);
        if self.talking && self.talk_index != index {
            self.set_talking(false);
        }
    }

    /// Whether the call in progress has to silence the speaker to keep it quiet.
    ///
    /// A call is full duplex by default: the camera's sound keeps playing while
    /// the microphone is open, which is what Android's processor does properly
    /// and what a desktop's separated speaker and microphone manage without it.
    /// A machine with the microphone beside its speaker can be made half duplex
    /// instead - the sound stops while the viewer talks - and that is the
    /// Audio tab's switch, `AppConfig::talk_mute`. See [`Self::apply_audio`].
    pub(super) fn call_mutes_speaker(&self) -> bool {
        self.talking && self.config.talk_mute
    }

    /// Switches the microphone on or off.
    ///
    /// Switching it on for the first time opens the device; a machine without
    /// one says so once and then stays silent - the picture is never affected.
    /// On Android the record permission is asked for the same way: the first
    /// press puts the system dialog up and says what to do about it, and the
    /// next press finds the capture open.
    pub(super) fn set_talking(&mut self, on: bool) {
        if on {
            if self.mic.is_none() {
                match crate::mic::Mic::open(
                    self.manager.talk().clone(),
                    self.config.audio_input.as_deref(),
                ) {
                    Ok(mic) => self.mic = Some(mic),
                    Err(crate::mic::MicError::Permission) => {
                        self.flash(monitor_i18n::tr("toast-microphone-permission"), ToastKind::Error);
                        return;
                    }
                    Err(crate::mic::MicError::NoDevice) => {
                        self.flash(monitor_i18n::tr("toast-no-microphone"), ToastKind::Error);
                        return;
                    }
                }
            }
        }
        self.talking = on;
        self.talk_index = on.then(|| self.scheduler.zoom()).flatten();
        self.manager.talk().set_talking(on);
        // The press is the call: a session of its own, opened while the
        // microphone is already pushing. The hub was told first, so the worker
        // that opens it finds a target and a queue to feed.
        if on {
            if let Some(index) = self.talk_index {
                self.manager.call(index);
            }
        }
        // The speaker goes quiet while the microphone is open, unless the
        // platform's canceller came with the microphone: it is the same device,
        // and without a canceller the sound it makes comes straight back in
        // through the microphone. See `apply_audio`.
        if let Some(sink) = &self.audio {
            sink.set_active(self.sound_on && !self.call_mutes_speaker());
            sink.set_volume(self.config.volume);
        }
        tracing::debug!(target: "xgview::talk", on, "the talk-back changed");
        // The bar now carries the one control that ends the call instead of the
        // microphone the press landed on, so the focus it had is gone with it;
        // the bar's seat puts the remote back on the strip without a ring, and
        // the first arrow is what reaches the handset. See `tilebar`.
    }

    /// What the viewer pulls off each camera, and what turns it into pictures.
    pub(super) fn settings_streams(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        ui.label(
            RichText::new(monitor_i18n::tr("streams-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        ui.add_space(theme::space::M);
        ui.label(RichText::new(monitor_i18n::tr("settings-reconnect")).strong());
        let reconnect = &mut self.config.reconnect;
        // Borrowed beside the policy: they are two fields of the same struct,
        // and the slider that owns it sits among the retry ones.
        let handshake_timeout_ms = &mut self.config.handshake_timeout_ms;
        let first = controls::slider(&mut self.nav, ui, &mut reconnect.initial_delay_ms, 100..=10_000, 100.0, " ms", &monitor_i18n::tr("streams-first-retry"), 1_000);
        dirty |= first.changed();
        let first_frame = controls::slider(&mut self.nav, ui, &mut reconnect.first_frame_delay_ms, 1_000..=30_000, 500.0, " ms", &monitor_i18n::tr("streams-first-frame-retry"), 5_000);
        dirty |= first_frame.changed();
        let second = controls::slider(&mut self.nav, ui, &mut reconnect.max_delay_ms, 1_000..=300_000, 1000.0, " ms", &monitor_i18n::tr("streams-then-at-most"), 30_000);
        dirty |= second.changed();
        // One attempt's budget for establishing the session. It sits with the
        // retry timings because it is read at the same moment and reaches a
        // stalled channel the same way, and because a camera reached over the
        // internet can need far more of it than one on the local network.
        let handshake = controls::slider(&mut self.nav, ui, handshake_timeout_ms, 5_000..=60_000, 1_000.0, " ms", &monitor_i18n::tr("streams-handshake-timeout"), 15_000);
        dirty |= handshake.changed();
        ui.label(RichText::new(monitor_i18n::tr("streams-change-note")).small().color(theme::TEXT_DIM));
        // The shape of the curve is settled once, by whoever sized the network,
        // and never looked at again: it belongs behind a fold, not on the tab.
        // The fold is a stop of its own, ahead of the controls it hides; see
        // `controls::fold`.
        let backoff = monitor_i18n::tr("streams-backoff");
        controls::fold(&mut self.nav, ui, &backoff, false, |ui, nav| {
            let factor = controls::slider(nav, ui, &mut reconnect.multiplier, 1.0..=5.0, 0.05, "", &monitor_i18n::tr("streams-factor"), 1.8);
            dirty |= factor.changed();
            let jitter = controls::slider(nav, ui, &mut reconnect.jitter, 0.0..=1.0, 0.02, "", &monitor_i18n::tr("streams-jitter"), 0.2);
            dirty |= jitter.changed();
            let spread = controls::slider(nav, ui, &mut reconnect.spread_ms, 0..=1_000, 50.0, " ms", &monitor_i18n::tr("streams-spread"), 100);
            dirty |= spread.changed();
            let attempts = controls::slider(nav, ui, &mut reconnect.max_attempts, 0..=100, 1.0, "", &monitor_i18n::tr("streams-attempts"), 0);
            dirty |= attempts.changed();
        });

        // At this point `dirty` is only the connection sliders above. They are
        // handed to the supervisor so that the channels already running pick the
        // new values up at their next attempt instead of only the ones opened
        // after the change - a running channel holds no copy of its own to go
        // stale.
        if dirty {
            self.manager.set_reconnect_policy(self.config.reconnect.clone());
            self.manager
                .set_handshake_timeout(Duration::from_millis(self.config.handshake_timeout_ms));
        }

        ui.add_space(theme::space::M);
        ui.label(RichText::new(monitor_i18n::tr("settings-decoding")).strong());
        let mut prefer_hardware = self.config.prefer_hardware_decode;
        ui.add_enabled_ui(self.decoder_selectable, |ui| {
            let hardware = ui.checkbox(&mut prefer_hardware, monitor_i18n::tr("streams-prefer-hardware"));
            self.nav.item(&hardware);
            if hardware.changed() {
                self.config.prefer_hardware_decode = prefer_hardware;
                // A decoder is chosen when a session opens, so the change is applied
                // by reopening the channels rather than on the next connection.
                self.manager.set_hardware_preference(prefer_hardware);
                dirty = true;
                self.flash(
                    monitor_i18n::tr(if prefer_hardware {
                        "toast-decoding-gpu"
                    } else {
                        "toast-decoding-cpu"
                    }),
                    ToastKind::Info,
                );
            }
        });
        // A build with no hardware decoder at all is told apart first: the
        // "nothing to switch" wording describes a platform that decodes in
        // hardware and sees to its own fallbacks, which such a build is not.
        let summary = if !self.hardware_decoder {
            monitor_i18n::tr("streams-summary-software")
        } else if !self.decoder_selectable {
            monitor_i18n::tr("streams-summary-fixed")
        } else {
            monitor_i18n::tr("streams-summary-available")
        };
        ui.label(RichText::new(summary).small().color(theme::TEXT_DIM));

        ui.add_space(theme::space::M);
        ui.label(RichText::new(monitor_i18n::tr("settings-audio")).strong());
        // The volume slider carries the sound switch on its right, the way the
        // magnified bar does: the icon shows the same `sound_on` the bar flips,
        // so the two always agree. The reset button the shared `controls::slider`
        // would draw is left out - the switch is the row's second control.
        let (volume, mute) = ui
            .horizontal(|ui| {
                ui.label(monitor_i18n::tr("streams-volume"));
                let slider_response =
                    ui.add(egui::Slider::new(&mut self.config.volume, 0.0..=1.0).step_by(0.05));
                // The glyph is the state: a speaker when sound is on, a crossed
                // speaker when it is not - the same pair the magnified bar
                // draws. The button is as tall as a text button on this row, so
                // that the slider, the words and the switch share one baseline,
                // and the drawing takes most of that box rather than the
                // toolbar's half: a line-high square would otherwise leave the
                // shape looking like a note beside the text.
                let side = ui.text_style_height(&egui::TextStyle::Button)
                    + 2.0 * ui.spacing().button_padding.y;
                let mute = icons::button_sized_glyph(
                    ui,
                    if self.sound_on { Icon::Sound } else { Icon::Mute },
                    false,
                    false,
                    &monitor_i18n::tr("streams-sound"),
                    side,
                    0.7,
                );
                (slider_response, mute)
            })
            .inner;
        self.nav.item_kind(Kind::Slider, &volume);
        self.nav.item(&mute);
        if mute.clicked() {
            self.sound_on = !self.sound_on;
            self.config.sound_on = self.sound_on;
            self.apply_audio();
            self.mark_dirty();
            dirty = true;
        }
        if volume.changed() {
            if let Some(sink) = &self.audio {
                sink.set_volume(self.config.volume);
            }
            dirty = true;
        }
        ui.label(RichText::new(monitor_i18n::tr("streams-volume-hint")).small().color(theme::TEXT_DIM));

        ui.add_space(theme::space::S);
        let talk_mute = ui.checkbox(
            &mut self.config.talk_mute,
            monitor_i18n::tr("streams-talk-mute"),
        );
        self.nav.item(&talk_mute);
        self.name(&talk_mute, "settings-talk-mute");
        if talk_mute.changed() {
            self.apply_audio();
            self.mark_dirty();
            dirty = true;
        }
        ui.label(
            RichText::new(monitor_i18n::tr("streams-talk-mute-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        // Which device the sound leaves through, and which one the microphone
        // listens to. A machine with one sound card never needs this; a kiosk
        // with a USB dongle and a television on HDMI does, because the system's
        // default is card 0 and that is not always the one with a speaker behind
        // it. The rows are left out entirely when the platform has nothing to
        // list - Android, whose audio routing is the system's, and a machine that
        // reports no devices at all.
        dirty |= self.audio_device_rows(ui);

        dirty
    }

    /// The output and input device choices, and whether either changed.
    ///
    /// The names are the platform's own and are not translated: `plughw:CARD=PCH,DEV=3`
    /// is a fact about the machine, not a word in a language. The first entry of
    /// each list is the system's default, which is what a configuration written
    /// before this existed means.
    pub(super) fn audio_device_rows(&mut self, ui: &mut egui::Ui) -> bool {
        /// The values the control cycles through: the system's default, every
        /// device the platform lists, and the configured one even when the list
        /// has lost it - a device that is open can drop out of the enumeration,
        /// and the one in use has to stay reachable to be cycled away from.
        fn options_for(
            devices: &[crate::audio::AudioDevice],
            current: Option<&str>,
        ) -> Vec<Option<String>> {
            let mut options: Vec<Option<String>> = std::iter::once(None)
                .chain(devices.iter().map(|device| Some(device.name.clone())))
                .collect();
            if let Some(current) = current {
                if !options.iter().any(|option| option.as_deref() == Some(current)) {
                    options.push(Some(current.to_string()));
                }
            }
            options
        }

        let mut changed = false;
        let outputs = self.audio_outputs.clone();
        let inputs = self.audio_inputs.clone();
        let system = monitor_i18n::tr("streams-audio-system");

        // A cycling button, not a drop-down: the popup a `ComboBox` opens is
        // egui's own and the navigation layer cannot walk into it, so a remote
        // would reach the control and then have no way to choose. Left and Right
        // - or Enter, or a click - step to the next device and wrap at the ends,
        // which is how the language picker and the OSD corners already work.
        if !outputs.is_empty() {
            ui.add_space(theme::space::S);
            ui.label(monitor_i18n::tr("streams-audio-output"));
            let mut choice = self.config.audio_output.clone();
            let options = options_for(&outputs, self.config.audio_output.as_deref());
            let button = controls::cycle_button(&mut self.nav, ui, &options, &mut choice, |choice| {
                match choice {
                    None => system.clone(),
                    Some(name) => crate::audio::device_label(name, true),
                }
            });
            self.focus_names.insert(button.id, "settings-audio-output");
            if button.changed() {
                self.config.audio_output = choice;
                self.reopen_audio_sink();
                changed = true;
            }
        }

        if !inputs.is_empty() {
            ui.add_space(theme::space::S);
            ui.label(monitor_i18n::tr("streams-audio-input"));
            let mut choice = self.config.audio_input.clone();
            let options = options_for(&inputs, self.config.audio_input.as_deref());
            let button = controls::cycle_button(&mut self.nav, ui, &options, &mut choice, |choice| {
                match choice {
                    None => system.clone(),
                    Some(name) => crate::audio::device_label(name, false),
                }
            });
            self.focus_names.insert(button.id, "settings-audio-input");
            if button.changed() {
                self.config.audio_input = choice;
                // The capture is opened on the press, so a call in progress is
                // ended here rather than left listening to the old device: the
                // microphone is reopened, on the new one, at the next press.
                if self.talking {
                    self.set_talking(false);
                }
                self.mic = None;
                changed = true;
            }
        }

        if changed {
            self.mark_dirty();
        }
        changed
    }

    /// Reopens the sink after the output device changed.
    ///
    /// The old one is dropped with the new one's arrival: a stream is not
    /// reopened in place, and the ring buffer it was draining goes with it.
    pub(super) fn reopen_audio_sink(&mut self) {
        self.audio = crate::audio::Sink::open(self.config.audio_output.as_deref());
        match &self.audio {
            Some(sink) => {
                sink.set_volume(self.config.volume);
                self.manager.audio().set_output(Some(sink.clone()));
            }
            None => self.manager.audio().set_output(None),
        }
        self.apply_audio();
    }

    /// Plays a test tone to the camera a draft describes.
    ///
    /// The tone is played from the draft rather than the saved entry, so a mode
    /// can be tried before it is kept and a camera that has never been saved can
    /// still be heard. Both the edit window's button and the manual tab's reach
    /// it, which is why it is a method and not a block in one window.
    pub(super) fn spawn_test_tone(&mut self, source: CameraSource) {
        let uri = source.active_uri(StreamKind::Main).to_string();
        let credentials = source
            .username
            .as_ref()
            .map(|user| (user.clone(), source.password.clone().unwrap_or_default()));
        let mode = source.talk;
        let (sender, receiver) = unbounded();
        self.handle.spawn(async move {
            let _ = sender.send(monitor_core::talk::test_tone(&uri, credentials, mode).await);
        });
        self.talk_test = Some(receiver);
        self.flash(monitor_i18n::tr("toast-talk-test-playing"), ToastKind::Info);
    }

    /// Takes what a sealing thread has come to, if it has.
    ///
    /// Run once a frame with the other answers the background jobs leave behind.
    /// The thread is left to finish by itself when the job is over - what it
    /// holds is a document and a passphrase, and there is nothing to gain by
    /// stopping it half way.
    /// Reports the test tone once it is over.
    ///
    /// Polled with the other background jobs: a tone is a second and a half of
    /// socket work, and the answer arrives long after the press.
    pub(super) fn poll_talk_test(&mut self) {
        use crossbeam_channel::TryRecvError;

        let Some(receiver) = self.talk_test.take() else { return };
        match receiver.try_recv() {
            Ok(Ok(())) => self.flash(monitor_i18n::tr("toast-talk-test-sent"), ToastKind::Info),
            Ok(Err(ToneFailure::Off)) => {
                self.flash(monitor_i18n::tr("toast-talk-test-off"), ToastKind::Error)
            }
            Ok(Err(ToneFailure::NoRoute)) => {
                self.flash(monitor_i18n::tr("toast-talk-test-no-route"), ToastKind::Error)
            }
            Ok(Err(ToneFailure::Failed(reason))) => self.flash(
                monitor_i18n::tr_args("toast-talk-test-failed", &[("reason", reason.into())]),
                ToastKind::Error,
            ),
            Err(TryRecvError::Empty) => self.talk_test = Some(receiver),
            Err(TryRecvError::Disconnected) => {
                self.flash(monitor_i18n::tr("toast-talk-test-failed-unknown"), ToastKind::Error)
            }
        }
    }

    /// Whether the magnified camera has sound for the bar to switch.
    ///
    /// A camera that has not reported yet counts as having it: the switch shows
    /// with the picture and only goes away once the stream itself says it
    /// carries no audio track, so it never flickers in and out while a session
    /// is coming up.
    pub(super) fn magnified_has_audio(&self) -> bool {
        self.scheduler
            .zoom()
            .map_or(false, |index| self.channels.get(&index).map_or(true, |channel| channel.audio))
    }

    /// Whether the magnified camera offers a talk-back media.
    ///
    /// The answer comes from a probe of the camera, not from the picture's
    /// session: that one carries no back channel and asks for none, which is
    /// what keeps a magnified tile from holding a camera's speaker open while
    /// nobody is talking. There is still no optimistic default - the microphone
    /// appears when a camera has said yes - but the yes is the camera's, so it
    /// survives the session being reopened under it.
    pub(super) fn magnified_has_backchannel(&self) -> bool {
        self.magnified_stream_uri()
            .is_some_and(|uri| self.talk_route.get(&uri).is_some_and(|route| route.reachable()))
    }

    /// The address a call to the magnified camera would be placed to.
    ///
    /// The main stream, whichever one the grid is pulling: a call goes to what
    /// the viewer is looking at, and that is the address the probe asked about.
    pub(super) fn magnified_stream_uri(&self) -> Option<String> {
        let index = self.scheduler.zoom()?;
        let slot = self.config.active_slots().get(index).copied()?;
        let camera = self.config.cameras.get(slot)?;
        Some(camera.stream_uri(StreamKind::Main).to_string())
    }
}
