//! The top and bottom bars, in both orientations.

use super::*;
use egui::Response;

/// Radius of the portrait viewport's page arrows.
const PAGE_ARROW_R: f32 = 24.0;
/// How far their centres sit from the side of the area they are drawn against.
const PAGE_ARROW_MARGIN: f32 = 6.0;

/// The portrait bar's name and numbered layout buttons, as painted.
///
/// A paint pass is `&self`: it creates the widgets in the same order as the
/// old in-one-call bar and hands their responses and rectangles back. The
/// wall's wrapper consumes them in that order; the Android overlay calls the
/// passes and drops what they return, so it writes no app state.
struct PortraitLayoutsPaint {
    items: Vec<(Id, Rect)>,
    layouts: Vec<(Response, GridLayout)>,
}

/// The right-hand group of the portrait bar, as painted.
struct PortraitEndPaint {
    items: Vec<(Id, Rect)>,
    fullscreen: Response,
    back: Option<Response>,
    aspect: Option<Response>,
}

/// The landscape bar's name, separator and standard layout buttons, as
/// painted. `current` is the layout at paint time: the next pass needs the
/// pre-click value for its "more" button.
struct ToolbarLayoutsPaint {
    items: Vec<(Id, Rect)>,
    layouts: Vec<(Response, GridLayout)>,
    current: GridLayout,
}

/// The landscape bar's "more" and back buttons, as painted. They are their
/// own pass because their clicks - `set_layout` and `zoom_out` - are read by
/// the paging controls painted next.
struct ToolbarMoreBackPaint {
    items: Vec<(Id, Rect)>,
    more: Response,
    back: Option<Response>,
}

/// The landscape bar's paging controls, as painted, after the back button's
/// click has been applied.
struct ToolbarPagingPaint {
    items: Vec<(Id, Rect)>,
    previous: Response,
    next: Response,
    previous_id: Id,
    next_id: Id,
    can_prev: bool,
    can_next: bool,
}

/// The right-hand group of the landscape bar, as painted.
struct ToolbarEndPaint {
    items: Vec<(Id, Rect)>,
    fullscreen: Response,
    settings: Response,
    devices: Response,
}

/// The orientation button, as painted. Its name travels with the response so
/// the wall registers the one the paint pass chose; the overlay never has to
/// read or consume the click.
struct OrientationPaint {
    response: Response,
    name: &'static str,
}

/// The landscape status bar's author link, as painted.
struct StatusBarPaint {
    link: Response,
}

