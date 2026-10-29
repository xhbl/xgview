//! Keys, gestures and the exit question: everything that turns an input into
//! something the viewer sees.

use super::*;

impl XgViewApp {
    /// Remembers where a press went down, so the release can be read as a tap.
    ///
    /// egui forgets a press's start time on the frame the press ends, which is
    /// exactly the frame the answer is needed on; the press is therefore kept
    /// here while it lasts. A second finger while the press is down is a pinch -
    /// or the beginning of one - and is remembered too, because the touches are
    /// gone by the release.
    pub(super) fn track_tap(&mut self, ctx: &egui::Context) {
        ctx.input(|input| {
            if input.pointer.any_pressed() {
                if let Some(origin) = input.pointer.press_origin() {
                    self.tap_press = Some((origin, input.time));
                    self.tap_multi = false;
                }
            }
            if input.multi_touch().is_some() {
                self.tap_multi = true;
            }
        });
    }

    /// Where a tap ended this frame, if the press was one.
    ///
    /// A tap is a press that came up quickly, stayed where it went down, and was
    /// the only finger on the glass. A press that lingered is a thumb resting on
    /// the picture, one that slid is a pan being reached for, and a pinch is two
    /// fingers: the magnified viewport's bar goes on none of them. See
    /// [`TAP_SECS`] and [`TAP_POINTS`].
    pub(super) fn tap_release(&mut self, ctx: &egui::Context) -> Option<egui::Pos2> {
        let (origin, started) = self.tap_press?;
        let released = ctx.input(|input| input.pointer.any_released());
        if !released {
            return None;
        }
        // The press is over either way, so it is never read twice.
        self.tap_press = None;
        let multi = std::mem::take(&mut self.tap_multi);
        if multi {
            return None;
        }
        ctx.input(|input| {
            let at = input.pointer.interact_pos()?;
            let quick = input.time - started <= TAP_SECS;
            let still = (at - origin).length() <= TAP_POINTS;
            (quick && still).then_some(at)
        })
    }

    /// Whether a Back, on this frame, is the viewer asking for the bars.
    ///
    /// Back unwinds one layer at a time - the magnified viewport, then whatever
    /// is open over the wall - and a Back that does that is the wall's, which is
    /// not a request for the bars. With nothing left to unwind it is the viewer
    /// asking for the menus instead, and it is the one key a remote control has
    /// that reads that way: the bars are where the settings, the device window
    /// and full screen are, and a remote has no function keys at all. See
    /// [`Self::back`], which leaves the exit question to a Back that arrives
    /// with the bars already on screen.
    pub(super) fn back_is_a_door(&self) -> bool {
        !self.scheduler.is_zoomed() && !self.discovery.open && !self.show_settings
    }

    /// Whether one event, with the remote's focus on the wall, asks for the
    /// bars. The wall's whole answer, one event at a time; see
    /// [`Self::viewer_active`] for why.
    ///
    /// `back_is_a_door` is [`Self::back_is_a_door`], read once for the frame.
    ///
    /// `band_is_a_door` is whether a press in the band a bar sits in is that
    /// bar's: true on the grid, and true while magnified with the bars on
    /// screen, where the wall's own band lies below theirs. It is false only in
    /// the one state where the two bands are the same part of the screen - the
    /// magnified viewport with the bars away - and there the band is the
    /// viewport's own bar being asked for. The menus are still reachable there:
    /// `F1`/`F2`, or Back a layer at a time. See [`crate::tilebar`].
    pub(super) fn wall_event(
        bars: Option<(Rect, Rect)>,
        back_is_a_door: bool,
        band_is_a_door: bool,
        event: &egui::Event,
    ) -> bool {
        match event {
            // Back is the wall's own key while there is something on the wall
            // for it to unwind - see `back` - and the viewer asking for the
            // menus when there is not.
            egui::Event::Key { key, pressed: true, .. } => {
                if matches!(*key, Key::Escape | Key::Backspace | Key::BrowserBack) {
                    back_is_a_door
                } else {
                    !Self::wall_key(*key)
                }
            }
            egui::Event::Key { pressed: false, .. } => false,
            // A press on the wall is a press on the picture - a tile to focus,
            // or a double press to magnify it - and a press in the band a bar
            // sits in is the pointer asking for that bar. A release is the tail
            // of a press that has already been answered, and never a request of
            // its own.
            //
            // Magnified with the bars away, that band belongs to the viewport's
            // own bar; see above.
            egui::Event::PointerButton { pos, pressed: true, .. } => {
                band_is_a_door && Self::in_a_bar_band(bars, *pos)
            }
            egui::Event::PointerButton { pressed: false, .. } => false,
            // A touch is placed by where it is, whatever phase it is in. A tap
            // on a touch screen arrives as a `Start` and an `End` at the same
            // place, and the end is not a second request for the bars - reading
            // it as one is what brought the bars back under every touch on the
            // wall, however carefully its start had been placed.
            egui::Event::Touch { pos, .. } => band_is_a_door && Self::in_a_bar_band(bars, *pos),
            // What a printable key arrives as *on top of* its key event, which
            // is why the grid digits are read as keys above: a character typed
            // at the wall does nothing at all - there is no field on the wall to
            // type into - and counting it would put the bars back every time the
            // viewer picked a grid with `1`, `2`, `3` or `4`.
            egui::Event::Text(_) => false,
            egui::Event::PointerMoved(_)
            | egui::Event::MouseMoved(_)
            | egui::Event::WindowFocused(_)
            | egui::Event::Screenshot { .. }
            | egui::Event::PointerGone => false,
            // A wheel and a trackpad's pinch are the viewer at the *picture*: in
            // the magnified viewport a wheel pushes it in and a drag moves it.
            // Turning one is no more a request for the bars than turning a page
            // is - the wall's own keys and a swipe are read the same way - so in
            // full screen a zoom leaves the bars to their own clock rather than
            // bringing them back, and either is answered by the picture rather
            // than by a press in the band the bars sit in.
            egui::Event::MouseWheel { .. } | egui::Event::Zoom(_) => false,
            // A dropped file, and anything an integration sends that is not named
            // here: the viewer, and none of it is something done on the wall. The
            // bars are asked for by a press in their band, by Back or by a
            // function key, and by nothing else.
            _ => true,
        }
    }

