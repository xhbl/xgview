//! The wall itself: the grid, the magnified viewport and the bar that floats
//! over it.

use super::*;

impl XgViewApp {
    pub(super) fn current_view(&self, total: usize) -> PageView {
        match self.scheduler.zoom() {
            Some(index) if index < total => PageView { layout: GridLayout::G1x1, cells: vec![Some(index)] },
            _ => {
                let layout = self.scheduler.layout();
                PageView { layout, cells: grid::page_cells(layout, self.scheduler.page(), total) }
            }
        }
    }

    /// Page revealed by a finger drag, when it exists.
    pub(super) fn neighbour_view(&self, forward: bool, total: usize) -> Option<PageView> {
        if self.scheduler.is_zoomed() {
            let focus = self.scheduler.focus()?;
            let index = if forward { focus + 1 } else { focus.checked_sub(1)? };
            return (index < total).then_some(PageView { layout: GridLayout::G1x1, cells: vec![Some(index)] });
        }
        let page = if forward { self.scheduler.page() + 1 } else { self.scheduler.page().checked_sub(1)? };
        if page >= self.scheduler.page_count() {
            return None;
        }
        let layout = self.scheduler.layout();
        Some(PageView { layout, cells: grid::page_cells(layout, page, total) })
    }

    /// Where the magnified viewport's picture is drawn this frame, if it has one.
    ///
    /// The one cell a magnified viewport has, through the same rectangle the
    /// wall draws it with - the layout, the line the grid leaves, the slide the
    /// page is drawn at - and the size of the picture the channel is showing. It
    /// is what a pinch and a drag are measured against, because both are aimed
    /// where the fingers are and the fingers are over this rectangle.
    ///
    /// A channel whose first picture has not arrived has no rectangle: what the
    /// tile shows meanwhile is a waiting icon, and an icon is nothing to push in.
    /// The fit a camera's picture takes in a tile of the view on screen.
    ///
    /// Magnified, the viewport has a fit of its own - the bar's picture-fit
    /// control edits it, and until it does the picture keeps its own ratio. A
    /// tile on the wall takes the fit its orientation gives it.
    pub(super) fn tile_aspect(&self, camera: &CameraSource) -> TileAspect {
        if self.scheduler.is_zoomed() {
            camera.zoom_aspect.unwrap_or(TileAspect::Original)
        } else if self.is_portrait() {
            camera.portrait_aspect
        } else {
            camera.aspect
        }
    }

    /// Whether the magnified picture is drawn a quarter turn on an upright wall.
    ///
    /// Upright, a landscape stream used to be turned to fill the screen. It is
    /// drawn as the camera sends it now - letterboxed - until the bar's rotate
    /// control asks for the turn. The choice is the configuration's own
    /// (`zoom_rotate`), one answer for every camera, because it is the wall's
    /// orientation that makes it a question and the viewer who wants it filled.
    /// A landscape wall never turns a picture: there is nothing to fill, and the
    /// control that would ask for it is not on the bar there.
    pub(super) fn magnified_rotated(&self) -> bool {
        self.is_portrait() && self.scheduler.is_zoomed() && self.config.zoom_rotate
    }

    /// Whether the magnified bar is the picture's own default: up, and staying.
    ///
    /// A portrait wall that is not turned shows the strip from the moment the
    /// picture is magnified and leaves it there. Upright, the strip lies across
    /// the bottom of a letterboxed picture and the controls - the quarter turn
    /// above all - are what the viewer is looking for; a strip on its own clock
    /// would be one more thing to ask for. Turned, the picture fills the screen
    /// and the strip would lie along its side, in the way of it: it is asked for
    /// there, exactly as in landscape. See [`Self::settle_tile_bar`].
    pub(super) fn tile_bar_pinned(&self) -> bool {
        self.is_portrait() && self.scheduler.is_zoomed() && !self.magnified_rotated()
    }

    /// Gives the bar the state the picture asks for: up while pinned, away
    /// otherwise.
    ///
    /// Called where the picture changes - a magnification, a channel step, the
    /// quarter turn, the orientation - and not every frame, so a viewer who taps
    /// the strip away is not overruled by the next frame.
    pub(super) fn settle_tile_bar(&mut self) {
        // The picture has changed under the question: whatever a tap was asking
        // for, it was asking of the picture that has just gone.
        self.tap_open_at = None;
        if self.tile_bar_pinned() {
            self.tile_bar.show(tilebar::Dock::Bottom, Instant::now());
        } else {
            self.tile_bar.hide();
        }
    }

