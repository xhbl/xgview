//! The top and bottom bars, in both orientations.

use super::*;

impl XgViewApp {
    /// The toolbar as drawn while the wall stands upright: the four portrait
    /// grids, paging, full screen and the orientation button - the few things a
    /// narrow bar can hold. The settings, the add-devices button and the layout
    /// picker are left out; their panels are built for landscape and do not fit.
    pub(super) fn portrait_toolbar(&mut self, ui: &mut egui::Ui) {
        Self::scale_bar(ui);
        self.nav.open("toolbar");
        let mut items: Vec<(Id, Rect)> = Vec::new();
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
            let current = self.scheduler.layout();
            for (slot, layout) in GridLayout::PORTRAIT.into_iter().enumerate() {
                let selected = current == layout && !self.scheduler.is_zoomed();
                let response = ui.selectable_label(selected, (slot + 1).to_string());
                items.push((response.id, response.rect));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }


            self.page_prev_id = Some(Id::new("portrait-prev-page"));
            self.page_next_id = Some(Id::new("portrait-next-page"));
            for (id_str, has) in [
                ("portrait-prev-page", self.can_advance(false)),
                ("portrait-next-page", self.can_advance(true)),
            ] {
                if !has {
                    continue;
                }
                if let Some(&(_, prev)) = items.last() {
                    let gap = prev.width() * 0.4;
                    let r = Rect::from_min_size(
                        egui::pos2(prev.right() + gap, prev.top()),
                        egui::vec2(prev.width(), prev.height()),
                    );
                    items.push((Id::new(id_str), r));
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.orientation_button(ui, &mut items);
                let fullscreen = icons::button(
                    ui,
                    if self.fullscreen { Icon::Collapse } else { Icon::Expand },
                    self.fullscreen,
                    &monitor_i18n::tr(if self.fullscreen {
                        "toolbar-fullscreen-leave-tip"
                    } else {
                        "toolbar-fullscreen-enter-tip"
                    }),
                );
                items.push((fullscreen.id, fullscreen.rect));
                self.name(
                    &fullscreen,
                    if self.fullscreen { "toolbar-fullscreen-leave" } else { "toolbar-fullscreen-enter" },
                );
                if fullscreen.clicked() {
                    let enabled = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), enabled);
                }
                if self.scheduler.is_zoomed() {
                    let back = icons::button(
                        ui,
                        Icon::Grid,
                        false,
                        &monitor_i18n::tr("toolbar-back-grid-tip"),
                    );
                    items.push((back.id, back.rect));
                    self.name(&back, "toolbar-back-grid");
                    if back.clicked() {
                        self.zoom_out();
                    }
                } else {
                    let aspect = icons::button(
                        ui,
                        Icon::AspectRatio,
                        self.portrait_aspect_picker,
                        &monitor_i18n::tr("toolbar-portrait-aspect-tip"),
                    );
                    items.push((aspect.id, aspect.rect));
                    self.name(&aspect, "toolbar-portrait-aspect");
                    if aspect.clicked() {
                        self.portrait_aspect_picker = !self.portrait_aspect_picker;
                        self.portrait_aspect_picker_from =
                            (!aspect.clicked_by(egui::PointerButton::Primary))
                                .then_some(aspect.id);
                    }
                }
            });
        });
        items.sort_by(|(_, left), (_, right)| left.center().x.total_cmp(&right.center().x));
        for (id, _) in &items {
            self.nav.item_id(*id);
        }
        self.nav.close();
        self.toolbar_items = items;
    }

    /// The status bar as drawn while the wall stands upright: the group when
    /// there is more than one, how many channels are live, the page, and the
    /// name of the control the remote is on. The layout is left out - the
    /// toolbar's own numbered buttons already say which grid is up.
    pub(super) fn portrait_status_bar(&mut self, ui: &mut egui::Ui) {
        Self::scale_bar(ui);
        ui.horizontal(|ui| {
            let info = self.scheduler.page_info();
            if self.config.groups.len() > 1 {
                ui.label(RichText::new(dialogs::group_name(
                    &self.config,
                    self.config.active_index(),
                )));
                ui.separator();
            }
            let live = self.channels.values().filter(|channel| channel.state.is_live()).count();
            ui.label(
                RichText::new(monitor_i18n::tr_args(
                    "status-live",
                    &[("live", live.into()), ("total", self.config.enabled_count().into())],
                ))
                .color(if live > 0 { theme::LIVE } else { theme::TEXT_DIM }),
            );
            ui.separator();
            ui.label(monitor_i18n::tr_args(
                "status-page-plain",
                &[("page", (info.page + 1).into()), ("count", info.page_count.into())],
            ));
            if let Some(name) = self.focused_name {
                ui.separator();
                ui.label(RichText::new(monitor_i18n::tr(name)).color(theme::FOCUS));
            }
        });
    }

    pub(super) fn portrait_page_buttons(&mut self, ui: &mut egui::Ui, area: Rect) {

        let r = 24.0;
        let margin = 6.0;
        let y = area.center().y;
        let fill = egui::Color32::from_black_alpha(51);
        let edge = egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(140));
        let glyph = egui::Stroke::new(2.0_f32, egui::Color32::from_white_alpha(230));
        let d = r * 0.4;
        let sides = [
            (false, "portrait-prev-page", self.can_advance(false)),
            (true, "portrait-next-page", self.can_advance(true)),
        ];
        for (forward, id, has) in sides {
            if !has {
                continue;
            }
            let x = if forward { area.max.x - margin - r } else { area.min.x + margin + r };
            let center = egui::pos2(x, y);
            let rect = Rect::from_center_size(center, egui::Vec2::splat(r * 2.0));
            let button_id = Id::new(id);
            let resp = ui.interact(rect, button_id, egui::Sense::click());
            let focused = ui.memory(|m| m.focused()) == Some(button_id);
            let p = ui.painter();
            p.circle_filled(center, r, fill);
            if focused {
                p.circle_stroke(center, r, egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS));
            } else {
                p.circle_stroke(center, r, edge);
            }
            let sign = if forward { 1.0 } else { -1.0 };
            let tip = egui::pos2(center.x + sign * d * 2.0 / 3.0, center.y);
            let wing_x = center.x - sign * d / 3.0;
            p.add(egui::Shape::line_segment(
                [egui::pos2(wing_x, center.y - d), tip],
                glyph,
            ));
            p.add(egui::Shape::line_segment(
                [egui::pos2(wing_x, center.y + d), tip],
                glyph,
            ));
            if resp.clicked() {
                self.advance(forward, !resp.clicked_by(egui::PointerButton::Primary));
            }
        }
    }

    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            self.portrait_toolbar(ui);
            return;
        }
        // The row is laid out for a television. A phone in landscape has less
        // room than that, and wrapping would cost the grid a row, so the bar
        // gives up text size instead: at the reference width and above nothing
        // changes, and below it the labels, the buttons' padding and the gaps
        // shrink together, down to the floor where they stop being readable.
        Self::scale_bar(ui);
        // The bar is one row of the navigation layer: the arrows move by
        // declaration from here on, one control per press. The controls are
        // registered after they are drawn rather than as they are, in the
        // order sorted out below - the right hand group is laid out right to
        // left, so registering it as it is drawn would walk it backwards.
        self.nav.open("toolbar");

        // Every control the remote may land on is recorded with the rectangle
        // it was drawn in: the layer needs the order, but the wall needs the
        // geometry too, to know which control is above the tile it is leaving.
        let mut items: Vec<(Id, Rect)> = Vec::new();
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
            ui.separator();

            // Layout and page are the two things a viewer changes while
            // watching, so they stay on the bar whatever its width.
            let current = self.scheduler.layout();
            for layout in GridLayout::STANDARD {
                let selected = current == layout && !self.scheduler.is_zoomed();
                let response = ui.selectable_label(selected, layout.label());
                items.push((response.id, response.rect));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }

            // The layouts that have no button of their own live behind one
            // button: it shows the number of the one in use, and an ellipsis
            // while the wall is on a layout that has a button of its own.
            let active = !current.is_standard() && !self.scheduler.is_zoomed();
            let more = ui.selectable_label(active || self.layout_picker, current.numeric_label().unwrap_or("…"));
            items.push((more.id, more.rect));
            self.name(&more, "toolbar-layout-more");
            if more.clicked() {
                self.layout_picker = !self.layout_picker;
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                self.layout_picker_from = (self.layout_picker
                    && !more.clicked_by(egui::PointerButton::Primary))
                .then_some(more.id);
            }

            // Only while magnified, and only there: the arrow on a tile, Enter
            // and a double click all magnify, but a touch screen has no keyboard
            // and no arrow, so this is the one way back it can offer.
            if self.scheduler.is_zoomed() {
                let back = icons::button(ui, Icon::Grid, false, &monitor_i18n::tr("toolbar-back-grid-tip"));
                items.push((back.id, back.rect));
                self.name(&back, "toolbar-back-grid");
                if back.clicked() {
                    self.zoom_out();
                }
            }

            ui.separator();
            let info = self.scheduler.page_info();
            let can_prev = self.can_advance(false);
            let can_next = self.can_advance(true);
            let previous = icons::button_sized(
                ui,
                Icon::Left,
                false,
                !can_prev,
                &monitor_i18n::tr("toolbar-prev-page"),
                30.0,
            );
            self.page_prev_id = Some(previous.id);
            if can_prev {
                items.push((previous.id, previous.rect));
                self.name(&previous, "toolbar-prev-page");
            }
            if previous.clicked() {
                self.advance(false, !previous.clicked_by(egui::PointerButton::Primary));
            }
            ui.label(monitor_i18n::tr_args("toolbar-page", &[("page", (info.page + 1).into()), ("count", info.page_count.into())]));
            let next = icons::button_sized(
                ui,
                Icon::Right,
                false,
                !can_next,
                &monitor_i18n::tr("toolbar-next-page"),
                30.0,
            );
            self.page_next_id = Some(next.id);
            if can_next {
                items.push((next.id, next.rect));
                self.name(&next, "toolbar-next-page");
            }
            if next.clicked() {
                self.advance(true, !next.clicked_by(egui::PointerButton::Primary));
            }

            // The right end of the row is the three things a viewer reaches for
            // while watching, and each of them is a shape a remote control, a
            // mouse and a finger all understand without a word of English. The
            // version ends the row, at the very edge: a touch on the outermost
            // band of a touch screen belongs to the system - a device using
            // gesture navigation keeps a back-gesture band there that no window
            // flag removes - so what sits in it must not be a control. A label
            // is not, and it holds the shape buttons left of the band.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .color(theme::TEXT_DIM),
                );
                self.orientation_button(ui, &mut items);
                let fullscreen = icons::button(
                    ui,
                    if self.fullscreen { Icon::Collapse } else { Icon::Expand },
                    self.fullscreen,
                    &monitor_i18n::tr(if self.fullscreen {
                        "toolbar-fullscreen-leave-tip"
                    } else {
                        "toolbar-fullscreen-enter-tip"
                    }),
                );
                items.push((fullscreen.id, fullscreen.rect));
                self.name(
                    &fullscreen,
                    if self.fullscreen { "toolbar-fullscreen-leave" } else { "toolbar-fullscreen-enter" },
                );
                if fullscreen.clicked() {
                    let enabled = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), enabled);
                }


                let settings = icons::button(ui, Icon::Gear, self.show_settings, &monitor_i18n::tr("toolbar-settings-tip"));
                items.push((settings.id, settings.rect));
                self.name(&settings, "toolbar-settings");
                if settings.clicked() {
                    self.show_settings = !self.show_settings;
                }

                let devices = icons::button(ui, Icon::Plus, self.discovery.open, &monitor_i18n::tr("toolbar-add-devices-tip"));
                items.push((devices.id, devices.rect));
                self.name(&devices, "toolbar-add-devices");
                if devices.clicked() {
                    self.discovery.open = !self.discovery.open;
                    // Only for a press that came from the keyboard or a remote:
                    // a pointer does not move the focus anywhere else in this
                    // program, so it must not appear to move it here either.
                    self.discovery.from = (self.discovery.open
                        && !devices.clicked_by(egui::PointerButton::Primary))
                    .then_some(devices.id);
                }
            });
        });

        // A right to left group is laid out - and so recorded - from the right
        // end backwards; the reader and the remote both want it left to right.
        items.sort_by(|(_, left), (_, right)| left.center().x.total_cmp(&right.center().x));
        // Registered in that order, which is the order of the layer's row.
        for (id, _) in &items {
            self.nav.item_id(*id);
        }
        self.nav.close();
        self.toolbar_items = items;
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            self.portrait_status_bar(ui);
            return;
        }
        // The status bar shares the toolbar's scale so the two rows read at the
        // same size on a phone in landscape, where the bar shrinks to fit.
        Self::scale_bar(ui);
        ui.horizontal(|ui| {
            // The left half of the row comes and goes - the zoom label, and the
            // name of the control the remote is on - and egui derives a widget's
            // id from how many widgets preceded it in its row. The author at the
            // other end is a control the arrows land on, so that variable half is
            // put in a scope of its own: the row then has the same shape every
            // frame and the author keeps its id. Without this the author is drawn
            // under a different id each time the name appears or goes, and the
            // focus the arrows gave it is dropped the moment it is drawn.
            ui.push_id("xgview-status-left", |ui| {
                let info = self.scheduler.page_info();
                // The group comes first, with no word in front of it, when there
                // is more than one to tell apart: everything after it - the live
                // count, the page, the focus - is about that group.
                if self.config.groups.len() > 1 {
                    ui.label(RichText::new(dialogs::group_name(
                        &self.config,
                        self.config.active_index(),
                    )));
                    ui.separator();
                }
                // How many of the configured channels are actually on the wall,
                // and how many there are to be: the one number that says at a
                // glance whether anything is missing. It reads better down here
                // than in the bar, where it was competing with the controls.
                let live = self.channels.values().filter(|channel| channel.state.is_live()).count();
                ui.label(
                    RichText::new(monitor_i18n::tr_args(
                        "status-live",
                        &[("live", live.into()), ("total", self.config.enabled_count().into())],
                    ))
                    .color(if live > 0 { theme::LIVE } else { theme::TEXT_DIM }),
                );
                ui.separator();
                ui.label(monitor_i18n::tr_args(
                    "status-page",
                    &[
                        ("layout", self.scheduler.layout().label().into()),
                        ("page", (info.page + 1).into()),
                        ("count", info.page_count.into()),
                    ],
                ));
                if self.scheduler.is_zoomed() {
                    ui.separator();
                    ui.label(RichText::new(monitor_i18n::tr("status-zoom")).color(theme::FOCUS));
                }
                ui.separator();
                match self.scheduler.focus() {
                    Some(focus) => ui.label(monitor_i18n::tr_args("status-focus", &[("index", (focus + 1).into())])),
                    None => ui.label(monitor_i18n::tr("status-no-focus")),
                };
                // What the remote control is on, for the controls that carry a
                // shape instead of a word and have no pointer to hover for a
                // tooltip.
                if let Some(name) = self.focused_name {
                    ui.separator();
                    ui.label(RichText::new(monitor_i18n::tr(name)).color(theme::FOCUS));
                }
            });
            // The author is a control of this row, so the row has to be a scope
            // of the navigation layer for the remote to walk onto it - without
            // one, `tracked` drops the id; see `Nav::push`.
            //
            // The group has an id of its own as well: the row on the left grows
            // and shrinks with what it has to say - the name of the focused
            // control appears there the moment the author takes the focus - and
            // without a salt of its own this group's widgets would move to new
            // ids with it, dropping the very focus the arrow just gave them.
            ui.push_id("xgview-status-right", |ui| {
                self.nav.open("status");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left, so the copyright lands at the very
                    // right and the decoder just left of it. The author is a
                    // control of its own rather than a mail link: a remote can
                    // walk onto it, and pressing it opens the About tab, which
                    // carries the address - a `mailto:` is not something a remote
                    // can follow. All three keep the top bar's own text size, and
                    // the author the default control text colour, so the bar
                    // reads as one row.
                    ui.label(
                        RichText::new(monitor_i18n::tr_args("status-copyright", &[("years", monitor_core::copyright_years().into())]))
                            .small()
                            .color(theme::TEXT_DIM),
                    );
                    let (author, _email) = monitor_core::author();
                    // A selectable label rather than a frameless button: this one
                    // draws its frame only while the pointer is on it or it holds
                    // the focus, and a focused control is painted with the focus
                    // fill and the ring - the same feedback the toolbar's own
                    // labels give. A button with no frame draws nothing at all.
                    //
                    // Its padding goes for the same reason the frame is there: a
                    // button is drawn with the button padding around its text and
                    // at least the interaction height, and both are sized for a
                    // control standing on its own - on this row they would open it
                    // up and lift the author off the line the labels either side
                    // of it sit on. With neither, the control is exactly the word,
                    // so the ring lands on the word.
                    let padding = ui.spacing().button_padding;
                    ui.spacing_mut().button_padding = egui::Vec2::ZERO;
                    let link = ui.add(
                        egui::Button::selectable(false, RichText::new(author).small())
                            .small(),
                    );
                    ui.spacing_mut().button_padding = padding;
                    let link = self.nav.tracked(link);
                    self.name(&link, "about-author-name");
                    self.status_author = Some(link.id);
                    if link.clicked() {
                        self.show_settings = true;
                        self.settings_tab = SettingsTab::About;
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(monitor_i18n::tr_args(
                            "status-decoder",
                            &[("backend", self.decoder.into())],
                        ))
                        .small()
                        .color(theme::TEXT_DIM),
                    );
                });
                self.nav.close();
            });
        });
    }
}