    /// Whether `input` holds anything the viewer did, as opposed to the
    /// window's own bookkeeping.
    ///
    /// Only what a person does counts. The window being resized or focused is
    /// the system, and entering full screen makes both happen; a key or a button
    /// coming back up is the tail of a press that has already counted. Reading
    /// any of it as "a viewer is here" would undo the hiding that the press
    /// itself asked for - which is exactly what entering full screen means - so
    /// the bars would come straight back.
    ///
    /// The two mouse-motion events egui sends have to be told apart, because
    /// egui sends both for one movement of the mouse:
    ///
    /// * `PointerMoved` carries a position relative to the window, so a resize
    ///   moves it under a cursor that never moved, and it cannot be trusted;
    /// * `MouseMoved` carries the raw hardware delta, which no resize produces,
    ///   and which is therefore what lets a desktop wake the bars by moving the
    ///   mouse without a change of the window's shape doing it too.
    ///
    /// This is the whole of [`Self::viewer_active`] for a viewer who is in the
    /// menus: with the focus on a control there is no wall to leave alone, and
    /// anything that keeps the control reachable keeps the bars - which the
    /// control it is part of cannot be drawn without. See
    /// [`Self::chrome_visible`].
    pub(super) fn any_input(input: &egui::InputState) -> bool {
        input.pointer.any_down()
            || input.events.iter().any(|event| match event {
                egui::Event::Key { pressed, .. } => *pressed,
                egui::Event::PointerButton { pressed, .. } => *pressed,
                egui::Event::PointerMoved(_)
                | egui::Event::WindowFocused(_)
                | egui::Event::Screenshot { .. }
                | egui::Event::PointerGone => false,
                _ => true,
            })
    }

    /// Whether the keys read this frame walk the focus ring, which is what arms
    /// it.
    ///
    /// The arrows walk it - wherever it is, on the wall or on a control - and so
    /// do Back and the F1/F2 that open the bars and carry it into them. Enter and
    /// Space count only while the **wall** owns the keys (`wall_owns`): on the
    /// wall they act on what the ring is on, while on a control they activate the
    /// control - which is how a remote page button gets pressed, and that is a
    /// page turn rather than an aim. The **page keys and the layout digits are
    /// left out on purpose**: they change what the wall is showing, not where the
    /// ring is standing, and that is also what a swipe and the toolbar's page
    /// arrows do, so all three ways of turning a page behave alike. See
    /// [`Self::ring_follows_cursor`], which takes a ring that is already showing
    /// down when the wall moves its own cursor - a page turned, or a layout that
    /// re-places the focus.
    pub(super) fn ring_keys(keys: &Keys, wall_owns: bool) -> bool {
        keys.left
            || keys.right
            || keys.up
            || keys.down
            || (keys.enter && wall_owns)
            || keys.back
            || keys.settings
            || keys.devices
    }

    /// Keys that act on the wall and nowhere else.
    ///
    /// Enter and Space magnify the focused viewport and unwind it again;
    /// PageUp and PageDown turn the page; the arrows move the focus, and turn
    /// the page when the focus leaves a side; the digits pick a grid, which is
    /// a shape of the wall and nothing else. The arrows that walk off the *top*
    /// and *bottom* are not the way into the bars any more either: with the bars
    /// hidden there is nothing for them to land on, and they are asked of
    /// [`Self::hand_off`], which only hands over to a bar that is on screen.
    ///
    /// Back is not here on purpose. It is the one key a remote control has that
    /// a viewer reads as "the menus" - the bars are where the settings, the
    /// device window and full screen are, and a remote has no function keys at
    /// all - so it is left to wake them, by the same rule that gives that job to
    /// F1, F2 and F11. See [`Self::viewer_active`].
    pub(super) fn wall_key(key: Key) -> bool {
        matches!(
            key,
            Key::ArrowLeft
                | Key::ArrowRight
                | Key::ArrowUp
                | Key::ArrowDown
                | Key::Enter
                | Key::Space
                | Key::PageUp
                | Key::PageDown
                | Key::Num1
                | Key::Num2
                | Key::Num3
                | Key::Num4
        )
    }

    /// Whether `pos` is in the band one of the bars occupies.
    ///
    /// The bands are the ones the bars were last drawn in; see [`Chrome::bars`].
    /// Only their height is read: a bar spans the width of the window, and the
    /// rectangles can be a frame or two old - the frame the bars are drawn in to
    /// be measured, before a start in full screen takes them away again, is laid
    /// out for a window that may still be on its way to the screen it ends up
    /// on. Their height is what the viewer is aiming at either way.
    ///
    /// Before they have been drawn once there is no band at all, and a press
    /// anywhere is a press on the wall.
    pub(super) fn in_a_bar_band(bars: Option<(Rect, Rect)>, pos: egui::Pos2) -> bool {
        bars.is_some_and(|(top, bottom)| {
            (top.top()..=top.bottom()).contains(&pos.y)
                || (bottom.top()..=bottom.bottom()).contains(&pos.y)
        })
    }

    pub(super) fn back(&mut self, ctx: &egui::Context) {
        // BACK / Esc unwinds one layer at a time: the magnified viewport, then
        // whatever was opened over the wall. With nothing left to close, the
        // wall answers - and with the bars away that answer is the bars
        // themselves, which this press has already asked for; the question of
        // leaving is asked from the bars, where both it and the answer are on
        // screen. Leaving the program is not something a press reaching for the
        // menus should be able to start.
        //
        // `chrome.visible` is read here as the frame began: `chrome_visible`
        // runs after the keys have been handed out, so this is the state the
        // viewer was looking at.
        //
        // Full screen is not one of these layers any more: on a television the
        // viewer is in it from the start, so it cannot have been what the press
        // meant, and unwinding it would put an exit out of reach. F11 and the
        // settings panel's own checkbox are what toggle it.
        if self.scheduler.is_zoomed() {
            // Back unwinds the magnification a layer at a time: the bar the
            // viewport carries, and then the magnification itself. It is the one
            // key that gets out of it now - the confirm key is the bar's.
            //
            // A call is a layer of its own and Back is its way out, the same as
            // the handset: it goes before the bar, because the bar is what is
            // showing the handset.
            if self.talking {
                self.set_talking(false);
            } else if self.tile_bar.is_open() {
                self.tile_bar.hide();
            } else {
                self.zoom_out();
            }
            self.exit_armed = None;
        } else if self.discovery.open {
            self.discovery.open = false;
            self.exit_armed = None;
        } else if self.show_settings {
            self.show_settings = false;
            self.exit_armed = None;
        } else if self.chrome.visible && !self.supervised {
            self.arm_exit(ctx);
        } else {
            // Under a supervisor the wall is meant to stay up: Back is not a way
            // out of the program, only the exit button in the settings is.
            self.exit_armed = None;
        }
    }