    pub(super) fn magnified_area(
        &self,
        cameras: &[CameraSource],
        view: &PageView,
        wall: Rect,
        dx: f32,
    ) -> Option<Rect> {
        let index = self.scheduler.zoom()?;
        let cell = view.cells.iter().position(|slot| *slot == Some(index))?;
        let rect = grid::tile_rect(wall.translate(vec2(dx, 0.0)), view.layout, cell, grid::LINE);
        let camera = cameras.get(index)?;
        let size = self.textures.get(&index)?.surface.size;
        let aspect = self.tile_aspect(camera);
        // Upright, the magnified picture is the one that may be drawn sideways -
        // the same answer the tile is drawn with. See `paint_view`.
        Some(grid::picture_area(size, rect, aspect, self.magnified_rotated()))
    }

    pub(super) fn paint_view(
        &self,
        ui: &mut egui::Ui,
        area: Rect,
        cameras: &[CameraSource],
        view: &PageView,
        dx: f32,
        interactive: bool,
        tag: u8,
    ) -> TileActions {
        let mut actions = TileActions::default();
        let origin = area.translate(vec2(dx, 0.0));
        if origin.max.x < area.min.x - 4.0 || origin.min.x > area.max.x + 4.0 {
            return actions;
        }
        let focus = self.scheduler.focus();
        // Read once for the frame: the same answer for every tile, and it is the
        // frame's clock rather than each cell's turn at it.
        let ring = self.ring_shown();

        for (cell, index) in view.cells.iter().enumerate() {
            let rect = grid::tile_rect(origin, view.layout, cell, grid::LINE);
            if rect.max.x < area.min.x - 4.0 || rect.min.x > area.max.x + 4.0 {
                continue;
            }
            let tile = Tile {
                id: Id::new(("xgview-tile", tag, cell, index.unwrap_or(usize::MAX))),
                index: *index,
                camera: index.and_then(|index| cameras.get(index)),
                channel: index.and_then(|index| self.channels.get(&index)),
                video: index.and_then(|index| self.textures.get(&index)).map(|entry| entry.surface),
                // The ring is a cursor, and a wall nobody is operating is just a
                // picture: it is drawn from the last key or press on and goes
                // once the wall has been left alone for the shorter of the ring's
                // own idle and the bars'. Which bars are up does not come into
                // it. See `ring_shown`.
                focused: interactive && *index == focus && ring,
                interactive,
                dim: !interactive,
                osd: self.config.osd,
                combined: [
                    &self.config.combined_info[0],
                    &self.config.combined_info[1],
                    &self.config.combined_info[2],
                    &self.config.combined_info[3],
                ],
                time: self.time,
                aspect: index
                    .and_then(|index| cameras.get(index))
                    .map(|camera| self.tile_aspect(camera))
                    .unwrap_or_default(),
                rotated: self.magnified_rotated(),
                // The magnified viewport is the one picture that can be pushed
                // in - it is the whole wall while it stands - and it is the only
                // tile that carries the window. Nothing else is zoomable, so
                // nothing else is given one.
                picture_zoom: (index.is_some() && *index == self.scheduler.zoom())
                    .then_some(self.picture_zoom),
            };
            let response = grid::paint(ui, rect, &tile);
            if interactive {
                if response.clicked() {
                    actions.focus = *index;
                    actions.click_pos = response.interact_pointer_pos();
                }
                if response.double_clicked() {
                    actions.zoom = *index;
                    actions.double_click_pos = response.interact_pointer_pos();
                }
                if response.drag_started() {
                    actions.drag_started = true;
                }
                if response.dragged() {
                    actions.drag_delta += response.drag_delta().x;
                }
                if response.drag_stopped() {
                    actions.drag_stopped = true;
                }
                if let Some(pos) = response.hover_pos() {
                    actions.hover_pos = Some(pos);
                }
            }
        }
        actions
    }

