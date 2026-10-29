//! The Cameras tab, the group list and the reorder mode: what a camera is called,
//! where it sits and which group it is in.

use super::*;

impl XgViewApp {
    /// The cameras the wall shows, and the way to add more.
    pub(super) fn settings_cameras(&mut self, ui: &mut egui::Ui) -> bool {
        // The reorder mode draws its own list and writes nothing back until it
        // is confirmed. See `cameras_reorder`.
        if let Some(order) = self.reorder.clone() {
            self.cameras_reorder(ui, &order);
            ui.add_space(theme::space::M);
            let mut confirm_clicked = false;
            let mut cancel_clicked = false;
            ui.horizontal(|ui| {
                let confirm = ui.button(monitor_i18n::tr("cameras-confirm-order"));
                self.nav.item(&confirm);
                self.name(&confirm, "cameras-confirm-order");
                confirm_clicked = confirm.clicked();
                let cancel = ui.button(monitor_i18n::tr("cancel"));
                self.nav.item(&cancel);
                self.name(&cancel, "cameras-cancel-order");
                cancel_clicked = cancel.clicked();
            });
            if cancel_clicked {
                self.cancel_reorder();
                return false;
            }
            if confirm_clicked {
                return self.apply_reorder();
            }
            return false;
        }
        self.cameras_list(ui)
    }

    /// Parks the view the scheduler holds into the group it belongs to: the
    /// configuration is where a group's layout, paging and focus wait while it
    /// is not on the wall.
    pub(super) fn park_group_view(&mut self) {
        let (layout, page, focus) =
            (self.scheduler.layout(), self.scheduler.page(), self.scheduler.focus());
        let portrait = self.orientation.is_portrait();
        let group = self.config.group_mut();
        if portrait {
            group.portrait_layout = layout;
        } else {
            group.layout = layout;
        }
        group.page = page;
        group.focus = focus;
    }

    /// Loads the active group's view into the scheduler. The cameras follow on
    /// the next [`Self::sync`].
    ///
    /// The magnified viewport goes with the wall it was opened on: a channel
    /// index is a place in a group's camera list, so keeping it would open the
    /// new group magnified on whatever sits at that index there. A group
    /// arriving on the wall arrives whole, on the page and focus it was left at.
    pub(super) fn load_group_view(&mut self) {
        self.tile_bar.hide();
        self.scheduler.zoom_out();
        let group = self.config.group().clone();
        let layout =
            if self.orientation.is_portrait() { group.portrait_layout } else { group.layout };
        self.scheduler.set_layout(layout);
        self.scheduler.set_page(group.page);
        self.scheduler.set_focus(group.focus);
        self.needs_sync = true;
        self.apply_audio();
    }

    /// Switches the wall to another camera group.
    pub(super) fn switch_group(&mut self, index: usize) {
        if index == self.config.active_index() {
            return;
        }
        self.park_group_view();
        self.config.active_group = index;
        self.load_group_view();
    }