    /// Asks before leaving the program, and leaves on the second Back.
    ///
    /// The question is drawn in the middle of the screen for [`EXIT_WINDOW`];
    /// a Back inside that window ends the program, and a Back after it has
    /// lapsed asks again rather than leaving on a press the viewer has already
    /// forgotten making.
    pub(super) fn arm_exit(&mut self, ctx: &egui::Context) {
        if self.exit_armed.is_some_and(|at| at.elapsed() < EXIT_WINDOW) {
            self.quit(ctx);
            return;
        }
        self.exit_armed = Some(Instant::now());
        // The hint has to be taken down when it lapses, and a wall with no live
        // channel is not repainting on its own.
        ctx.request_repaint_after(EXIT_WINDOW);
    }

    /// Ends the program, once the viewer has confirmed it.
    ///
    /// The configuration is written and the streaming runtime stopped first:
    /// on Android the process is ended outright, and neither would otherwise
    /// happen. Ending the process, rather than only asking the viewport to
    /// close, is what Android needs - the activity's event loop cannot be built
    /// a second time in the same process, so a viewer that came back from a
    /// closed loop could never be started again. On the desktop the viewport is
    /// simply asked to close, and the close request saves and stops the runtime
    /// through the path already there.
    pub(super) fn quit(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "android")]
        {
            let _ = ctx;
            self.save_now();
            self.manager.shutdown();
            // Explicitly, and before the process ends: `exit` runs no
            // destructors at all, so a `Drop` that released the wake request
            // would never be reached and the lock would outlive the program.
            if let Err(err) = power::release() {
                tracing::warn!(target: "xgview::power", %err, "cannot take the wake request back");
            }
            std::process::exit(0);
        }
        #[cfg(not(target_os = "android"))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ---------------------------------------------------------------- events

    /// Android: the floating input box.
    ///
    /// The soft keyboard covers the lower part of a full screen window, and a
    /// window that is full screen is not moved up out of its way. So while a
    /// field is being edited a copy of it is drawn at the top of the screen -
    /// the part that is always in sight - with the caret where the field's own
    /// is. The focus stays on the field and the keystrokes still reach it
    /// through `keyboard::drain`; this only shows what they did, and carries
    /// the buttons that act on the field: select all, copy, paste, clear, done.
    ///
    /// The buttons are for a touch screen or a pointer. A remote has neither,
    /// and while the keyboard is up its arrows belong to the keyboard.
    #[cfg(target_os = "android")]
    pub(super) fn input_overlay(&mut self, ctx: &egui::Context) {
        enum Act {
            SelectAll,
            Copy,
            Paste,
            Clear,
            /// Take the text and put the keyboard away - the only way out of a
            /// multi-line field, whose input method action key is a newline.
            Done,
        }

        let Some(mirror) = ctx.data(|data| data.get_temp::<dialogs::Mirror>(Id::new(dialogs::MIRROR))) else {
            return;
        };
        if self.editing != Some(mirror.id) {
            return;
        }

        let shown: String = if mirror.password {
            "\u{2022}".repeat(mirror.text.chars().count())
        } else {
            mirror.text.clone()
        };
        let range = egui::TextEdit::load_state(ctx, mirror.id).and_then(|state| state.cursor.char_range());
        let selecting = range.is_some_and(|range| range.primary.index != range.secondary.index);
        let text = if selecting {
            // Only select-all makes a selection here, so the whole text is it.
            RichText::new(shown).size(20.0).color(theme::TEXT).background_color(theme::FOCUS_FILL)
        } else {
            let mut chars: Vec<char> = shown.chars().collect();
            let caret = range.map(|range| range.primary.index).unwrap_or(chars.len()).min(chars.len());
            chars.insert(caret, '|');
            RichText::new(chars.into_iter().collect::<String>()).size(20.0).color(theme::TEXT)
        };

        let screen = ctx.screen_rect();
        let mut act = None;
        let area = egui::Area::new(Id::new("xg-input-overlay"))
            .order(egui::Order::Tooltip)
            .fixed_pos(screen.center_top() + vec2(0.0, 12.0))
            .pivot(egui::Align2::CENTER_TOP)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_max_width(screen.width() * 0.8);
                        ui.label(RichText::new(&mirror.label).small().color(theme::TEXT_DIM));
                        ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
                        ui.horizontal(|ui| {
                            if ui.button(monitor_i18n::tr("input-select-all")).clicked() {
                                act = Some(Act::SelectAll);
                            }
                            // A password is not put on the clipboard.
                            if ui.add_enabled(!mirror.password, egui::Button::new(monitor_i18n::tr("input-copy"))).clicked() {
                                act = Some(Act::Copy);
                            }
                            if ui.button(monitor_i18n::tr("input-paste")).clicked() {
                                act = Some(Act::Paste);
                            }
                            if ui.button(monitor_i18n::tr("input-clear")).clicked() {
                                act = Some(Act::Clear);
                            }
                            if ui.button(monitor_i18n::tr("input-done")).clicked() {
                                act = Some(Act::Done);
                            }
                        });
                    });
            });

        // A press on the box is a click elsewhere to the field, which lets go
        // of the focus - and with it the keyboard, a frame later. The box is
        // not a place the viewer left the field for, so the focus is handed
        // straight back.
        let rect = area.response.rect;
        let touched = ctx.input(|input| {
            (input.pointer.any_pressed() || input.pointer.any_released())
                && input.pointer.interact_pos().is_some_and(|pos| rect.contains(pos))
        });
        if touched && ctx.memory(|memory| memory.focused()) != Some(mirror.id) {
            ctx.memory_mut(|memory| memory.request_focus(mirror.id));
        }

        if let Some(act) = act {
            match act {
                Act::SelectAll => crate::keyboard::select_all(),
                Act::Copy => crate::android::set_clipboard_text(&mirror.text),
                Act::Paste => crate::keyboard::paste(crate::android::clipboard_text()),
                Act::Clear => crate::keyboard::clear(),
                Act::Done => {
                    // Confirm the edit the way Enter would on a single-line
                    // field: drop the focus, which drops the keyboard and the
                    // floating box together, and hand it to the next control -
                    // the same move the form makes for a value control that
                    // confirms with Enter - rather than only closing the box.
                    self.editing = None;
                    ctx.memory_mut(|memory| memory.surrender_focus(mirror.id));
                    if let Some(next) = self.nav.next_in_scope(mirror.id) {
                        ctx.memory_mut(|memory| memory.request_focus(next));
                        self.reveal_next = Some(next);
                    }
                }
            }
            ctx.request_repaint();
        }
    }

    /// Whether a control of this kind takes typed text while it has the focus.
    ///
    /// A drag value is one of them: egui hands its keyboard-edit mode a
    /// `TextEdit`, and its caret keys belong to it while it is focused. A
    /// slider is not - it has no text to type into.
    pub(super) fn types(kind: Kind) -> bool {
        matches!(kind, Kind::Text | Kind::DragValue)
    }

    /// Whether a control of this kind raises the Android soft keyboard.
    ///
    /// Only a text field does. A drag value can be typed into on the desktop,
    /// where a keyboard is already at hand, but on Android it is stepped by
    /// dragging or with the remote's Up / Down; raising the keyboard for it
    /// would only cover the box, and unlike a text field it leaves no copy for
    /// the floating input box to mirror.
    #[cfg(target_os = "android")]
    pub(super) fn soft_keyboard(kind: Kind) -> bool {
        matches!(kind, Kind::Text)
    }

    /// Whether a text field is being typed into, which is what the soft
    /// keyboard follows.
    ///
    /// The navigation layer answers it: every text field in the application
    /// registers itself as [`Kind::Text`], and nothing else does. The obvious
    /// test - whether a [`egui::text_edit::TextEditState`] sits under the
    /// focused id - is wrong, and wrong for the rest of the process: egui keeps
    /// that state *persisted*, so an id that belonged to a field once keeps
    /// answering yes for whatever else is drawn with it later. That is how the
    /// keyboard came up on a television when the remote reached the transport
    /// button of the first camera row.
    pub(super) fn typing(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.focused())
            .is_some_and(|focused| Self::types(self.nav.kind(focused)))
    }

    /// The room to leave before a row of buttons to put it in the middle.
    ///
    /// egui hands a nested row the full width and lays its widgets out from the
    /// left, so a row that wants to be centred measures itself and pads its

    /// left side. The button labels are the only thing that decides the width:
    /// button padding is the theme's, and a label wider than the minimum size
    /// is the usual case here.
    pub(super) fn center_offset(ui: &egui::Ui, labels: &[&str]) -> f32 {
        let padding = ui.spacing().button_padding.x * 2.0;
        let font = egui::TextStyle::Button.resolve(ui.style());
        let mut total = 0.0;
        for label in labels {
            total += ui.fonts(|fonts| {
                fonts
                    .layout_no_wrap((*label).to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                    .size()
                    .x
            }) + padding;
        }
        total += ui.spacing().item_spacing.x * labels.len().saturating_sub(1) as f32;
        ((ui.available_width() - total) * 0.5).max(0.0)
    }

    /// Takes Back out of the queue, if it was pressed.
    ///
    /// Escape on a keyboard, `BrowserBack` from an Android remote. Backspace is
    /// left alone: it belongs to a field's caret, and the alternative owner of
    /// Back only claims it when no field is being typed into.
    pub(super) fn back_pressed(ctx: &egui::Context) -> bool {
        ctx.input_mut(|input| {
            let none = Modifiers::NONE;
            input.consume_key(none, Key::Escape) || input.consume_key(none, Key::BrowserBack)
        })
    }

    /// Back as a window reads it: Escape and BrowserBack always, and Backspace
    /// too while no text field is being typed into.
    ///
    /// The wall reads Back as those three keys as well, so a remote or keyboard
    /// whose Back arrives as Backspace has to be able to leave a window the same
    /// way it leaves the panel - otherwise the press falls through to the panel
    /// behind and closes that instead. A field with the focus keeps Backspace
    /// for its own text.
    pub(super) fn dialog_back_pressed(&self, ctx: &egui::Context) -> bool {
        if Self::back_pressed(ctx) {
            return true;
        }
        !self.typing(ctx)
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Backspace))
    }

    /// Whether this frame carries input that is not a Back press.
    ///
    /// Used to cancel the exit question: a viewer who presses something else has
    /// answered it by doing something else. Every key the wall answers Back to
    /// is a Back here and not an interruption - Escape, the remote's
    /// `BrowserBack`, and Backspace, which is the Back a keyboard offers and
    /// what the wall already reads as one. Movement of a pointer is not input
    /// in that sense either: a mouse nudged on a desk, or a finger resting on a
    /// touch screen, must not take the question down.
    pub(super) fn input_other_than_back(ctx: &egui::Context) -> bool {
        ctx.input(|input| {
            input.events.iter().any(|event| match event {
                egui::Event::Key { key, pressed: true, .. } => {
                    !matches!(key, Key::Escape | Key::BrowserBack | Key::Backspace)
                }
                egui::Event::PointerButton { pressed: true, .. } => true,
                egui::Event::Text(_) | egui::Event::MouseWheel { .. } => true,
                _ => false,
            })
        })
    }

    /// Routes one frame of keys to whoever owns them.
    ///
    /// The four directions, Enter and Escape have two possible owners, and the
    /// choice is made by where the remote's focus was when the frame began -
    /// which is the only thing the viewer can see. A widget of the bars, the
    /// settings panel or a window owning them means egui's own focus walk does
    /// the work; it has already run by the time the application is called, and
    /// it runs on the very events read here, which is why they are *not* taken
    /// out of the queue in that case. The wall is not made of widgets, so it
    /// answers the same keys itself.
    /// Keyboard handler for the combined-info picker modal.  Called from
    /// `handle_keys` so the arrows are consumed before the nav layer or the
    /// wall can act on them.
    pub(super) fn handle_combined_info_keys(&mut self, ctx: &egui::Context) {
        let none = Modifiers::NONE;
        let back = ctx.input_mut(|input| {
            input.consume_key(none, Key::Escape)
                || input.consume_key(none, Key::BrowserBack)
                || input.consume_key(none, Key::Backspace)
        });
        if back {
            self.combined_info_picker.open = false;
            self.restore_focus = self.combined_info_picker.from.take();
            return;
        }

        let slot = self.combined_info_picker_slot;
        let mut selected = self.config.combined_info[slot].clone();
        let focus_id = Id::new("xgview-combined-info-focus");
        let (mut focus_col, mut focus_idx): (usize, usize) =
            ctx.data(|d| d.get_temp(focus_id).unwrap_or((0, 0)));

        let available: Vec<OsdItem> = OsdItem::ATOMIC
            .iter()
            .filter(|atom| !selected.contains(atom))
            .copied()
            .collect();

        if focus_col == 0 && available.is_empty() && !selected.is_empty() {
            focus_col = 1;
            focus_idx = 0;
        }
        if focus_col == 1 && selected.is_empty() && !available.is_empty() {
            focus_col = 0;
            focus_idx = 0;
        }
        let col_len = if focus_col == 0 { available.len() } else { selected.len() };
        if focus_idx >= col_len {
            focus_idx = col_len.saturating_sub(1);
        }

        let (up, down, left, right, activate) = ctx.input_mut(|i| (
            i.consume_key(none, Key::ArrowUp),
            i.consume_key(none, Key::ArrowDown),
            i.consume_key(none, Key::ArrowLeft),
            i.consume_key(none, Key::ArrowRight),
            i.consume_key(none, Key::Enter) || i.consume_key(none, Key::Space),
        ));

        let mut changed = false;
        if up && focus_idx > 0 {
            focus_idx -= 1;
        }
        if down {
            let len = if focus_col == 0 { available.len() } else { selected.len() };
            if focus_idx + 1 < len {
                focus_idx += 1;
            }
        }
        if left && focus_col == 1 && !available.is_empty() {
            focus_col = 0;
            focus_idx = focus_idx.min(available.len() - 1);
        }
        if right && focus_col == 0 && !selected.is_empty() {
            focus_col = 1;
            focus_idx = focus_idx.min(selected.len() - 1);
        }
        if activate {
            if focus_col == 0 && focus_idx < available.len() {
                let atom = available[focus_idx];
                selected.push(atom);
                changed = true;
                if available.len() > 1 {
                    if focus_idx >= available.len() - 1 {
                        focus_idx = available.len() - 2;
                    }
                } else {
                    focus_col = 1;
                    focus_idx = selected.len() - 1;
                }
            } else if focus_col == 1 && focus_idx < selected.len() {
                selected.remove(focus_idx);
                changed = true;
                if selected.is_empty() {
                    focus_col = 0;
                    focus_idx = focus_idx.min(available.len().saturating_sub(1));
                } else if focus_idx >= selected.len() {
                    focus_idx = selected.len() - 1;
                }
            }
        }

        ctx.data_mut(|d| d.insert_temp(focus_id, (focus_col, focus_idx)));
        if changed {
            self.config.combined_info[slot] = selected;
            self.mark_dirty();
        }
    }

    pub(super) fn handle_keys(&mut self, ctx: &egui::Context) {
        // The file dialog owns the keyboard while it is up. Its own keybindings
        // and egui's focus walk are how a viewer moves around it, and the arrows
        // and Back taken out of the queue below would reach neither; it is left
        // to close itself, by its Cancel button or the Esc it reads.
        //
        // Read as the frame began rather than now: a Back that has just closed
        // the dialog is the dialog's to answer, and it is the same press that a
        // `file_dialog_open` read here would find still in the queue and hand to
        // the wall - closing the panel the dialog was opened from.
        if self.file_dialog_keys {
            return;
        }

        // The combined-info picker handles its own keyboard: arrows move the
        // selection within and between columns, Enter moves an item, Back
        // closes.  All consumed here so the rest of `handle_keys` — the wall,
        // the nav walk — does not also act on them.
        if self.combined_info_picker.open {
            self.handle_combined_info_keys(ctx);
            return;
        }

        // Android: a field's edit confirmation is dropped the moment the focus
        // leaves it - whether by an arrow, a click, or Back. Without this, a
        // field confirmed once would raise the keyboard again on re-entering
        // it, and a walk from one field to the next would carry the keyboard
        // along. `set_wanted` already gates on `editing == focused`, so a stale
        // value would not raise the keyboard on the wrong field, but clearing
        // it here keeps the state honest.
        #[cfg(target_os = "android")]
        {
            let focused = ctx.memory(|memory| memory.focused());
            if self.editing.is_some_and(|e| focused != Some(e)) {
                self.editing = None;
            }
        }

        // The question the first Back asks on the wall is answered by a second
        // Back and by nothing else. Any *input* between the two presses - a
        // direction, a click, the key that opens a panel - was the viewer doing
        // something else, and calls it off.
        //
        // Input, and not the frame: the wall repaints several times a second on
        // its own, and a rule that cancelled on a frame would take the question
        // down before it could be read.
        if self.exit_armed.is_some() && Self::input_other_than_back(ctx) {
            self.exit_armed = None;
        }

        // A window this application opened is the innermost layer: Back closes
        // it, whatever inside it has the focus. A field's caret must not hold it
        // open - Back on the first field would otherwise hand the focus back to
        // that same field, and the window could never be left.
        // A prompt whose passphrase is being stretched is the exception: it is
        // the one thing on this side of the answer that cannot be backed out of,
        // because the answer would then have nowhere to go.
        if ((self.passphrase.as_ref().is_some_and(|prompt| !prompt.working))
            || self.camera_edit.is_some()
            || self.remove_confirm.is_some()
            || self.system_confirm.is_some())
            && self.dialog_back_pressed(ctx)
        {
            if self.passphrase.take().is_some() {
                self.passphrase_anchor = None;
                // Backing out of it leaves the export or import unasked for, and
                // the button behind the prompt takes the focus again.
                self.restore_focus = self.file_dialog_anchor.take().or(self.restore_focus);
            } else if let Some(confirm) = self.remove_confirm.take() {
                self.remove_confirm_anchor = None;
                self.restore_focus = confirm.from;
            } else if let Some(confirm) = self.system_confirm.take() {
                self.system_confirm_anchor = None;
                self.restore_focus = confirm.from;
            } else if let Some(edit) = self.camera_edit.take() {
                self.camera_edit_anchor = None;
                self.restore_focus = edit.from;
            }
            return;
        }

        // A text field being typed into owns the letters, the paste and the
        // caret keys. Two kinds of key are taken away from it here.
        //
        // Back, because it is the remote control's only way out of a field: it
        // leaves the field and not the panel - one step - and the press after it
        // closes the panel.
        //
        // And the four directions, because a form with a field in the middle has
        // to be walkable past it. `TextEdit` reads its input through
        // `filtered_events`, so taking the arrows out of the queue before the
        // field is drawn keeps it from spending them on its caret. The focus
        // still moves: egui read the same events for its focus walk in
        // `begin_pass`, which happens before the keys are handed out.
        //
        // This is deliberately *not* `Context::wants_keyboard_input`, which is
        // true for any focused widget at all. The bars hand the focus to a
        // button whenever a panel opens, so asking that question would leave a
        // viewer unable to press anything - F1, F2, BACK, anything - from the
        // moment a panel appeared.
        //
        // A control on the navigation layer is the exception: its arrows are
        // answered by [`crate::nav`], a field among them included, because a
        // field in the middle of a form still has to be walkable past. Only the
        // keys that are not the arrows - the letters, Back - fall through to
        // the field.
        let nav_owns =
            ctx.memory(|memory| memory.focused()).is_some_and(|focused| self.nav.owns(focused));
        // What the focused control keeps for itself: a slider is moved with
        // Left and Right, a drag value with Up and Down, and only the other
        // axis moves the focus. A plain control keeps neither.
        let nav_kind = if nav_owns {
            ctx.memory(|memory| memory.focused()).map(|focused| self.nav.kind(focused))
        } else {
            None
        };
        // The layer already knows what kind of control it focused, so a field
        // is one that registered as a kind that takes typed text - the same
        // answer `typing` gives at the end of the frame.
        let typing = nav_kind.is_some_and(Self::types);
        if typing {
            // Android: the soft keyboard is not raised by focus alone. Enter on
            // a text field that is not yet being edited confirms it - `editing`
            // takes the focus and the keyboard comes up next frame; the Enter
            // itself is consumed so `TextEdit` does not read it as a submit. A
            // drag value is left out: it is stepped, not typed into, on Android.
            // Back on a field that is being edited drops the keyboard but keeps
            // the focus, so the viewer can read the field and re-confirm it.
            // Back on a field that is not being edited falls through to `leave`
            // below and walks the focus out, as on the desktop. A touch click is
            // handled at the end of the frame, where it can be told apart from a
            // DPAD walk.
            #[cfg(target_os = "android")]
            {
                let focused = ctx.memory(|memory| memory.focused());
                let none = Modifiers::NONE;
                if self.editing == focused {
                    if ctx.input_mut(|input| {
                        input.consume_key(none, Key::Escape)
                            || input.consume_key(none, Key::BrowserBack)
                    }) {
                        self.editing = None;
                        return;
                    }
                } else if nav_kind.is_some_and(Self::soft_keyboard)
                    && ctx.input_mut(|input| input.consume_key(none, Key::Enter))
                {
                    self.editing = focused;
                    return;
                }
            }
            // Back leaves the field; the arrows walk the form. Where a field
            // keeps its caret keys for itself - the bars and the wall - the
            // arrows are taken out of the queue here, as before; on the
            // navigation layer they are left for the layer to move the focus.
            let leave = ctx.input_mut(|input| {
                let none = Modifiers::NONE;
                let leave = input.consume_key(none, Key::Escape)
                    || input.consume_key(none, Key::BrowserBack);
                if !nav_owns {
                    for key in [Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown] {
                        input.consume_key(none, key);
                    }
                }
                leave
            });
            if leave {
                if let Some(focused) = ctx.memory(|memory| memory.focused()) {
                    ctx.memory_mut(|memory| memory.surrender_focus(focused));
                }
                return;
            }
            if !nav_owns {
                return;
            }
        }
        let grid_owns = self.owner == Owner::Grid;
        let keys = ctx.input_mut(|input| {
            let none = Modifiers::NONE;
            let mut keys = Keys::default();
            // Read before the press is consumed: `consume_key` matches key
            // repeats too, and a repeated Enter must not press the magnified
            // viewport's bar over and over. See `Keys::enter_fresh`.
            keys.enter_fresh = input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key { key: Key::Enter | Key::Space, pressed: true, repeat: false, .. }
                )
            });
            // Back is Escape on a keyboard and `BrowserBack` from an Android
            // remote: winit maps `AKEYCODE_BACK` onto that, not onto Escape, so
            // a remote that only ever sends BACK would otherwise have no way out
            // of anything.
            if grid_owns {
                keys.left = input.consume_key(none, Key::ArrowLeft);
                keys.right = input.consume_key(none, Key::ArrowRight);
                keys.up = input.consume_key(none, Key::ArrowUp);
                keys.down = input.consume_key(none, Key::ArrowDown);
                keys.enter = input.consume_key(none, Key::Enter) || input.consume_key(none, Key::Space);
                keys.back = input.consume_key(none, Key::Escape)
                    || input.consume_key(none, Key::Backspace)
                    || input.consume_key(none, Key::BrowserBack);
            } else {
                // A control of the navigation layer has its arrows taken out of
                // the queue here: the layer decides where they go, and the
                // widget must not also read them. Everything else is left in
                // the queue for egui's own walk, which ran already.
                if nav_owns {
                    // On a remote Up and Down always move the focus. The two
                    // value controls are adjusted sideways instead: the slider
                    // reads the sideways keys itself, and a drag value has them
                    // recorded on the layer for the widget to apply as it is
                    // drawn (see `controls::drag_value`), because it steps with
                    // Up / Down and those are the focus's.
                    match nav_kind {
                        Some(Kind::Slider) => {
                            keys.left = input.key_pressed(Key::ArrowLeft);
                            keys.right = input.key_pressed(Key::ArrowRight);
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                        }
                        Some(Kind::DragValue) | Some(Kind::Cycle) => {
                            // Taken so the field's caret does not move on them;
                            // the value change happens at the widget.
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                            keys.left = input.consume_key(none, Key::ArrowLeft);
                            keys.right = input.consume_key(none, Key::ArrowRight);
                        }
                        Some(Kind::Text) => {
                            // The caret is the field's, so the sideways keys are
                            // left in the queue for it. The vertical ones still
                            // walk the form.
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                        }
                        _ => {
                            // A plain control: all four arrows move the focus.
                            keys.left = input.consume_key(none, Key::ArrowLeft);
                            keys.right = input.consume_key(none, Key::ArrowRight);
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                        }
                    }
                } else {
                    keys.left = input.key_pressed(Key::ArrowLeft);
                    keys.right = input.key_pressed(Key::ArrowRight);
                    keys.up = input.key_pressed(Key::ArrowUp);
                    keys.down = input.key_pressed(Key::ArrowDown);
                }
                keys.enter = input.key_pressed(Key::Enter) || input.key_pressed(Key::Space);
                // Backspace is the field's own when a field has the focus: it
                // deletes a character rather than unwinding a window.
                keys.back = input.key_pressed(Key::Escape)
                    || (!typing && input.key_pressed(Key::Backspace))
                    || input.key_pressed(Key::BrowserBack);
            }
            // The rest belong to the application whatever has the focus, and
            // are consumed so that nothing downstream acts on them as well.
            keys.settings = input.consume_key(none, Key::F1);
            keys.devices = input.consume_key(none, Key::F2);
            keys.fullscreen = input.consume_key(none, Key::F11);
            // The layout digits and the page keys are the exception: a field
            // being typed into keeps them. An address is made of the digits -
            // `192.168.1.4` - so consuming them changes the wall's shape under a
            // viewer who was only naming a camera. The page keys are not text,
            // but a page turning behind a dialog is the same surprise.
            if !typing {
                keys.page_prev = input.consume_key(none, Key::PageUp);
                keys.page_next = input.consume_key(none, Key::PageDown);
                keys.layout = [
                    input.consume_key(none, Key::Num1),
                    input.consume_key(none, Key::Num2),
                    input.consume_key(none, Key::Num3),
                    input.consume_key(none, Key::Num4),
                    input.consume_key(none, Key::Num5),
                    input.consume_key(none, Key::Num6),
                    input.consume_key(none, Key::Num7),
                    input.consume_key(none, Key::Num8),
                    input.consume_key(none, Key::Num9),
                    input.consume_key(none, Key::Num0),
                ];
            }
            keys
        });

        // The focus ring is armed here, before any of these keys is acted on:
        // what arms it is the keys that walk it, whatever else they end up
        // doing. See `ring_keys`.
        if Self::ring_keys(&keys, self.owner == Owner::Grid) {
            self.ring_seen = Some(Instant::now());
            self.ring_aimed = true;
        }

        // The magnified viewport's bar goes when the viewer stops using it, not
        // eight seconds after it opened: every key it answers keeps its clock
        // running. This is done here, before any of the branches below can
        // return - the bar's own keys are the navigation layer's, and that path
        // leaves this function early.
        if self.tile_bar.is_open()
            && (keys.left || keys.right || keys.up || keys.down || keys.enter || keys.back)
        {
            self.tile_bar.touch(Instant::now());
        }

        // The reorder mode owns Back: it drops the arrangement and nothing
        // else, so a press that means "never mind" cannot also close the panel
        // behind it.
        if keys.back && self.reorder.is_some() {
            self.cancel_reorder();
            return;
        }


        // Application keys first: they are how a keyboard, and whatever button
        // a remote offers beside the DPAD, reach the panels at all.
        // A dialog owns the screen while it is up: nothing behind it answers the
        // pointer, and the keys that open a panel are spent the same way, so one
        // dialog can never be stacked on another.
        let dialog_open = self.discovery.open
            || self.camera_edit.is_some()
            || self.remove_confirm.is_some()
            || self.system_confirm.is_some()
            || self.passphrase.is_some();
        // The layout picker is a modal of its own, and it owns the keys while
        // it is up: opening a panel behind it would stack two of them.
        if keys.settings && !dialog_open && !self.layout_picker.open && !self.portrait_aspect_picker.open && !self.combined_info_picker.open && !self.is_portrait() {
            self.show_settings = !self.show_settings;
        }
        if keys.devices && !dialog_open && !self.layout_picker.open && !self.portrait_aspect_picker.open && !self.combined_info_picker.open {
            self.discovery.open = !self.discovery.open;
            // Opened from the keyboard, so there is no button to hand the focus
            // back to when it closes.
            self.discovery.from = None;
        }
        if keys.fullscreen {
            let enabled = !self.fullscreen;
            self.set_fullscreen(ctx, enabled);
        }
        // A key that opens a panel, or changes the shape of the wall, is not the
        // magnified viewport's bar's: it takes the bar down before it acts, so
        // the two are never up at once. See [`crate::tilebar`].
        if keys.settings
            || keys.devices
            || keys.fullscreen
            || keys.page_prev
            || keys.page_next
            || keys.layout.iter().any(|pressed| *pressed)
        {
            self.tile_bar.hide();
        }
        // The picker is a modal: while it is up the wall's own keys - page
        // turns and the layout digits - are spent, so a press cannot change the
        // wall behind it. Back is answered below, where the panels are unwound.
        if !self.layout_picker.open && !self.portrait_aspect_picker.open && !self.combined_info_picker.open {
            // A page key turns the page from wherever the focus is, but the
            // bounce a turn at the edge makes - to the other page button -
            // belongs to a focus that was standing on one of those buttons: the
            // turn is what took the button it stood on away. A tile's focus, or
            // a focus anywhere else, turns the page and stays put.
            let on_page_button = self.page_button_has_focus(ctx);
            if keys.page_prev {
                self.advance(false, on_page_button);
            }
            if keys.page_next {
                self.advance(true, on_page_button);
            }
            for (slot, pressed) in keys.layout.iter().enumerate() {
                if !*pressed {
                    continue;
                }
                if self.is_portrait() {
                    // Upright, the digits pick the four stacked grids.
                    if let Some(layout) = GridLayout::PORTRAIT.get(slot) {
                        self.set_layout(*layout);
                    }
                } else {
                    // The digit is the viewport count; the last slot is `0`,
                    // which stands for the ten-cell layout. 12, 13 and 16 have
                    // no digit.
                    let digit = if slot == 9 { 0 } else { slot as u8 + 1 };
                    if let Some(layout) = GridLayout::for_digit(digit) {
                        self.set_layout(layout);
                    }
                }
            }
        }

        // A control of the navigation layer: the arrow moves by declaration -
        // the scope's order, then its exits - and not by geometry. Answered
        // before the Back handling below, so that Back still unwinds a window.
        if nav_owns {
            // A value control - a drag value, or the language button - takes
            // its step from the sideways presses recorded here; the widget
            // applies it as it is drawn this frame. Left steps down and Right
            // up, the way a slider reads them.
            if matches!(nav_kind, Some(Kind::DragValue) | Some(Kind::Cycle)) {
                if keys.left {
                    self.nav.step_value(-1);
                }
                if keys.right {
                    self.nav.step_value(1);
                }
            }
            // Up and Down always move the focus. A value control - slider, drag
            // value, text field or cycling button - answers its sideways keys
            // itself, so they never reach here; a plain control walks with all
            // four.
            let dir = match nav_kind {
                Some(Kind::Slider) | Some(Kind::DragValue) | Some(Kind::Text) | Some(Kind::Cycle) => {
                    if keys.up {
                        Some(Dir::Up)
                    } else if keys.down {
                        Some(Dir::Down)
                    } else {
                        None
                    }
                }
                _ => {
                    if keys.up {
                        Some(Dir::Up)
                    } else if keys.down {
                        Some(Dir::Down)
                    } else if keys.left {
                        Some(Dir::Left)
                    } else if keys.right {
                        Some(Dir::Right)
                    } else {
                        None
                    }
                }
            };
            if let (Some(dir), Some(focused)) = (dir, ctx.memory(|memory| memory.focused())) {
                match self.nav.step(focused, dir) {
                    Some(target) => {
                        ctx.memory_mut(|memory| memory.request_focus(target));
                    }
                    None => {
                        if dir == Dir::Down && self.nav.scope_of(focused) == Some("settings-body") {
                            // The press had nowhere to move the focus, which is
                            // the case at the end of a tab: what follows the last
                            // control - the adb line, the table of keys - is text
                            // and has no control to be walked onto. A remote has
                            // no wheel, so the press moves the body instead.
                            // Asked for here rather than applied here: only a
                            // scroll area that is being drawn reads a scroll
                            // request, and this runs before anything is drawn.
                            let points = ctx.screen_rect().height() * REMOTE_SCROLL_FRACTION;
                            self.body_scroll = Some(points);
                        }
                    }
                }
            }
        }

        if !grid_owns {
            // The magnified viewport's bar opens on its seat, which is not one
            // of its controls: the first arrow is what steps off it, onto the
            // first control - the handset while a call is up - and no arrow ever
            // walks back onto it. See `tilebar::seat`.
            if self.tile_bar.is_open()
                && ctx.memory(|memory| memory.focused()) == Some(tilebar::seat())
                && (keys.left || keys.right || keys.up || keys.down)
            {
                ctx.memory_mut(|memory| memory.request_focus(tilebar::control(0)));
            }
            // egui has walked the focus already - and dropped it, if this was
            // Escape. Back is therefore one step out of the controls and not
            // two: a panel opened over the wall closes, and otherwise the wall
            // takes the keys back.
            if keys.back {
                #[cfg(juanz)]
                if crate::juanz::close(ctx) {
                    return;
                }
                // Back unwinds one layer at a time, and the magnified viewport's
                // bar is a layer: it goes first, before the panels behind it and
                // long before the magnification. The focus goes back to the wall
                // with it, or the next press would be read by nothing at all.
                //
                // A call is a layer of its own, above the bar: Back ends it, the
                // same as the handset, and leaves the picture where it is.
                if self.talking {
                    self.set_talking(false);
                } else if self.tile_bar.is_open() {
                    self.tile_bar.hide();
                    self.leave_controls(ctx);
                } else if self.layout_picker.open {
                    self.layout_picker.open = false;
                    self.restore_focus = self.layout_picker.from.take();
                } else if self.portrait_aspect_picker.open {
                    self.portrait_aspect_picker.open = false;
                    self.restore_focus = self.portrait_aspect_picker.from.take();
                } else if self.discovery.open {
                    self.discovery.open = false;
                    self.restore_focus = self.discovery.from.take();
                } else if self.show_settings {
                    self.show_settings = false;
                } else {
                    self.leave_controls(ctx);
                }
            } else if keys.down && !self.show_settings && self.toolbar_has_focus(ctx) {
                // Below the bar there is only the wall, so the arrow that walks
                // off the bottom of it hands the keys back.
                self.leave_controls(ctx);
            } else if keys.up && self.status_has_focus(ctx) {
                // The status row is below the wall, so the arrow back to the
                // wall from it is Up, the mirror of the one that came down.
                self.leave_controls(ctx);
            }
            if keys.enter {
                let focused = ctx.memory(|memory| memory.focused());
                if focused == Some(Id::new("portrait-prev-page")) {
                    self.advance(false, true);
                } else if focused == Some(Id::new("portrait-next-page")) {
                    self.advance(true, true);
                }
            }
            return;
        }

        if self.scheduler.is_zoomed() {
            // The strip owns the keys only where the remote is in it: its
            // controls are egui's own buttons on the navigation layer, and the
            // arrows walk them. A strip that is merely up - the upright default,
            // or one a tap put there - leaves the remote on the wall, so the
            // arrows keep stepping channels and the confirm key is what walks
            // into the strip.
            let bar_owns_keys = self.tile_bar.is_open()
                && ctx.memory(|memory| memory.focused()).is_some_and(tilebar::owns);
            if !bar_owns_keys {
                // A magnified viewport is one channel, not a grid: Right and
                // Left are the next and previous channel in channel order, and
                // Up and Down have no grid to walk, so they go straight to the
                // bars.
                if keys.left {
                    self.step_zoom(false);
                }
                if keys.right {
                    self.step_zoom(true);
                }
                if keys.down {
                    self.hand_off(ctx, Bar::Status);
                }
                if keys.up {
                    self.hand_off(ctx, Bar::Toolbar);
                }
                // The confirm key brings the viewport's bar up rather than
                // unwinding the magnification: Back is the way out now, a layer
                // at a time. A held key is not a second press. The bar appears
                // without its selection drawn - the focus is put on its seat, not
                // on a control - and the first arrow is what lights it. A bar
                // that is already up is entered the same way, without being
                // shown again.
                //
                // It comes up along the bottom: the remote asks for it without
                // pointing at an edge, and the status side is where the hand and
                // the eye expect a strip of controls.
                if keys.enter && keys.enter_fresh {
                    if !self.tile_bar.is_open() {
                        self.tile_bar.show(tilebar::Dock::Bottom, Instant::now());
                    }
                    ctx.memory_mut(|memory| memory.request_focus(tilebar::seat()));
                }
            }
        } else {
            if keys.left {
                self.navigate(Direction::Left);
            }
            if keys.right {
                self.navigate(Direction::Right);
            }
            if keys.down && self.navigate(Direction::Down) == NavigateOutcome::Blocked {
                self.hand_off(ctx, Bar::Status);
            }
            if keys.up && self.navigate(Direction::Up) == NavigateOutcome::Blocked {
                self.hand_off(ctx, Bar::Toolbar);
            }
            if keys.enter {
                self.zoom_in();
            }
        }
        if keys.back {
            self.back(ctx);
        }
    }
}