    pub(super) fn draw_grid(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        if area.width() < 24.0 || area.height() < 24.0 {
            return;
        }
        let cameras = self.config.active_cameras();
        let total = cameras.len();
        if total == 0 {
            grid::empty_state(
                ui,
                area,
                &dialogs::group_heading(
                    &self.config,
                    self.config.active_index(),
                    "wall-empty-title",
                    "wall-empty-title-group",
                ),
                &monitor_i18n::tr(if self.is_portrait() {
                    "wall-empty-hint-portrait"
                } else {
                    "wall-empty-hint"
                }),
            );
            return;
        }

        // The wall's own colour is the grid line: the tiles leave one line's
        // width around and between themselves, and it shows through. Drawing
        // it here rather than as a border on each tile is what keeps every line
        // one line wide - two neighbours would otherwise double up in the
        // middle while the outer ring stayed half as thick.
        ui.painter().rect_filled(area, egui::CornerRadius::ZERO, theme::GRID_LINE);
        let wall = area.shrink(grid::LINE);

        let view = self.current_view(total);

        // Where the focused tile is: the arrow that walks off the top of the
        // wall should come down into the bar above that tile rather than at one
        // end of the row.
        self.grid_focus_x = self.scheduler.focus().and_then(|index| {
            let cell = view.cells.iter().position(|slot| *slot == Some(index))?;
            Some(grid::tile_rect(wall, view.layout, cell, grid::LINE).center().x)
        });

        // Page sliding out, or the neighbour page revealed by a finger drag.
        if let Some(outgoing) = self.slide.outgoing.clone() {
            let dx = (self.slide.offset - self.slide.dir) * area.width() + self.slide.drag;
            self.paint_view(ui, wall, &cameras, &outgoing, dx, false, 3);
        } else if self.slide.dragging && self.slide.drag.abs() > 1.0 {
            let forward = self.slide.drag < 0.0;
            if let Some(neighbour) = self.neighbour_view(forward, total) {
                let dx = if forward { self.slide.drag + area.width() } else { self.slide.drag - area.width() };
                self.paint_view(ui, wall, &cameras, &neighbour, dx, false, 3);
            }
        }

        let dx = self.slide.offset * area.width() + self.slide.drag;
        // What the magnified viewport shows of its picture, read and moved before
        // the tiles are drawn, so that this frame draws the window the gesture
        // has just left it at rather than the one before it. The gestures are the
        // frame's own input: while the magnification stands there is one picture
        // over the whole wall, and no tile beside it to catch a drag as one. See
        // `PictureZoom`.
        let pinched = ui.ctx().input(|input| pinch_frame(input, area));
        // A wheel is the same thing to the picture as a pinch - the fingers or
        // the mouse, pushed in about the point between them - and it is read
        // separately only because egui hands it over as a *scroll*: the interface
        // scrolls elsewhere, and the magnified viewport is the one place where
        // there is nothing to scroll. See `wheel_frame`.
        let speed = ui.ctx().options(|options| options.input_options.scroll_zoom_speed);
        let wheeled = ui.ctx().input(|input| wheel_frame(input, speed, area));
        // The fingers are the wall's on the same terms as the keys: while
        // something else owns them - a panel, a window, or the black - a gesture
        // made over the wall is the panel's to answer, and the picture keeps the
        // window it has. See `handle_keys`, which runs on the same answer. The
        // magnified viewport's bar is the wall's own surface even though its
        // controls hold the focus, so a gesture over the picture still reaches
        // the picture. The bar stays through it: a pinch, a wheel and a pan leave
        // the viewer on the same picture, and only a tap or a change of picture
        // takes the strip away. See [`Self::tap_release`].
        if self.scheduler.is_zoomed()
            && !self.blackout_on
            && (self.owner == Owner::Grid || self.tile_bar.is_open())
        {
            if let Some(dest) = self.magnified_area(&cameras, &view, wall, dx) {
                // A pinch or a wheel pushes the picture in about where it is
                // aimed; a drag moves it. The first two are one gesture or the
                // other, never both - a pinch is the fingers and the wheel is
                // the mouse - and neither is a drag, because a pinch's pointer
                // follows the first finger and a wheel has no pointer of its own.
                if let Some((delta, at)) = pinched.or(wheeled) {
                    // Pushing the picture in or out is the viewer at the same
                    // picture: the strip keeps its place while it happens.
                    self.picture_zoom = self.picture_zoom.zoomed(delta, at, dest);
                } else if self.picture_zoom.magnified() {
                    // A drag on the picture moves it; one that starts on the bar
                    // is the bar's. A finger's tap carries a little movement, and
                    // taking that as a pan would move the picture under the
                    // press that was meant for the strip.
                    let drag = ui.ctx().input(|input| {
                        drag_frame(input, area).map(|delta| (delta, input.pointer.press_origin()))
                    });
                    if let Some((delta, origin)) = drag {
                        let on_bar = self.tile_bar.is_open()
                            && origin.is_some_and(|origin| {
                                in_tile_bar_strip(
                                    origin,
                                    wall,
                                    self.magnified_rotated(),
                                    self.tile_bar.dock(),
                                )
                            });
                        if !on_bar {
                            self.picture_zoom = self.picture_zoom.panned(delta, dest);
                        }
                    }
                }
            }
        } else if !self.scheduler.is_zoomed() {
            // A wall that is not magnified has no picture to push in, and the
            // window a camera was left at belongs to that camera: whichever one
            // is magnified next comes up whole.
            self.picture_zoom = PictureZoom::FULL;
        }

        let mut actions = self.paint_view(ui, wall, &cameras, &view, dx, true, 2);

        // A drag on the magnified picture is the picture moving, not the page:
        // the wall has one picture on it and nowhere to turn to, and the channel
        // after it is reached from the toolbar's arrows or the page keys, which
        // step a magnified viewport already. A pinch is not a drag either, from
        // the frame the second finger lands: the pointer follows the first one,
        // and letting this through would turn a page under the fingers. See
        // `step_zoom`.
        if self.scheduler.is_zoomed() && (pinched.is_some() || self.picture_zoom.magnified()) {
            self.slide.dragging = false;
            self.slide.drag = 0.0;
            actions.drag_started = false;
            actions.drag_delta = 0.0;
            actions.drag_stopped = false;
        }

        if actions.drag_started {
            self.slide.dragging = true;
            self.slide.drag = 0.0;
        }
        if actions.drag_delta != 0.0 {
            self.slide.drag = (self.slide.drag + actions.drag_delta).clamp(-area.width(), area.width());
        }
        if actions.drag_stopped {
            self.slide.dragging = false;
            let threshold = area.width() * 0.18;
            if self.slide.drag <= -threshold {
                self.advance(true, false);
            } else if self.slide.drag >= threshold {
                self.advance(false, false);
            }
        }
        // The magnified viewport's bar is answered and drawn before the tile's
        // own click is: a press on it, or in the band that asks for it, belongs
        // to the bar rather than to the picture behind. See [`crate::tilebar`].
        let bar_owns_press = self.tile_bar_overlay(ui, &view, wall, dx, &actions);

        if !bar_owns_press {
            if let Some(index) = actions.focus {
                // A press on the wall is the viewer pointing at what they are
                // watching, so whatever control of the bars was holding the
                // remote's focus gives it up and the arrows move channels again.
                self.leave_controls(ui.ctx());
                if self.scheduler.focus() != Some(index) {
                    self.scheduler.set_focus(Some(index));
                    self.needs_sync = true;
                    self.mark_dirty();
                }
                // A click on a tile is the pointer's way of placing the ring, and
                // the only one it has: a drag turns the page without aiming at
                // anything, and a movement places nothing. So this is where a
                // pointer arms it. See `ring_keys`.
                self.ring_seen = Some(Instant::now());
                self.ring_aimed = true;
            }
            if let Some(index) = actions.zoom {
                self.zoom_to(index);
            }
        }
    }