    /// The camera list as configured, with the button that opens the reorder
    /// mode at its foot. Returns `true` when the configuration changed.
    pub(super) fn cameras_list(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        // Which group the wall is showing comes first, as a strip of tabs like
        // the settings tabs above, with the button that adds one at its end: the
        // buttons under it add to that group, and the list below it is that
        // group's. Renaming and removing are the uncommon half of the choice, so
        // their controls are at the foot of the page instead.
        let active = self.config.active_group;
        let labels: Vec<String> = (0..self.config.groups.len())
            .map(|index| {
                monitor_i18n::tr_args(
                    "settings-group-label",
                    &[
                        ("name", dialogs::group_name(&self.config, index).into()),
                        ("count", self.config.group_camera_count(index).into()),
                    ],
                )
            })
            .collect();
        let mut switch = None;
        let mut add = false;
        ui.horizontal_wrapped(|ui| {
            for (index, label) in labels.into_iter().enumerate() {
                let tab = ui.selectable_label(active == index, label);
                self.nav.item(&tab);
                if tab.clicked() {
                    switch = Some(index);
                }
            }
            let button = ui.button(monitor_i18n::tr("settings-group-add"));
            self.nav.item(&button);
            if button.clicked() {
                add = true;
            }
        });
        if let Some(index) = switch {
            self.switch_group(index);
            dirty = true;
        }
        if add {
            // The new group is named for the viewer to type over, and it is the
            // one on the wall - so the page under the strip is its page, and the
            // field that names it takes the focus.
            let index = self.config.add_group(monitor_i18n::tr("settings-group-new"));
            self.switch_group(index);
            self.focus_group_name = true;
            dirty = true;
        }
        ui.add_space(theme::space::S);

        ui.horizontal(|ui| {
            let devices = ui.button(monitor_i18n::tr("cameras-add-devices"));
            self.nav.item(&devices);
            if devices.clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Onvif;
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                self.discovery.from =
                    (!devices.clicked_by(egui::PointerButton::Primary)).then_some(devices.id);
            }
            let manual = ui.button(monitor_i18n::tr("cameras-add-manually"));
            self.nav.item(&manual);
            if manual.clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Manual;
                self.discovery.from =
                    (!manual.clicked_by(egui::PointerButton::Primary)).then_some(manual.id);
            }
        });
        ui.add_space(theme::space::S);

        let mut toggle: Option<(usize, bool)> = None;
        let mut ask_remove: Option<(usize, String, Option<Id>)> = None;
        let mut edit: Option<(usize, Option<Id>)> = None;
        let mut aspect_change: Option<(usize, TileAspect)> = None;
        let mut infer: Option<usize> = None;
        let mut connect_change: Option<usize> = None;
        let mut stream_change: Option<usize> = None;
        // Only the group the selector chose. The slot is a position in the whole
        // list, which is what the actions below write back through.
        let active = self.config.active_group;
        let slots: Vec<usize> = self
            .config
            .cameras
            .iter()
            .enumerate()
            .filter(|(_, camera)| usize::from(camera.group) == active)
            .map(|(slot, _)| slot)
            .collect();
        for slot in slots {
            let camera = &mut self.config.cameras[slot];
            ui.horizontal(|ui| {
                let mut camera_enabled = camera.enabled;
                let enable = ui.checkbox(&mut camera_enabled, "");
                self.nav.item(&enable);
                if enable.changed() {
                    toggle = Some((slot, camera_enabled));
                }
                ui.label(RichText::new(camera.short_label(18)).strong());
                if let Some(sub) = camera.rtsp_sub.as_deref() {
                    let inferred = sub_is_derived(camera, sub);
                    ui.label(
                        RichText::new(monitor_i18n::tr(if inferred { "cameras-sub-derived" } else { "cameras-sub-set" }))
                            .small()
                            .color(theme::ACCENT),
                    );
                } else if !camera.is_restream() && infer_sub_stream(&camera.rtsp_main).is_some() {
                    // The vendor sub-stream rules only match a camera's own
                    // address; a go2rtc or Frigate restream is a stream name, so
                    // the offer is drawn only where there is something to infer.
                    let infer_button = ui.small_button(monitor_i18n::tr("cameras-infer-sub"));
                    self.nav.item(&infer_button);
                    if infer_button.clicked() {
                        infer = Some(slot);
                    }
                }
                // The stream switch: a press moves this tile between the sub
                // stream the wall pulls on every other one and the main stream
                // the viewer pinned it to. It reads the tag the tile's corner
                // reads - see `grid`'s stream item - under the same dot, so the
                // row and the picture say the same thing, and it is drawn only
                // where there is a second address to pull: with one address both
                // of its stops are the same stream.
                if camera.has_sub() {
                    let stream_button = ui
                        .small_button(
                            RichText::new(format!("● {}", monitor_i18n::tr(camera.grid_stream().tag())))
                                .small()
                                .color(theme::ACCENT),
                        )
                        .on_hover_text(monitor_i18n::tr("cameras-stream-tip"));
                    self.nav.item(&stream_button);
                    if stream_button.clicked() {
                        stream_change = Some(slot);
                    }
                }
                // The connection switch: a press moves the camera on to the next
                // stop it offers - `AUTO/RTSP/MSE` for a camera with an MSE
                // address, `TCP/UDP` for one on RTSP alone. Its tip names what it
                // switches: the protocol where there are two to choose from, the
                // RTSP transport where there is only RTSP. A camera carried over
                // a WebSocket url has a single transport, so it has nothing to
                // switch and the button is left off.
                if !camera.is_ws_stream() {
                    let tip = if camera.has_mse() { "cameras-connect-tip" } else { "cameras-transport-tip" };
                    let connect_button = ui
                        .small_button(RichText::new(camera.connection_label()).small().color(theme::ACCENT))
                        .on_hover_text(monitor_i18n::tr(tip));
                    self.nav.item(&connect_button);
                    if connect_button.clicked() {
                        connect_change = Some(slot);
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left so the pair reads edit, remove;
                    // registered edit first, so the walk reaches the control
                    // that changes before the one that deletes.
                    let remove_button = icons::button_sized(ui, Icon::Trash, false, false, &monitor_i18n::tr("cameras-remove-tip"), ROW_ICON);
                    let edit_button = icons::button_sized(ui, Icon::Edit, false, false, &monitor_i18n::tr("cameras-edit-tip"), ROW_ICON);
                    self.nav.item(&edit_button);
                    self.focus_names.insert(edit_button.id, "cameras-edit-name");
                    self.nav.item(&remove_button);
                    self.focus_names.insert(remove_button.id, "cameras-remove-name");
                    if remove_button.clicked() {
                        // Only for a press that came from the keyboard or a
                        // remote: a pointer does not move the focus anywhere else
                        // in this program, so it must not appear to move it here
                        // either.
                        let from = (!remove_button.clicked_by(egui::PointerButton::Primary))
                            .then_some(remove_button.id);
                        ask_remove = Some((slot, camera.name.clone(), from));
                    }
                    if edit_button.clicked() {
                        let from = (!edit_button.clicked_by(egui::PointerButton::Primary))
                            .then_some(edit_button.id);
                        edit = Some((slot, from));
                    }
                });
            });
            // The address, the display mode and where the camera came from, on
            // a line of their own: squeezed onto the row above, they ran into
            // each other. The mode is a button - a press moves it on to the
            // next one - and the tile carries the same mode in its corner.
            ui.horizontal(|ui| {
                ui.add_space(theme::space::S);
                let aspect = camera.aspect;
                let aspect_button = ui
                    .small_button(RichText::new(monitor_i18n::tr(aspect.label())).small().color(theme::ACCENT))
                    .on_hover_text(monitor_i18n::tr("cameras-aspect-tip"));
                self.nav.item(&aspect_button);
                self.focus_names.insert(aspect_button.id, "cameras-aspect-name");
                if aspect_button.clicked() {
                    aspect_change = Some((slot, aspect.next()));
                }
                // The address follows the connection mode, so a camera on MSE
                // reads out its MSE address. Where there is a second address,
                // the tooltip names both.
                let address = theme::truncate(&camera.masked_uri(StreamKind::Main), 26);
                let address_label = ui.label(RichText::new(address).small().monospace().color(theme::TEXT_DIM));
                if let Some(mse) = camera.masked_mse_uri(StreamKind::Main) {
                    address_label.on_hover_text(format!(
                        "rtsp: {}\nmse:  {}",
                        camera.masked_rtsp_uri(StreamKind::Main),
                        mse
                    ));
                }
                ui.label(RichText::new(monitor_i18n::tr(camera.origin.label())).small().color(theme::TEXT_DIM));
            });
            ui.add_space(theme::space::XS);
        }
        if let Some((slot, aspect)) = aspect_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.aspect = aspect;
            }
            // A display choice: nothing is reopened, the tiles just paint
            // differently from this frame on.
            dirty = true;
        }
        if let Some((slot, name, from)) = ask_remove {
            self.remove_confirm.open(RemoveConfirm { slot, name });
            self.remove_confirm.from = from;
        }
        if let Some((slot, enabled)) = toggle {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.enabled = enabled;
            }
            self.needs_sync = true;
            dirty = true;
        }
        if let Some(slot) = infer {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                if camera.apply_sub_inference() {
                    self.needs_sync = true;
                    dirty = true;
                } else {
                    self.flash(monitor_i18n::tr("toast-no-sub"), ToastKind::Error);
                }
            }
        }
        if let Some((slot, from)) = edit {
            let draft = self.config.cameras.get(slot).map(CameraDraft::from_source);
            if let Some(draft) = draft {
                self.camera_edit.open(CameraEdit { slot, draft });
                self.camera_edit.from = from;
            }
        }
        if let Some(slot) = connect_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.advance_connection();
            }
            // The connection is part of the schedule plan, so the channel is
            // reopened with the new one on the next sync.
            self.needs_sync = true;
            dirty = true;
        }
        if let Some(slot) = stream_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.advance_stream();
            }
            // The stream a tile pulls is part of the plan in the same way, so the
            // channel re-targets on the next sync - and holds its last frame
            // until the new stream delivers one; see `ScheduleChange`.
            self.needs_sync = true;
            dirty = true;
        }

        // Rearranging begins here, and changes nothing yet: the mode works on a
        // copy of the order and the configuration is written only when it is
        // confirmed. Hidden with fewer than two cameras - there is nothing to
        // arrange. The identifier is fixed, so the button that opens the mode
        // can be given the focus back when the mode closes.
        if self.config.group_camera_count(self.config.active_group) >= 2 {
            ui.add_space(theme::space::M);
            let trigger =
                ui.push_id("xgview-reorder-trigger", |ui| ui.button(monitor_i18n::tr("cameras-reorder"))).inner;
            self.nav.item(&trigger);
            self.name(&trigger, "cameras-reorder");
            self.reorder_trigger_anchor = Some(trigger.id);
            if trigger.clicked() {
                let active = self.config.active_group;
                self.reorder = Some(
                    self.config
                        .cameras
                        .iter()
                        .filter(|camera| usize::from(camera.group) == active)
                        .map(|camera| camera.id.clone())
                        .collect(),
                );
                self.reorder_focus = true;
                self.flash(monitor_i18n::tr("toast-reorder"), ToastKind::Info);
            }
        }

        // Renaming and removing a group are the uncommon half of the selector,
        // so they are down here rather than in the strip: one field and one
        // button, for the group on the wall.
        ui.add_space(theme::space::M);
        let active = self.config.active_index();
        let default_name = dialogs::group_name(&self.config, active);
        let mut remove = false;
        let mut confirmed = None;
        let mut make_default = false;
        ui.horizontal(|ui| {
            ui.label(monitor_i18n::tr("settings-group-name"));
            let field = dialogs::text_field(
                ui,
                &mut self.nav,
                &mut self.config.groups[active].name,
                &monitor_i18n::tr("settings-group-name"),
                false,
                false,
                |text| egui::TextEdit::singleline(text).hint_text(default_name).desired_width(200.0),
            );
            if field.changed() {
                dirty = true;
            }
            if std::mem::take(&mut self.focus_group_name) {
                // Just added: the focus goes to the field, so the name is typed
                // over at once rather than hunted for at the foot of the page.
                field.request_focus();
                field.scroll_to_me(Some(Align::Center));
            }
        });
        // The group's own actions, on a row of their own: the built-in group has
        // nothing to set or to remove.
        if active != 0 {
            ui.horizontal(|ui| {
                let set_default = ui.button(monitor_i18n::tr("settings-group-default-set"));
                self.nav.item(&set_default);
                if set_default.clicked() {
                    make_default = true;
                }
                let button = ui.button(monitor_i18n::tr("settings-group-remove"));
                self.nav.item(&button);
                if button.clicked() {
                    remove = true;
                }
            });
        }
        if make_default {
            // The group takes the built-in slot and stays on the wall, so the
            // view the scheduler holds is still its own; only the names moved.
            self.group_remove = None;
            if self.config.set_default_group(active) {
                dirty = true;
            }
        }
        if remove {
            self.group_remove = Some(active);
        }
        if let Some(index) = self.group_remove.filter(|index| *index == active) {
            // The question is asked where the button was, rather than in a
            // window: the group is about to go with its cameras, and a line of
            // text says so without hiding what is being lost.
            let name = dialogs::group_name(&self.config, index);
            let count = self.config.group_camera_count(index);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(monitor_i18n::tr_args(
                        "settings-group-remove-confirm",
                        &[("name", name.into()), ("count", count.into())],
                    ))
                    .small()
                    .color(theme::WARN),
                );
                let yes = ui.button(monitor_i18n::tr("settings-group-remove-yes"));
                self.nav.item(&yes);
                let no = ui.button(monitor_i18n::tr("settings-group-remove-no"));
                self.nav.item(&no);
                if yes.clicked() {
                    confirmed = Some(index);
                } else if no.clicked() {
                    self.group_remove = None;
                }
            });
        } else if self.group_remove.is_some() {
            // The page moved to another group while the question stood.
            self.group_remove = None;
        }
        if let Some(index) = confirmed {
            self.group_remove = None;
            // The view the scheduler holds belongs to the group about to go, so
            // it is parked before the list moves under it; the group the wall
            // falls back to then loads its own.
            self.park_group_view();
            if self.config.remove_group(index) {
                self.load_group_view();
                dirty = true;
            }
        }

        dirty
    }

    /// The camera list in its reorder mode.
    ///
    /// One row per camera, with up and down buttons where the ordinary list
    /// keeps its edit and remove buttons, and nothing else: the controls that
    /// change a camera are hidden while the list is being arranged. The order
    /// is the preview held in [`Self::reorder`]; it is written to the
    /// configuration only on Confirm, so the wall is untouched while the viewer
    /// arranges, and Back drops the whole thing.
    pub(super) fn cameras_reorder(&mut self, ui: &mut egui::Ui, order: &[String]) {
        let last = order.len().saturating_sub(1);
        let mut moved: Option<(usize, i32, Id)> = None;
        let mut anchor = None;
        // The list is the active group's, so its labels come from that group:
        // the identifier alone would find the entry in whichever group comes
        // first in the file.
        let active = self.config.active_group;
        for (slot, id) in order.iter().enumerate() {
            let label = self
                .config
                .find_camera_in_group(id, active)
                .map(|camera| camera.short_label(24))
                .unwrap_or_default();
            let first = slot == 0;
            let bottom = slot == last;
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left so the pair reads up, down; registered
                    // up first, so the walk reaches it first.
                    //
                    // The identifiers are the camera's, not the row's place.
                    // egui's own are seeded from the sibling position and change
                    // when the row moves, which would drop the focus on the row
                    // the viewer was working on and hand it to the list anchor
                    // at the top. Under a fixed id the focus follows the camera
                    // through the list. See `icons::button_at`.
                    let down = icons::button_at(
                        ui,
                        Id::new(id).with("down"),
                        Icon::Down,
                        false,
                        bottom,
                        &monitor_i18n::tr("cameras-move-down"),
                        ROW_ICON,
                    );
                    let up = icons::button_at(
                        ui,
                        Id::new(id).with("up"),
                        Icon::Up,
                        false,
                        first,
                        &monitor_i18n::tr("cameras-move-up"),
                        ROW_ICON,
                    );
                    // Only the arrows with somewhere to go join the walk: a
                    // muted one is inert - see `icons::button_at` - and must not
                    // be reachable either, or the viewer would land on a button
                    // that does nothing. They are still drawn, so the row keeps
                    // its shape.
                    //
                    // egui gives focus from the keyboard, not from the pointer:
                    // a press hands the focus of whatever held it back to
                    // nothing, and the panel anchor would then take it to the
                    // first row while the pointer is still down on this button.
                    // The press takes the focus here instead, as the tab strip
                    // does after a click.
                    if !first {
                        self.nav.item(&up);
                        self.name(&up, "cameras-move-camera-up");
                        if up.is_pointer_button_down_on() {
                            up.request_focus();
                        }
                    }
                    if !bottom {
                        self.nav.item(&down);
                        self.name(&down, "cameras-move-camera-down");
                        if down.is_pointer_button_down_on() {
                            down.request_focus();
                        }
                    }
                    if first {
                        // The mode opens on the down arrow of the first row: the
                        // up arrow beside it is muted - the first camera has
                        // nowhere to go up - and a mode that opens on a dead
                        // button reads as a mode that does not work.
                        anchor = Some(down.id);
                    }
                    if up.clicked() && !first {
                        moved = Some((slot, -1, up.id));
                    }
                    if down.clicked() && !bottom {
                        moved = Some((slot, 1, down.id));
                    }
                });
            });
            ui.add_space(theme::space::XS);
        }
        self.reorder_anchor = anchor;
        let Some((slot, delta, pressed)) = moved else {
            return;
        };
        let to = (slot as i32 + delta).clamp(0, last as i32) as usize;
        let Some(order) = self.reorder.as_mut() else {
            return;
        };
        order.swap(slot, to);
        let camera = order[to].clone();
        // The button that was just pressed may have run out of room: the down
        // arrow at the foot of the list, the up arrow at the top are drawn
        // muted and do nothing. The focus goes to the arrow that still has
        // somewhere to go, so a run of presses keeps moving the camera instead
        // of landing on a dead button. Anywhere else it stays where it was.
        let follow = if to == last && delta > 0 {
            Id::new(&camera).with("up")
        } else if to == 0 && delta < 0 {
            Id::new(&camera).with("down")
        } else {
            pressed
        };
        // The focus went with the camera - to the other arrow when the pressed
        // one is spent - and the row it is on has to be brought into view. The
        // reveal is armed here and made next frame, as the button is drawn
        // again. See `reveal_next`.
        self.reveal_next = Some(follow);
        ui.ctx().memory_mut(|memory| memory.request_focus(follow));
    }

    /// Writes the arranged order back and leaves the mode. `true` when the
    /// order actually changed, so that the wall is only retargeted then.
    pub(super) fn apply_reorder(&mut self) -> bool {
        let Some(order) = self.reorder.take() else {
            return false;
        };
        self.reorder_focus = false;
        self.restore_focus = self.reorder_trigger_anchor;
        if !self.config.apply_order(&order) {
            return false;
        }
        // The order is the channel order: the plan follows it on the next sync.
        self.needs_sync = true;
        self.mark_dirty();
        true
    }

    /// Leaves the reorder mode without writing anything. The configuration was
    /// never touched, so the list simply goes back to what it was.
    pub(super) fn cancel_reorder(&mut self) {
        if self.reorder.take().is_none() {
            return;
        }
        self.reorder_focus = false;
        self.restore_focus = self.reorder_trigger_anchor;
    }
}
