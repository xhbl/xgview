//! The four windows the application opens over the wall: the camera form, two
//! confirmations and the passphrase prompt.

use super::*;

impl XgViewApp {
    /// The window one camera of the settings panel is edited in.
    ///
    /// It lays out the same form as the "Manual entry" tab - see
    /// [`dialogs::camera_fields`] - and writes back under the identifier of the
    /// entry it is editing, so a changed url repoints the camera instead of
    /// adding a second one. Returns `true` when the configuration changed.
    pub(super) fn camera_edit_window(&mut self, ctx: &egui::Context) -> bool {
        self.modal_frame(
            ctx,
            "xgview-edit-camera",
            560.0,
            |app| &mut app.camera_edit,
            |app, ui, edit| {
                // The form edits the camera where it lives, so the heading names
                // the group it is in - when there is more than one to tell apart.
                let group = app
                    .config
                    .cameras
                    .get(edit.slot)
                    .map(|camera| usize::from(camera.group))
                    .unwrap_or(0);
                let title = dialogs::group_heading(
                    &app.config,
                    group,
                    "camera-edit-title",
                    "camera-edit-title-group",
                );
                // The form reads the entry to decide what it draws - greyed
                // addresses for an imported camera, and no "infer sub" offer on a
                // restream. It is read afresh each frame, so pressing Detach
                // unlocks it on the spot.
                let camera = app.config.cameras.get(edit.slot);

                dialogs::title_bar(ui, &title, false, "");
                app.nav.open("edit-body");
                let fields =
                    dialogs::camera_fields(ui, &mut app.nav, "edit-camera", &mut edit.draft, camera);
                // The dialog opens on the first control of the form.
                app.camera_edit.anchor = fields.first;
                if fields.inferred {
                    ui.label(
                        RichText::new(monitor_i18n::tr("camera-edit-sub-derived"))
                            .small()
                            .color(theme::LIVE),
                    );
                }
                ui.add_space(theme::space::M);
                let mut save = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    let save_label = monitor_i18n::tr("save");
                    let cancel_label = monitor_i18n::tr("cancel");
                    ui.add_space(Self::center_offset(ui, &[&save_label, &cancel_label]));
                    if app.nav.tracked(ui.button(save_label)).clicked() {
                        save = true;
                    }
                    if app.nav.tracked(ui.button(cancel_label)).clicked() {
                        cancel = true;
                    }
                });
                app.nav.close();

                if fields.test {
                    match edit.draft.build() {
                        Ok(source) => app.spawn_test_tone(source),
                        Err(error) => app.flash(error, ToastKind::Error),
                    }
                }

                if fields.detach {
                    // The camera leaves the import's hands: its MSE address and
                    // login go with the mark, so its addresses can be edited by
                    // hand without one falling out of step. The window stays
                    // open, now unlocked.
                    if let Some(camera) = app.config.cameras.get_mut(edit.slot) {
                        camera.detach();
                    }
                    app.needs_sync = true;
                    return Answer::STAY_CHANGED;
                }

                if save {
                    match edit.draft.build() {
                        Ok(source) => {
                            // Only what the form shows is written back. The entry
                            // keeps its identifier, its origin, its transport and
                            // everything else the form has no field for - the
                            // display aspect among them - so an edit to a name or
                            // a url cannot reset them.
                            match app.config.cameras.get_mut(edit.slot) {
                                Some(camera) => camera.apply_edit(&source),
                                None => {
                                    app.config.upsert_camera(source);
                                }
                            }
                            app.needs_sync = true;
                            // The focus goes back to the button the window was
                            // opened from, not to the first control of the panel -
                            // and only when that button was reached with the
                            // keyboard or a remote. See `Modal::from`.
                            return Answer::CLOSE_CHANGED;
                        }
                        Err(error) => app.flash(error, ToastKind::Error),
                    }
                }

                if cancel {
                    Answer::CLOSE
                } else {
                    Answer::STAY
                }
            },
        )
        .changed
    }

    /// The window the settings panel asks about removing a camera in.
    ///
    /// A window rather than a question drawn into the row: a row that turns into
    /// its own question can be walked past and left that way, while a window has
    /// one way out. Returns `true` when the camera was removed.
    pub(super) fn remove_confirm_window(&mut self, ctx: &egui::Context) -> bool {
        let answer = self.modal_frame(
            ctx,
            "xgview-remove-camera",
            380.0,
            |app| &mut app.remove_confirm,
            |app, ui, confirm| {
                // The heading names the group the camera is leaving when there is
                // more than one to tell apart, as the edit window's names the
                // group it is in: two groups can hold a camera of the same name.
                let group = app
                    .config
                    .cameras
                    .get(confirm.slot)
                    .map(|camera| usize::from(camera.group))
                    .unwrap_or(0);
                let title = dialogs::remove_title(&app.config, group, &confirm.name);

                // The question is its own column of the navigation layer, as the
                // edit window is: without it the two answers are not registered
                // at all, and the arrows are left to egui's geometric walk -
                // which the panel behind then takes away from them.
                app.nav.open("confirm-body");
                dialogs::title_bar(ui, &title, false, "");
                ui.label(
                    RichText::new(monitor_i18n::tr("remove-hint"))
                        .small()
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(theme::space::M);
                let mut remove = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    let keep_label = monitor_i18n::tr("action-keep");
                    let remove_label = monitor_i18n::tr("action-remove");
                    ui.add_space(Self::center_offset(ui, &[&keep_label, &remove_label]));
                    let keep_button = app.nav.tracked(ui.button(keep_label));
                    // The dialog opens on the answer that changes nothing.
                    app.remove_confirm.anchor = Some(keep_button.id);
                    if keep_button.clicked() {
                        cancel = true;
                    }
                    if app
                        .nav
                        .tracked(ui.button(RichText::new(remove_label).color(theme::ERROR)))
                        .clicked()
                    {
                        remove = true;
                    }
                });
                app.nav.close();

                if remove && confirm.slot < app.config.cameras.len() {
                    app.config.cameras.remove(confirm.slot);
                    // The row is gone, so there is no button to hand the focus
                    // back to; the panel anchor picks it up.
                    app.needs_sync = true;
                    return Answer::CLOSE_LEAVING_THE_FOCUS;
                }
                if cancel {
                    Answer::CLOSE
                } else {
                    Answer::STAY
                }
            },
        );
        answer.changed
    }

    /// The reboot confirmation window.
    ///
    /// A modal, like the remove window: centred, and nothing behind it
    /// answers. Its own buttons (or Back) are the only way out, and the dialog
    /// opens on the answer that changes nothing.
    pub(super) fn system_confirm_window(&mut self, ctx: &egui::Context) {
        if self.system_confirm.take().is_none() {
            return;
        }
        let from = self.system_confirm.from;
        let mut confirm = false;
        let mut cancel = false;

        egui::Modal::new(Id::new("xgview-system-confirm")).show(ctx, |ui| {
            self.nav.open("confirm-body");
            ui.set_width(380.0);
            dialogs::title_bar(ui, &monitor_i18n::tr("reboot-confirm-title"), false, "");
            ui.label(
                RichText::new(monitor_i18n::tr("reboot-confirm-hint"))
                    .small()
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::space::M);
            ui.horizontal(|ui| {
                let cancel_label = monitor_i18n::tr("action-cancel");
                let reboot_label = monitor_i18n::tr("action-reboot");
                ui.add_space(Self::center_offset(ui, &[&cancel_label, &reboot_label]));
                let cancel_button = self.nav.tracked(ui.button(cancel_label));
                // The dialog opens on the answer that changes nothing.
                self.system_confirm.anchor = Some(cancel_button.id);
                if cancel_button.clicked() {
                    cancel = true;
                }
                if self
                    .nav
                    .tracked(ui.button(RichText::new(reboot_label).color(theme::ERROR)))
                    .clicked()
                {
                    confirm = true;
                }
            });
            self.nav.close();
        });

        if confirm {
            self.system_confirm.close();
            // Save first: a reboot ends the process, and the debounced save
            // would otherwise never happen.
            self.save_now();
            match reboot::reboot() {
                Ok(()) => self.quit(ctx),
                Err(err) => {
                    self.flash(
                        monitor_i18n::tr_args(
                            "toast-reboot-failed",
                            &[("error", err.to_string().into())],
                        ),
                        ToastKind::Error,
                    );
                    // The machine was not restarted, so the viewer is still
                    // here — give the button its focus back.
                    self.restore_focus = from;
                }
            }
            return;
        }
        if !cancel {
            self.system_confirm.open(());
        } else {
            self.system_confirm.close();
            self.restore_focus = from;
        }
    }

    /// The passphrase prompt: what seals a configuration for the trip, and what
    /// opens one that was sealed. Returns `true` when the configuration changed.
    ///
    /// Sealing asks twice, so a mistyped passphrase cannot put the configuration
    /// out of reach before it has even left. Opening asks once and, when the
    /// passphrase does not take, leaves the prompt up with that said - the
    /// document is kept here, so a second try costs nothing but the typing.
    pub(super) fn passphrase_window(&mut self, ctx: &egui::Context) -> bool {
        // Taken for the frame, like the edit and remove windows, so the fields
        // and the navigation layer can be borrowed apart.
        let Some(mut prompt) = self.passphrase.take() else {
            return false;
        };
        let mut confirm = false;
        let mut leave = false;

        // A modal, like the other two: centred, and nothing behind it answers.
        // Its own buttons (or Back) are the only way out.
        egui::Modal::new(Id::new("xgview-passphrase")).show(ctx, |ui| {
            self.nav.open("passphrase-body");
            ui.set_width(440.0);
            dialogs::title_bar(ui, &prompt.title(), false, "");
            ui.label(RichText::new(prompt.hint()).small().color(theme::TEXT_DIM));
            ui.add_space(theme::space::S);

            // Read off once: the field is drawn with the entry hidden unless the
            // viewer asked to see it.
            let reveal = prompt.reveal;
            ui.label(monitor_i18n::tr("prompt-passphrase"));
            let first = dialogs::text_field(
                ui,
                &mut self.nav,
                &mut prompt.first,
                &monitor_i18n::tr("prompt-passphrase"),
                !reveal,
                false,
                |s| egui::TextEdit::singleline(s).password(!reveal).desired_width(f32::INFINITY),
            );
            // The prompt opens on this field.
            self.passphrase.anchor = Some(first.id);

            if prompt.job == SealJob::Export {
                ui.add_space(theme::space::XS);
                ui.label(monitor_i18n::tr("prompt-passphrase-again"));
                dialogs::text_field(
                    ui,
                    &mut self.nav,
                    &mut prompt.again,
                    &monitor_i18n::tr("prompt-passphrase-again"),
                    !reveal,
                    false,
                    |s| egui::TextEdit::singleline(s).password(!reveal).desired_width(f32::INFINITY),
                );
            }

            ui.add_space(theme::space::XS);
            self.nav.tracked(ui.checkbox(&mut prompt.reveal, monitor_i18n::tr("prompt-reveal")));

            if let Some(error) = &prompt.error {
                ui.add_space(theme::space::XS);
                ui.label(RichText::new(error).color(theme::ERROR));
            }

            ui.add_space(theme::space::M);
            if prompt.working {
                // The passphrase is being stretched away from this thread: the
                // window stays up and says so, and its two buttons are gone,
                // because there is nothing to press until the answer is in.
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(theme::space::S);
                    ui.label(monitor_i18n::tr("prompt-open-working"));
                });
            } else {
                let action = prompt.action();
                let cancel_label = monitor_i18n::tr("cancel");
                ui.horizontal(|ui| {
                    ui.add_space(Self::center_offset(ui, &[&action, &cancel_label]));
                    if self.nav.tracked(ui.button(action)).clicked() {
                        confirm = true;
                    }
                    if self.nav.tracked(ui.button(cancel_label)).clicked() {
                        leave = true;
                    }
                });
            }
            self.nav.close();
        });

        if confirm {
            match prompt.job {
                SealJob::Export => {
                    if prompt.first.is_empty() {
                        prompt.error = Some(monitor_i18n::tr("prompt-passphrase-empty"));
                    } else if prompt.first != prompt.again {
                        prompt.error = Some(monitor_i18n::tr("prompt-passphrase-mismatch"));
                    } else {
                        // Asked for here, and drawn from the next frame on by
                        // `poll_file_dialog`.
                        self.ask_for_file(FileJob::Export(Some(prompt.first.clone())));
                        leave = true;
                    }
                }
                SealJob::Import => {
                    // Handed to a thread of its own, because stretching the
                    // passphrase is the better part of a second of arithmetic:
                    // what comes of it is taken by `poll_sealing`, and the
                    // prompt waits for it with its buttons gone.
                    let data = prompt.sealed.clone().unwrap_or_default();
                    self.spawn_open(data, prompt.first.clone(), prompt.origin.clone());
                    prompt.working = true;
                }
            }
        }

        if leave {
            self.passphrase.close();
        } else {
            self.passphrase.open(prompt);
        }
        // Nothing here changes the configuration any more: what an opened file
        // becomes is settled by `poll_sealing`, on the frame the answer arrives.
        false
    }
}