    /// Answers the magnified viewport's bar, draws it, and reads the tap that
    /// puts it up or away.
    ///
    /// A tap - quick, still, one finger - is the whole of the pointer's part in
    /// it: one on the picture brings the strip up along the bottom, and one on
    /// the picture while it is up takes it away. Everything else the pointer can
    /// do there - a press that lingered, a drag, a pinch - belongs to the
    /// picture, which stays as it was. A double press is still the way back to
    /// the grid: the first press opens the strip, so the second is let through
    /// to the wall and the zoom closes the strip with it.
    ///
    /// Its shape comes from [`crate::tilebar`], which lays it out in the
    /// picture's frame and maps that onto the screen, so a picture drawn turned
    /// for a portrait wall carries a turned bar.
    pub(super) fn tile_bar_overlay(
        &mut self,
        ui: &mut egui::Ui,
        view: &PageView,
        wall: Rect,
        dx: f32,
        actions: &TileActions,
    ) -> bool {
        if !self.scheduler.is_zoomed() {
            return false;
        }
        // A page or a channel sliding under the viewer is not the moment to
        // bring the bar up: a press that lands during a slide belongs to the
        // slide. A bar that is already up stays drawn through the drag - it
        // belongs to the picture under it and moves with it - so only the closed
        // case is held back.
        if !self.tile_bar.is_open() && (self.slide.offset != 0.0 || self.slide.dragging) {
            return false;
        }
        let Some(index) = self.scheduler.zoom() else {
            return false;
        };
        let Some(cell) = view.cells.iter().position(|slot| *slot == Some(index)) else {
            return false;
        };
        let rect = grid::tile_rect(wall.translate(vec2(dx, 0.0)), view.layout, cell, grid::LINE);
        // A window too small to hold the strip and the room it keeps clear of
        // the edge is not given one; this is a picture down to a postage stamp.
        if rect.width() < 120.0 || rect.height() < 120.0 {
            self.tile_bar.hide();
            return false;
        }

        let rotated = self.magnified_rotated();
        // The controls are read again below, after the press, so a press that
        // changes the picture fit also redraws the bar with the set it has in the
        // same frame it changes it. Every control is a square.
        let controls = self.tile_bar_controls();
        let widths = vec![tilebar::icon_width(); controls.len()];
        let layout = tilebar::layout(rect, rotated, self.tile_bar.dock(), &widths);
        let now = Instant::now();
        let mut consumed = false;

        // A press on the picture closes the bar only when it was a tap: quick,
        // still, and the only finger down. A press that lingered is a thumb left
        // on the glass, one that slid is a pan being reached for, and a pinch is
        // two fingers - the bar stays through all of them. See `tap_release`.
        let tap = self.tap_release(ui.ctx());
        if let Some(pos) = actions.click_pos {
            let hit = layout.hit(pos);
            if self.tile_bar.is_open() {
                if matches!(hit, tilebar::Hit::Outside) {
                    // A tap on the picture takes the bar down. The bar takes this
                    // press with it, so one press outside is not also the first
                    // click of a double press on the picture - a tap that opened
                    // the bar and then closed it would otherwise put the
                    // magnification back.
                    // A tap on the picture takes the bar down - unless a call is
                    // up, which makes the bar modal: the handset is the way out
                    // of a call, and the microphone being open has to stay on
                    // screen. The press is still spent here, so it does not
                    // reach the wall behind.
                    if tap.is_some() && !self.tile_bar.ignoring(now) && !self.talking {
                        self.tile_bar.hide();
                    }
                    // A double press out here is the way back to the grid: the
                    // first press opened the strip, so the second is the wall's,
                    // and the zoom closes the strip as it goes.
                    consumed = actions.double_click_pos.is_none();
                } else {
                    // A press on the strip that did not land on a control - the
                    // padding between them - is still the bar being used.
                    self.tile_bar.touch(now);
                    consumed = true;
                }
            } else if tap.is_some() {
                // A tap anywhere on the picture asks for the strip, along the
                // bottom where a strip of controls belongs - but it is asked for
                // after egui's double-click window, because the second tap of a
                // double press is the way back to the grid and the first must not
                // flash the strip on its way there. See `tap_open_at`.
                if actions.double_click_pos.is_some() {
                    // This is the second tap: the wall's, and no strip.
                    self.tap_open_at = None;
                    consumed = false;
                } else {
                    let delay = ui
                        .ctx()
                        .options(|options| options.input_options.max_double_click_delay);
                    self.tap_open_at = Some(now + Duration::from_secs_f64(delay.max(0.0)));
                    consumed = true;
                }
            }
        }

        // The press may have opened the bar on the other edge than the one the
        // last opening used, and it may have changed the set of controls the
        // strip carries - a call starting takes all of them away: lay it out
        // again with the dock and the widths it has now, so this frame paints it
        // there rather than at the old edge and then jumping over on the next
        // one.
        let entries = self.tile_bar_entries();
        let widths = vec![tilebar::icon_width(); entries.len()];
        let layout = tilebar::layout(rect, rotated, self.tile_bar.dock(), &widths);

        // A double press in the band is the bar being asked for, not the
        // viewport being put back: its first press has already opened the bar,
        // and the second is the same gesture.
        if let Some(pos) = actions.double_click_pos {
            if !matches!(layout.hit(pos), tilebar::Hit::Outside) {
                consumed = true;
            }
        }

        if self.tile_bar.is_open() {
            // The controls are egui's own buttons, placed in the picture's own
            // frame: a press lights the one it went down on, the ring is the
            // focus the navigation layer puts there, and the confirm key reaches
            // it as a click on that focus. What is left here is the guard - the
            // gesture that opened the bar must not also press it - and the act.
            let controls: Vec<tilebar::Control> =
                entries.iter().map(|(control, _)| *control).collect();
            if let Some(index) = tilebar::draw(ui, &mut self.nav, &layout, rotated, &controls) {
                if !self.tile_bar.ignoring(now) {
                    // Was the ring on a control, or still on the seat? The
                    // difference is whether the remote is driving the bar.
                    let driven = ui
                        .ctx()
                        .memory(|memory| memory.focused())
                        .is_some_and(|id| id != tilebar::seat());
                    let talking = self.talking;
                    self.tile_bar.touch(now);
                    if let Some((_, action)) = entries.get(index) {
                        self.tile_bar_activate(*action);
                    }
                    if self.talking != talking {
                        // The microphone has become the handset and the ring goes
                        // with it: a call the remote started is hung up by the
                        // same key, without a walk. A press that started it is
                        // not aiming with a ring, and keeps the cursor on the
                        // seat. See `tilebar::seat`.
                        let target =
                            if driven { tilebar::control(0) } else { tilebar::seat() };
                        ui.ctx().memory_mut(|memory| memory.request_focus(target));
                    }
                }
            }
        }

        consumed
    }