impl XgViewApp {
    /// The toolbar as drawn while the wall stands upright: the four portrait
    /// grids, paging, full screen and the orientation button - the few things a
    /// narrow bar can hold. The settings, the add-devices button and the layout
    /// picker are left out; their panels are built for landscape and do not fit.
    ///
    /// Its body is split into paint-only passes. The wall runs each pass and
    /// applies its responses before the next pass creates widgets, so a later
    /// control sees the state an earlier click just wrote - the back button
    /// still disappears the frame a layout button is chosen. The Android
    /// overlay runs the same passes and discards what they return.
    pub(super) fn portrait_toolbar(&mut self, ui: &mut egui::Ui) {
        self.nav.open("toolbar");
        let mut items: Vec<(Id, Rect)> = Vec::new();
        ui.horizontal_centered(|ui| {
            let a = self.paint_portrait_toolbar_a(ui);
            self.apply_portrait_toolbar_a(&mut items, a);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let b = self.paint_portrait_toolbar_b(ui);
                self.apply_portrait_toolbar_b(&mut items, b);
                let c = self.paint_portrait_toolbar_c(ui);
                self.apply_portrait_toolbar_c(ui, &mut items, c);
            });
        });
        items.sort_by(|(_, left), (_, right)| left.center().x.total_cmp(&right.center().x));
        for (id, _) in &items {
            self.nav.item_id(*id);
        }
        self.nav.close();
        self.toolbar_items = items;
    }

    /// Paint the portrait bar's name and numbered layout buttons.
    ///
    /// The first of the three passes [`Self::portrait_toolbar`] runs inside a
    /// single `horizontal_centered` / `right_to_left` pair.
    fn paint_portrait_toolbar_a(&self, ui: &mut egui::Ui) -> PortraitLayoutsPaint {
        Self::scale_bar(ui);
        let mut paint = PortraitLayoutsPaint {
            items: Vec::new(),
            layouts: Vec::new(),
        };
        ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
        let current = self.scheduler.layout();
        for (slot, layout) in GridLayout::PORTRAIT.into_iter().enumerate() {
            let selected = current == layout && !self.scheduler.is_zoomed();
            let response = ui.selectable_label(selected, (slot + 1).to_string());
            paint.items.push((response.id, response.rect));
            paint.layouts.push((response, layout));
        }
        paint
    }

    /// Register the portrait layout buttons and work out the page arrows.
    ///
    /// The arrows are rectangles rather than widgets, so they are collected
    /// here from the buttons just painted - before the right-hand group is
    /// created, just where the unsplit bar collected them.
    fn apply_portrait_toolbar_a(&mut self, items: &mut Vec<(Id, Rect)>, paint: PortraitLayoutsPaint) {
        let PortraitLayoutsPaint { items: drawn, layouts } = paint;
        items.extend(drawn);
        for (response, layout) in layouts {
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
    }

    /// Paint the portrait bar's orientation button, the first control of its
    /// right-to-left group.
    fn paint_portrait_toolbar_b(&self, ui: &mut egui::Ui) -> Option<OrientationPaint> {
        self.paint_orientation_button(ui)
    }

    /// Register the portrait orientation button and apply its click before
    /// the rest of the group is painted: a rotation changes the zoom that the
    /// back/aspect button below reads.
    fn apply_portrait_toolbar_b(&mut self, items: &mut Vec<(Id, Rect)>, paint: Option<OrientationPaint>) {
        if let Some(paint) = paint {
            self.apply_orientation_button(items, paint);
        }
    }

    /// Paint the portrait bar's fullscreen and back/aspect buttons.
    ///
    /// The zoom read that picks back against aspect ratio happens here, after
    /// the earlier passes have applied their clicks, so it matches the old
    /// in-order bar.
    fn paint_portrait_toolbar_c(&self, ui: &mut egui::Ui) -> PortraitEndPaint {
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
        let mut paint = PortraitEndPaint {
            items: vec![(fullscreen.id, fullscreen.rect)],
            fullscreen,
            back: None,
            aspect: None,
        };
        if self.scheduler.is_zoomed() {
            let back = icons::button(
                ui,
                Icon::Grid,
                false,
                &monitor_i18n::tr("toolbar-back-grid-tip"),
            );
            paint.items.push((back.id, back.rect));
            paint.back = Some(back);
        } else {
            let aspect = icons::button(
                ui,
                Icon::AspectRatio,
                self.portrait_aspect_picker.is_open(),
                &monitor_i18n::tr("toolbar-portrait-aspect-tip"),
            );
            paint.items.push((aspect.id, aspect.rect));
            paint.aspect = Some(aspect);
        }
        paint
    }

    /// Register the last portrait controls and consume their clicks.
    fn apply_portrait_toolbar_c(
        &mut self,
        ui: &mut egui::Ui,
        items: &mut Vec<(Id, Rect)>,
        paint: PortraitEndPaint,
    ) {
        let PortraitEndPaint {
            items: drawn,
            fullscreen,
            back,
            aspect,
        } = paint;
        items.extend(drawn);
        self.name(
            &fullscreen,
            if self.fullscreen { "toolbar-fullscreen-leave" } else { "toolbar-fullscreen-enter" },
        );
        if fullscreen.clicked() {
            let enabled = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), enabled);
        }
        if let Some(back) = back {
            self.name(&back, "toolbar-back-grid");
            if back.clicked() {
                self.zoom_out();
            }
        }
        if let Some(aspect) = aspect {
            self.name(&aspect, "toolbar-portrait-aspect");
            if aspect.clicked() {
                if self.portrait_aspect_picker.is_open() {
                    self.portrait_aspect_picker.close();
                } else {
                    self.portrait_aspect_picker.open(());
                }
                self.portrait_aspect_picker.from =
                    (!aspect.clicked_by(egui::PointerButton::Primary)).then_some(aspect.id);
            }
        }
    }

    /// The orientation button, shared by both bars: the shape is the mode it
    /// switches to. Drawn only where the device can stand upright.
    ///
    /// This is the paint half only: the icon, tip and name are chosen from the
    /// state at paint time. The wall registers the name and handles the click
    /// in [`Self::apply_orientation_button`]; the overlay drops the paint.
    fn paint_orientation_button(&self, ui: &mut egui::Ui) -> Option<OrientationPaint> {
        if !self.supports_portrait {
            return None;
        }
        let (icon, key, tip) = if self.is_portrait() {
            (Icon::Landscape, "toolbar-orientation-landscape", "toolbar-orientation-landscape-tip")
        } else {
            (Icon::Portrait, "toolbar-orientation-portrait", "toolbar-orientation-portrait-tip")
        };
        let response = icons::button(ui, icon, false, &monitor_i18n::tr(tip));
        Some(OrientationPaint { response, name: key })
    }

    /// The wall half of the shared orientation button: register it and act on
    /// its click, after the paint and before the rest of the bar is drawn.
    fn apply_orientation_button(&mut self, items: &mut Vec<(Id, Rect)>, paint: OrientationPaint) {
        let OrientationPaint { response, name } = paint;
        items.push((response.id, response.rect));
        self.name(&response, name);
        if response.clicked() {
            self.set_orientation(!self.is_portrait());
        }
    }

    /// The status bar as drawn while the wall stands upright: the group when
    /// there is more than one, how many channels are live, the page, and the
    /// name of the control the remote is on. The layout is left out - the
    /// toolbar's own numbered buttons already say which grid is up.
    pub(super) fn portrait_status_bar(&mut self, ui: &mut egui::Ui) {
        self.paint_portrait_status_bar(ui);
    }

    /// The paint-only copy of [`Self::portrait_status_bar`].
    ///
    /// This bar has no controls and writes nothing to the app, so the wall and
    /// the Android overlay can share one body: the overlay paints it over the
    /// picture while the wall's copy sits under it. See `draw_overlay_only`.
    fn paint_portrait_status_bar(&self, ui: &mut egui::Ui) {
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

    /// The bar's visible copy on the Android overlay, which sits above the
    /// picture's surface. The wall's copy is under that surface, so a pinch
    /// that scales the picture past its box hides it; this is the copy the
    /// viewer actually sees then.
    ///
    /// The wall's frame runs first and applies every click, so the state read
    /// here is the same one the wall's later paint passes saw. Replaying the
    /// same paint passes in the same layout scopes reproduces the same widgets
    /// and appearance. No response is consumed and nothing is written: the
    /// presses still belong to the wall's frame, where the pointer and the
    /// remote already are. See `draw_overlay_only`.
    #[cfg(target_os = "android")]
    pub(super) fn paint_toolbar_copy(&self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            ui.horizontal_centered(|ui| {
                let _a = self.paint_portrait_toolbar_a(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let _b = self.paint_portrait_toolbar_b(ui);
                    let _c = self.paint_portrait_toolbar_c(ui);
                });
            });
        } else {
            ui.horizontal_centered(|ui| {
                let a = self.paint_toolbar_a(ui);
                let current = a.current;
                let _b = self.paint_toolbar_b(ui, current);
                let _c = self.paint_toolbar_c(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let _d = self.paint_toolbar_d(ui);
                    let _e = self.paint_toolbar_e(ui);
                });
            });
        }
    }

    /// The status bar's copy on the Android overlay; see [`Self::paint_toolbar_copy`].
    #[cfg(target_os = "android")]
    pub(super) fn paint_status_bar_copy(&self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            self.paint_portrait_status_bar(ui);
        } else {
            let _ = self.paint_status_bar(ui);
        }
    }

    /// Whether the portrait viewport's page arrows are up.
    ///
    /// Asked by both copies - the wall's, and the overlay's over a picture
    /// scanning out of its own surface - so that the two cannot disagree about
    /// it. They are the arrows of a viewport with the bars kept and no strip of
    /// controls open: the strip is docked to the same edges, and its own paging
    /// buttons are what a viewer reaches for while it is up.
    pub(super) fn page_arrows_shown(&self) -> bool {
        self.is_portrait() && self.chrome.visible && !self.tile_bar.is_open()
    }

    /// Where the portrait viewport's page arrows are, and whether there is a
    /// page to go to that way: `(forward, the control's id, whether there is a
    /// page that way, the centre of the circle)`.
    ///
    /// The arrows are drawn twice while a picture is on its own surface - by
    /// the wall, which is under that picture, and by the overlay over it - so
    /// both copies have to put them in the same place. See
    /// [`Self::portrait_page_buttons`] and [`Self::paint_page_arrow`].
    pub(super) fn portrait_page_arrow_sides(
        &self,
        area: Rect,
    ) -> [(bool, &'static str, bool, egui::Pos2); 2] {
        let y = area.center().y;
        [
            (
                false,
                "portrait-prev-page",
                self.can_advance(false),
                egui::pos2(area.min.x + PAGE_ARROW_MARGIN + PAGE_ARROW_R, y),
            ),
            (
                true,
                "portrait-next-page",
                self.can_advance(true),
                egui::pos2(area.max.x - PAGE_ARROW_MARGIN - PAGE_ARROW_R, y),
            ),
        ]
    }

    /// One page arrow, painted: the disc, its rim - or the remote's ring, when
    /// that is on it - and the chevron.
    ///
    /// A paint and nothing else: the presses are the wall's frame's, which is
    /// where the pointer and the remote are. `focused` is that frame's focus
    /// mirror, carried across by `android_surface`.
    pub(super) fn paint_page_arrow(
        painter: &egui::Painter,
        center: egui::Pos2,
        forward: bool,
        focused: bool,
    ) {
        let fill = egui::Color32::from_black_alpha(51);
        let edge = egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(140));
        let glyph = egui::Stroke::new(2.0_f32, egui::Color32::from_white_alpha(230));
        let d = PAGE_ARROW_R * 0.4;
        painter.circle_filled(center, PAGE_ARROW_R, fill);
        if focused {
            painter.circle_stroke(
                center,
                PAGE_ARROW_R,
                egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS),
            );
        } else {
            painter.circle_stroke(center, PAGE_ARROW_R, edge);
        }
        let sign = if forward { 1.0 } else { -1.0 };
        let tip = egui::pos2(center.x + sign * d * 2.0 / 3.0, center.y);
        let wing_x = center.x - sign * d / 3.0;
        painter.add(egui::Shape::line_segment(
            [egui::pos2(wing_x, center.y - d), tip],
            glyph,
        ));
        painter.add(egui::Shape::line_segment(
            [egui::pos2(wing_x, center.y + d), tip],
            glyph,
        ));
    }

    pub(super) fn portrait_page_buttons(&mut self, ui: &mut egui::Ui, area: Rect) {
        for (forward, id, has, center) in self.portrait_page_arrow_sides(area) {
            if !has {
                continue;
            }
            let rect = Rect::from_center_size(center, egui::Vec2::splat(PAGE_ARROW_R * 2.0));
            let button_id = Id::new(id);
            let resp = ui.interact(rect, button_id, egui::Sense::click());
            let focused = ui.memory(|m| m.focused()) == Some(button_id);
            Self::paint_page_arrow(ui.painter(), center, forward, focused);
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
        // The scaling is done by the first paint pass so an overlay painting
        // the same passes gets the same text and spacing on the same screen.
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
            let a = self.paint_toolbar_a(ui);
            let current = a.current;
            self.apply_toolbar_a(&mut items, a);
            let b = self.paint_toolbar_b(ui, current);
            self.apply_toolbar_b(&mut items, b);
            let c = self.paint_toolbar_c(ui);
            self.apply_toolbar_c(&mut items, c);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let d = self.paint_toolbar_d(ui);
                self.apply_toolbar_d(&mut items, d);
                let e = self.paint_toolbar_e(ui);
                self.apply_toolbar_e(ui, &mut items, e);
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

    /// Paint the landscape bar's name, separator and standard layout buttons.
    ///
    /// The first pass of [`Self::toolbar`]. `current` is handed to the next
    /// pass so its "more" button reads the same pre-click layout as before.
    fn paint_toolbar_a(&self, ui: &mut egui::Ui) -> ToolbarLayoutsPaint {
        Self::scale_bar(ui);
        let mut items = Vec::new();
        let mut layouts = Vec::new();
        ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
        ui.separator();

        // Layout and page are the two things a viewer changes while
        // watching, so they stay on the bar whatever its width.
        let current = self.scheduler.layout();
        for layout in GridLayout::STANDARD {
            let selected = current == layout && !self.scheduler.is_zoomed();
            let response = ui.selectable_label(selected, layout.label());
            items.push((response.id, response.rect));
            layouts.push((response, layout));
        }
        ToolbarLayoutsPaint {
            items,
            layouts,
            current,
        }
    }

    /// Register the landscape layout buttons and consume their clicks. The
    /// next pass is painted only after `set_layout` has run, so the back
    /// button below sees the zoom state it changed.
    fn apply_toolbar_a(&mut self, items: &mut Vec<(Id, Rect)>, paint: ToolbarLayoutsPaint) {
        items.extend(paint.items);
        for (response, layout) in paint.layouts {
            self.name(&response, layout_hint(layout));
            if response.clicked() {
                self.set_layout(layout);
            }
        }
    }

    /// Paint the landscape bar's "more" and back buttons.
    ///
    /// The second pass of [`Self::toolbar`], still inside `horizontal_centered`.
    /// The "more" button reads the layout handed over by the first pass - a
    /// stale layout with a fresh zoom read, as in the unsplit bar - while the
    /// back button's zoom read sees the first pass's clicks already applied.
    /// Both clicks are applied before the paging pass is painted: `zoom_out`
    /// changes the answer `can_advance` gives the buttons below.
    fn paint_toolbar_b(&self, ui: &mut egui::Ui, current: GridLayout) -> ToolbarMoreBackPaint {
        let mut items = Vec::new();

        // The layouts that have no button of their own live behind one
        // button: it shows the number of the one in use, and an ellipsis
        // while the wall is on a layout that has a button of its own.
        let active = !current.is_standard() && !self.scheduler.is_zoomed();
        let more = ui.selectable_label(
            active || self.layout_picker.is_open(),
            current.toolbar_label(),
        );
        items.push((more.id, more.rect));

        // Only while magnified, and only there: the arrow on a tile, Enter
        // and a double click all magnify, but a touch screen has no keyboard
        // and no arrow, so this is the one way back it can offer.
        let back = if self.scheduler.is_zoomed() {
            let response = icons::button(ui, Icon::Grid, false, &monitor_i18n::tr("toolbar-back-grid-tip"));
            items.push((response.id, response.rect));
            Some(response)
        } else {
            None
        };
        ToolbarMoreBackPaint { items, more, back }
    }

    /// Register the "more" and back buttons and consume their clicks, in the
    /// order the unsplit bar did.
    fn apply_toolbar_b(&mut self, items: &mut Vec<(Id, Rect)>, paint: ToolbarMoreBackPaint) {
        let ToolbarMoreBackPaint { items: drawn, more, back } = paint;
        items.extend(drawn);
        self.name(&more, "toolbar-layout-more");
        if more.clicked() {
            if self.layout_picker.is_open() {
                self.layout_picker.close();
            } else {
                self.layout_picker.open(());
                self.layout_picker_tab = None;
            }
            // Only for a press that came from the keyboard or a remote: a
            // pointer does not move the focus anywhere else in this program,
            // so it must not appear to move it here either.
            self.layout_picker.from = (self.layout_picker.is_open()
                && !more.clicked_by(egui::PointerButton::Primary))
            .then_some(more.id);
        }
        if let Some(back) = back {
            self.name(&back, "toolbar-back-grid");
            if back.clicked() {
                self.zoom_out();
            }
        }
    }

    /// Paint the landscape bar's paging controls.
    ///
    /// The third pass, painted after the back button's click has been applied
    /// so `page_info` and `can_advance` answer as they did in the unsplit bar.
    fn paint_toolbar_c(&self, ui: &mut egui::Ui) -> ToolbarPagingPaint {
        let mut items = Vec::new();
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
        let previous_id = previous.id;
        if can_prev {
            items.push((previous.id, previous.rect));
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
        let next_id = next.id;
        if can_next {
            items.push((next.id, next.rect));
        }
        ToolbarPagingPaint {
            items,
            previous,
            next,
            previous_id,
            next_id,
            can_prev,
            can_next,
        }
    }

    /// Register the paging controls and consume their clicks, in the order
    /// the unsplit bar did.
    fn apply_toolbar_c(&mut self, items: &mut Vec<(Id, Rect)>, paint: ToolbarPagingPaint) {
        let ToolbarPagingPaint {
            items: drawn,
            previous,
            next,
            previous_id,
            next_id,
            can_prev,
            can_next,
        } = paint;
        items.extend(drawn);
        self.page_prev_id = Some(previous_id);
        if can_prev {
            self.name(&previous, "toolbar-prev-page");
        }
        if previous.clicked() {
            self.advance(false, !previous.clicked_by(egui::PointerButton::Primary));
        }
        self.page_next_id = Some(next_id);
        if can_next {
            self.name(&next, "toolbar-next-page");
        }
        if next.clicked() {
            self.advance(true, !next.clicked_by(egui::PointerButton::Primary));
        }
    }

    /// Paint the version label and the orientation button: the first half of
    /// the landscape bar's right-to-left group.
    ///
    /// The right end of the row is the three things a viewer reaches for
    /// while watching, and each of them is a shape a remote control, a
    /// mouse and a finger all understand without a word of English. The
    /// version ends the row, at the very edge: a touch on the outermost
    /// band of a touch screen belongs to the system - a device using
    /// gesture navigation keeps a back-gesture band there that no window
    /// flag removes - so what sits in it must not be a control. A label
    /// is not, and it holds the shape buttons left of the band.
    fn paint_toolbar_d(&self, ui: &mut egui::Ui) -> Option<OrientationPaint> {
        ui.label(
            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                .small()
                .color(theme::TEXT_DIM),
        );
        self.paint_orientation_button(ui)
    }

    /// Register the orientation button and apply its click before the rest of
    /// the right-to-left group is painted: a rotation clears `show_settings`,
    /// which the settings button below reads at paint time.
    fn apply_toolbar_d(&mut self, items: &mut Vec<(Id, Rect)>, paint: Option<OrientationPaint>) {
        if let Some(paint) = paint {
            self.apply_orientation_button(items, paint);
        }
    }

    /// Paint the fullscreen, settings and add-devices buttons: the tail of
    /// the landscape bar's right-to-left group.
    fn paint_toolbar_e(&self, ui: &mut egui::Ui) -> ToolbarEndPaint {
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
        let settings = icons::button(ui, Icon::Gear, self.show_settings, &monitor_i18n::tr("toolbar-settings-tip"));
        let devices = icons::button(ui, Icon::Plus, self.discovery.open, &monitor_i18n::tr("toolbar-add-devices-tip"));
        ToolbarEndPaint {
            items: vec![
                (fullscreen.id, fullscreen.rect),
                (settings.id, settings.rect),
                (devices.id, devices.rect),
            ],
            fullscreen,
            settings,
            devices,
        }
    }

    /// Register the last landscape controls and consume their clicks.
    fn apply_toolbar_e(
        &mut self,
        ui: &mut egui::Ui,
        items: &mut Vec<(Id, Rect)>,
        paint: ToolbarEndPaint,
    ) {
        let ToolbarEndPaint {
            items: drawn,
            fullscreen,
            settings,
            devices,
        } = paint;
        items.extend(drawn);
        self.name(
            &fullscreen,
            if self.fullscreen { "toolbar-fullscreen-leave" } else { "toolbar-fullscreen-enter" },
        );
        if fullscreen.clicked() {
            let enabled = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), enabled);
        }
        self.name(&settings, "toolbar-settings");
        if settings.clicked() {
            self.show_settings = !self.show_settings;
        }
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
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            self.portrait_status_bar(ui);
            return;
        }
        // The status bar shares the toolbar's scale so the two rows read at the
        // same size on a phone in landscape, where the bar shrinks to fit.
        //
        // Painted in one pass, then the wall registers the author link and
        // consumes its click; the Android overlay paints the same pass and
        // drops the link without either.
        let StatusBarPaint { link } = self.paint_status_bar(ui);
        // The author is a control of this row, so the row has to be a scope
        // of the navigation layer for the remote to walk onto it - without
        // one, `tracked` drops the id; see `Nav::push`.
        self.nav.open("status");
        let link = self.nav.tracked(link);
        self.name(&link, "about-author-name");
        self.status_author = Some(link.id);
        if link.clicked() {
            self.show_settings = true;
            self.settings_tab = SettingsTab::About;
        }
        self.nav.close();
    }

    /// The paint-only copy of [`Self::status_bar`].
    ///
    /// Every widget is created here - the scale, the row, both `push_id`
    /// scopes and the author button - but nothing is registered with the
    /// navigation layer, no name is written and the author's click is left
    /// for the wall's wrapper to consume. The overlay can therefore call
    /// this `&self` body to repaint the bar over a magnified picture.
    fn paint_status_bar(&self, ui: &mut egui::Ui) -> StatusBarPaint {
        Self::scale_bar(ui);
        let link = ui.horizontal(|ui| {
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
                        ("layout", self.scheduler.layout().display_label().into()),
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
            // The group has an id of its own as well: the row on the left grows
            // and shrinks with what it has to say - the name of the focused
            // control appears there the moment the author takes the focus - and
            // without a salt of its own this group's widgets would move to new
            // ids with it, dropping the very focus the arrow just gave them.
            ui.push_id("xgview-status-right", |ui| {
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
                    ui.separator();
                    ui.label(
                        RichText::new(monitor_i18n::tr_args(
                            "status-decoder",
                            &[("backend", self.decoder.into())],
                        ))
                        .small()
                        .color(theme::TEXT_DIM),
                    );
                    link
                })
                .inner
            })
            .inner
        })
        .inner;
        StatusBarPaint { link }
    }
}