    /// The controls the magnified bar carries, with what each does, in walk
    /// order.
    ///
    /// A call in progress takes the bar over: it carries one control, the red
    /// handset that ends it, and nothing else can be pressed while the
    /// microphone is open. Upright, the quarter turn comes first - the one
    /// control that changes the shape of the picture before anything is fitted
    /// into it. Then the picture fit - the icon rather than the word, with the
    /// new fit named in a toast as it changes - is always there; the sound switch
    /// for a camera that has sound, the microphone for one that offers a back
    /// channel.
    pub(super) fn tile_bar_entries(&self) -> Vec<(tilebar::Control, TileBarAction)> {
        if self.talking {
            return vec![(tilebar::Control::Hangup, TileBarAction::Hangup)];
        }
        let mut entries = Vec::new();
        if self.is_portrait() {
            entries.push((tilebar::Control::Icon(Icon::Rotate), TileBarAction::Rotate));
        }
        entries.push((tilebar::Control::Icon(Icon::AspectRatio), TileBarAction::Fit));
        if tilebar::SOUND && self.magnified_has_audio() {
            let icon = if self.sound_on { Icon::Sound } else { Icon::Mute };
            entries.push((tilebar::Control::Icon(icon), TileBarAction::Sound));
        }
        if self.magnified_has_backchannel() {
            entries.push((tilebar::Control::Icon(Icon::Mic), TileBarAction::Mic));
        }
        entries
    }

    /// The controls alone, which is what the layout and the paint need.
    pub(super) fn tile_bar_controls(&self) -> Vec<tilebar::Control> {
        self.tile_bar_entries().into_iter().map(|(control, _)| control).collect()
    }

    /// Answers a control of the magnified bar: the handset that ends a call, the
    /// quarter turn, the picture fit, the sound switch when the camera has sound,
    /// or the microphone when it has a back channel.
    ///
    /// It is the action of the control the cursor is on for a key and of the one
    /// a finger landed on - a press acts on what it hit without moving the
    /// cursor.
    pub(super) fn tile_bar_activate(&mut self, action: TileBarAction) {
        match action {
            TileBarAction::Hangup => {
                // A call is up, and its one control is the way out.
                self.set_talking(false);
            }
            TileBarAction::Rotate => {
                self.toggle_zoom_rotate();
            }
            TileBarAction::Sound => {
                self.sound_on = !self.sound_on;
                self.config.sound_on = self.sound_on;
                self.apply_audio();
                self.mark_dirty();
            }
            TileBarAction::Mic => {
                self.set_talking(!self.talking);
            }
            TileBarAction::Fit => {
                // The fit this writes is the magnified viewport's own: it is kept
                // per camera, and the wall's landscape and portrait fits - the
                // pair the settings panel and the portrait aspect picker write -
                // are not touched. Until the first press the fit is the camera's
                // own ratio, `Original`.
                //
                // The control carries no word to read the value on, so the name
                // of the fit it has just moved to is said in the middle of the
                // screen.
                let Some(index) = self.scheduler.zoom() else {
                    return;
                };
                let Some(slot) = self.config.active_slots().get(index).copied() else {
                    return;
                };
                let Some(camera) = self.config.cameras.get_mut(slot) else {
                    return;
                };
                let next = camera.zoom_aspect.unwrap_or(TileAspect::Original).next();
                camera.zoom_aspect = Some(next);
                self.mark_dirty();
                self.flash_centered(monitor_i18n::tr(next.label()), ToastKind::Info);
            }
        }
    }
}
