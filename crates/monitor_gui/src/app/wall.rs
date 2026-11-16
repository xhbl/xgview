//! The wall itself: the grid, the magnified viewport and the bar that floats
//! over it.

use super::*;

impl XgViewApp {
    pub(super) fn current_view(&self, total: usize) -> PageView {
        match self.scheduler.zoom() {
            Some(index) if index < total => PageView {
                layout: GridLayout::G1x1,
                first: index,
                cells: vec![Some(index)],
            },
            _ => {
                let layout = self.scheduler.layout();
                let page = self.scheduler.page();
                PageView { layout, first: page * layout.capacity(), cells: grid::page_cells(layout, page, total) }
            }
        }
    }

    /// Page revealed by a finger drag, when it exists.
    pub(super) fn neighbour_view(&self, forward: bool, total: usize) -> Option<PageView> {
        if self.scheduler.is_zoomed() {
            let focus = self.scheduler.focus()?;
            let index = if forward { focus + 1 } else { focus.checked_sub(1)? };
            return (index < total).then_some(PageView {
                layout: GridLayout::G1x1,
                first: index,
                cells: vec![Some(index)],
            });
        }
        let page = if forward { self.scheduler.page() + 1 } else { self.scheduler.page().checked_sub(1)? };
        if page >= self.scheduler.page_count() {
            return None;
        }
        let layout = self.scheduler.layout();
        Some(PageView { layout, first: page * layout.capacity(), cells: grid::page_cells(layout, page, total) })
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
        // for, it was asking of the picture that has just gone. Only a change
        // of channel or of magnification does that - a quarter turn or a change
        // of orientation leaves the same picture, and its question, in place.
        if self.playback_asking.is_some_and(|(channel, _)| self.scheduler.zoom() != Some(channel)) {
            self.playback_asking = None;
        }
        self.tap_open_at = None;
        if self.tile_bar_pinned() {
            self.tile_bar.show(tilebar::Dock::Bottom, Instant::now());
        } else {
            self.tile_bar.hide();
        }
    }

    /// Whether the wall is showing one picture full screen.
    ///
    /// The picture's own surface is only worth having while it is. A
    /// `SurfaceView` keeps the last frame it was given, and it is drawn over the
    /// wall: left standing when the grid comes back, it covers the wall with a
    /// still picture - a wall that has gone back to nine channels and looks
    /// like one frozen one.
    pub fn magnified(&self) -> bool {
        self.scheduler.is_zoomed()
    }

    /// Whether the wall leaves the picture's area unpainted because the decoder
    /// is scanning it out of a surface of its own.
    ///
    /// Only a **magnification** asks for that. A one-cell grid is not one: it
    /// pulls the cheap sub stream, and its pictures come back as frames for the
    /// wall to draw. `android_surface` shows the picture's view for exactly
    /// `magnified()`, so the two have to agree - leave the area blank while the
    /// view is hidden and there is nothing on screen at all.
    ///
    /// `picture_on_its_own_surface` is the other half: a session really is
    /// rendering into that surface. A window alone is not enough - it exists on
    /// a grid too, and a codec that fell back to byte buffers leaves it empty.
    ///
    /// A seek that has asked for a recording is the third: its range is read
    /// over the network before a decoder is even opened, so the gap can outlast
    /// the codec's handover grace. The surface still holds its last picture
    /// throughout, and the wall's only fallback texture is the camera's, frozen
    /// when playback opened - the frame every seek used to flash. The wait ends
    /// with `Playback::pending`, cleared by the first picture of the new
    /// session, so no clock has to guess how long a box may take to answer.
    /// Watches which camera the magnified picture is asking of the surface.
    ///
    /// A `SurfaceView` keeps the last frame it was given. Stepping to another
    /// camera asks for a different picture, but the old session can still be
    /// draining frames while the new one connects; the wall must not leave the
    /// area unpainted for those, or the viewer watches the camera just left
    /// (on a surface that holds it) or a black rectangle (on one that does not).
    /// Both are what a channel step showed on the devices. `begin_surface_picture`
    /// makes those late frames answer an older request, and the wall stands
    /// aside again only once a frame of *this* camera has arrived.
    #[cfg(target_os = "android")]
    pub(super) fn track_surface_picture(&mut self) {
        let cameras = self.config.active_cameras();
        let owner = self
            .scheduler
            .zoom()
            .and_then(|index| cameras.get(index))
            .map(|camera| camera.id.clone());
        if owner != self.surface_picture_owner {
            self.surface_picture_owner = owner.clone();
            self.surface_awaiting_first = owner.is_some();
            if owner.is_some() {
                monitor_codec::amediacodec::begin_surface_picture();
            }
            // The views are told in the same frame the step is noticed, before
            // `picture_on_layer` can hide one. A null owner means the viewport
            // is gone: that is what clears the kept frames off the grid.
            crate::android::set_video_owner(owner.as_deref());
        }
        if self.surface_awaiting_first
            && monitor_codec::amediacodec::surface_holds_picture()
        {
            self.surface_awaiting_first = false;
        }
    }

    /// The surface is the activity's own answer, not a desktop concern.
    #[cfg(not(target_os = "android"))]
    pub(super) fn track_surface_picture(&mut self) {}

    /// Whether a window this application opened is up, over the picture.
    ///
    /// The magnified picture can be a surface of the activity's, and a surface
    /// is drawn above everything the wall paints - that is what makes it
    /// zero-copy, and it is also why a window the wall draws gets cut in half by
    /// it: the file picker, opened over a magnified camera, showed its right
    /// half and the camera's picture where the left should have been.
    ///
    /// So the view is taken down while one of these is up, and put back when it
    /// closes. A `SurfaceView` keeps the last frame it was given, so nothing is
    /// decoded and no stream is reopened for it: the picture is there again on
    /// the frame the window goes away.
    pub fn modal_is_up(&self) -> bool {
        self.file_picker.is_open()
            || self.layout_picker.is_open()
            || self.portrait_aspect_picker.is_open()
            || self.combined_info_picker.is_open()
            || self.passphrase.is_open()
            || self.camera_edit.is_open()
            || self.remove_confirm.is_open()
            || self.system_confirm.is_open()
            || self.download_panel.is_open()
            || self.discovery.open
    }

    /// Whether this frame *wants* the picture on the activity's surface, with
    /// no regard for whether the view has reached the screen yet.
    ///
    /// This is the answer the loop uses to show the view, and the answer the
    /// wall uses to measure its box: both must be settled before the wall
    /// stands aside. The invariants behind it are the ones above -
    /// magnification, a page at rest, a picture of *this* camera.
    pub fn picture_layer_wanted(&self) -> bool {
        if !self.magnified() {
            return false;
        }
        // A page settling after a swipe is two cameras on the screen at once -
        // the one leaving and the one arriving - and a surface can stand over
        // one of them only. Left up, it holds whatever the session before the
        // step was drawing, at the box the *new* camera's shape asks for: one
        // half of the picture squeezed into the other's shape, over the wall's
        // own copy of it sliding by at its proper size. So the wall paints both
        // pages from its textures for the length of the settle, and the view
        // comes back when the page is at rest - one picture, one place.
        //
        // Only the settle, not the drag: a finger moving a page has not chosen
        // a camera yet, so the view still belongs to the one it shows.
        if self.slide.offset != 0.0 {
            return false;
        }
        // The picture the surface holds belongs to the camera being left.
        // The wall paints the wait - the canvas, the spinner, the "connecting"
        // panel - until the new session's first frame arrives. See
        // [`Self::track_surface_picture`].
        if self.surface_awaiting_first {
            return false;
        }
        if crate::picture_on_its_own_surface() {
            return true;
        }
        #[cfg(target_os = "android")]
        {
            if self
                .playback
                .as_ref()
                .is_some_and(|playback| playback.pending.is_some())
                && monitor_codec::amediacodec::surface_holds_picture()
            {
                return true;
            }
        }
        false
    }

    /// Whether the wall leaves the picture's area unpainted *this frame*.
    ///
    /// The activity moves its view on the UI thread, one frame after the
    /// request reaches it. Asking for the view and standing aside in the same
    /// frame leaves a frame on which the wall has painted nothing and the view
    /// is still off screen: the dark flash between the last low-resolution
    /// picture and the first high-resolution one. So the loop holds this
    /// answer back for the frame the request is made in. The wall goes on
    /// painting its sub-stream picture, and stands aside only once the view
    /// has had a whole frame to arrive. See [`Self::set_picture_layer_held`].
    pub fn picture_on_layer(&self) -> bool {
        !self.picture_layer_held && self.picture_layer_wanted()
    }

    /// Holds the wall's standing-aside back for a frame. Only the Android loop
    /// calls it; on the desktop the answer is always the wanted one.
    pub fn set_picture_layer_held(&mut self, held: bool) {
        self.picture_layer_held = held;
    }

    /// The rectangle the magnified picture occupies, in wall points, while a
    /// surface of its own is showing it.
    ///
    /// `None` when nothing is magnified. Set whether or not a surface is
    /// drawing the picture: the caller that places that view reads it, and so
    /// does the toast, which is laid out in this tile's frame when the picture
    /// in it is drawn turned. See `paint_toast`.
    pub fn picture_rect(&self) -> Option<egui::Rect> {
        self.picture_rect
    }

    /// Whether a page is moving - dragged by a finger, or settling after one.
    ///
    /// Dragged, the picture's view travels with the page as a translation and
    /// keeps the box it will come to rest in: a box per frame is a layout pass
    /// per frame, which resizes the surface the codec draws into and lands the
    /// picture behind its own page. Settling, the view is hidden for the length
    /// of it - two cameras are on screen then. See [`Self::picture_on_layer`].
    pub fn picture_sliding(&self) -> bool {
        self.slide.is_animating() || self.slide.dragging
    }

    /// How far the moving page is from settled, in wall points.
    pub fn picture_slide(&self) -> f32 {
        self.picture_slide
    }

    /// Whether the magnified picture is drawn a quarter turn.
    ///
    /// The wall draws it by sampling its texture sideways (see
    /// [`grid::picture_area`] and `grid::turned_canvas`). A picture scanned out
    /// of a surface has no texture to sample, and turning the view does not
    /// turn the frames: the surface loop reads this and suspends the surface
    /// path for as long as the turn is up, so the codec hands the frames back
    /// and the wall draws them itself. See
    /// `monitor_codec::amediacodec::set_surface_suspended`.
    pub fn picture_turned(&self) -> bool {
        self.magnified_rotated()
    }

    /// The tile the magnified picture belongs to, in wall points.
    ///
    /// Not the screen: the wall leaves a gutter for a navigation bar it keeps,
    /// and its bars claim their own bands, so the tile is what is left of the
    /// screen. The corners, the frame and the magnified bar are laid out
    /// against this rectangle in the wall's frame; the overlay has to lay its
    /// copy out against the very same one, or the two drift apart by the width
    /// of a gutter. See [`Self::draw_overlay_only`].
    pub fn magnified_tile(&self) -> Option<egui::Rect> {
        self.magnified_rect
    }

    /// The band the pinned strip stands in, in wall points, or `None` where the
    /// strip floats over the picture.
    ///
    /// As the frame painted it: the wall's own rectangle, with the page's slide
    /// already taken out. The overlay paints a copy of this band over a picture
    /// the surface is showing and is handed the tile the page has slid to, so it
    /// needs the band's own rectangle rather than a division of that tile - one
    /// measured there would travel with the page. See [`Self::draw_overlay_only`].
    pub fn strip_band(&self) -> Option<egui::Rect> {
        self.strip_band
    }

    /// What the magnified picture is showing of itself: the whole of it, or the
    /// window a pinch has left it at.
    ///
    /// The wall's own frames draw it as a crop of the texture; the surface path
    /// sends it on as a scale of the view, and both are the same window. See
    /// [`PictureZoom`] and `android::set_video_zoom`.
    pub fn picture_zoom(&self) -> PictureZoom {
        self.picture_zoom
    }

    /// Whether the magnified viewport's bar is up.
    pub fn bar_open(&self) -> bool {
        self.tile_bar.is_open()
    }

    /// The shape of the picture being magnified.
    ///
    /// From the last frame uploaded, which is the picture, while the wall draws
    /// it. While a surface of its own scans it out **no frame is uploaded at
    /// all**, so the last one belongs to whatever was decoded before the
    /// magnification - the sub stream of a channel that has just switched to its
    /// main one, say - and the box the picture is given would be that stream's
    /// shape. What the session reports it is decoding is the answer there, and it
    /// is not the texture's: `ChannelUi` is fed by the stream's own statistics.
    ///
    /// [`Self::picture_on_layer`] and not `picture_on_its_own_surface`: the
    /// latter drops to false while a seek waits for its new session, but the
    /// view is still up holding the last picture. Asking the wall's texture for
    /// a shape then is asking the camera's sub stream, frozen when playback
    /// opened - a 4:3 box for a 16:9 picture - and the view is resized to it
    /// for the length of the wait. The squeezed picture was the frame a seek
    /// used to flash.
    fn picture_size(&self, index: usize) -> Option<egui::Vec2> {
        if self.picture_layer_wanted() {
            if let Some(size) = self.channels.get(&index).and_then(|channel| {
                match (channel.width, channel.height) {
                    (Some(width), Some(height)) if width > 0 && height > 0 => {
                        Some(vec2(width as f32, height as f32))
                    }
                    _ => None,
                }
            }) {
                return Some(size);
            }
        }
        self.view_texture(index).map(|entry| entry.surface.size)
    }

    /// The picture's region and the strip's band, or `None` where the strip has
    /// no band of its own.
    ///
    /// Landscape, and a picture drawn turned, keep the strip floating over the
    /// picture where it has always been: nothing asks for a band there, and none
    /// is given. Upright and not turned, the strip is pinned and has one, and
    /// this is where the two regions come from - asked of the controls the
    /// frame is about to draw, so that the band deepens in the frame a recording
    /// starts rather than in the one after it.
    pub(super) fn panel_split(&self, area: Rect) -> Option<PanelSplit> {
        if !self.tile_bar_pinned() {
            return None;
        }
        let (entries, breaks) = self.tile_bar_entries();
        let widths = vec![tilebar::icon_width(); entries.len()];
        let playing = self.scheduler.zoom().is_some_and(|slot| self.is_playing_back(slot));
        Some(split_panel(area, &widths, &breaks, playing, self.tile_bar_margin()))
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
        // Upright and not turned, the picture has the region above the strip's
        // band, and the strip sits under it rather than over it. See `split_panel`.
        let rect = match self.panel_split(rect) {
            Some(split) => split.video,
            None => rect,
        };
        let camera = cameras.get(index)?;
        let size = self.picture_size(index)?;
        let aspect = self.tile_aspect(camera);
        // Upright, the magnified picture is the one that may be drawn sideways -
        // the same answer the tile is drawn with. See `paint_view`.
        Some(grid::picture_area(size, rect, aspect, self.magnified_rotated()))
    }

    /// Draws everything the wall would put over the picture, into a context of
    /// its own: the corner display, the bar with its timeline, and the toast.
    ///
    /// They need a surface of their own, over the picture, and a surface is a
    /// rectangle: what egui draws in it is all that shows. So this is that
    /// frame - the same corners and the same controls, in the same places,
    /// doing the same things - laid out against the picture rather than against
    /// the rectangle, so that it lands where the wall's own copy sits and takes
    /// the presses the wall reads.
    ///
    /// Nothing here reads input: the bar's presses are found in the wall's own
    /// frame, where the pointer and the remote already are. This frame paints -
    /// and the ring the remote puts on a control is mirrored into this context
    /// by the caller, which is the one thing painting alone cannot supply.
    ///
    /// `picture` is the wall's screen **in the strip's own coordinates** - the
    /// screen's size, shifted by the difference between the two surfaces'
    /// heights. Wall-space rectangles reach this frame by that shift.
    ///
    /// `body` is the magnified tile, in those same coordinates, with the gutters
    /// a kept navigation bar claims and the bands the wall's own bars claim
    /// already taken out. It, and not the whole strip, is what the corners, the
    /// frame and the controls are laid out against: the wall lays its copy out
    /// against its tile, and a layout against the screen would put every corner
    /// and every button under the system bar whenever a gutter was kept - seen
    /// here, taking presses over there.
    pub fn draw_overlay_only(&self, ctx: &egui::Context, picture: egui::Rect, body: egui::Rect) {
        // Only while the wall's own frame is being dropped by the compositor:
        // then the corners and the bar have nowhere else to be drawn. When the
        // wall is on screen it draws both itself, and a second copy here would
        // be a doubled OSD and a doubled bar. The overlay is see-through, so an
        // empty frame leaves the picture as it was.
        let open = self.tile_bar.is_open();
        if !self.picture_on_layer() {
            return;
        }
        let strip = ctx.screen_rect();
        if strip.width() < 24.0 || strip.height() < 8.0 {
            return;
        }
        let entries = self.tile_bar_entries().0;
        let widths = vec![tilebar::icon_width(); entries.len()];
        let breaks = self.tile_bar_break();
        let margin = self.tile_bar_margin();
        let dock = self.tile_bar.dock();
        // Where the strip floats over the picture - landscape, and a picture
        // drawn turned - it is given a timeline on the same terms the wall's own
        // copy is: only a recording has one to draw, and only a panel that fits
        // the tile is a strip of controls. Which is asked of the tile, because
        // the tile is what has to hold it; where it goes is asked of the tile
        // too, because that is where the bar belongs. A pinned strip has a band
        // and has answered both already. See `split_panel`.
        //
        // Turned, the strip runs down the side of the picture and its glyphs
        // turn with it. The wall lays out and draws its copy with `rotated`, so
        // this copy must use the same answer; a turn suspends the surface path
        // and leaves the wall's own frame as the one that is seen, but the frames
        // on either side of the switch still have to agree.
        let rotated = self.magnified_rotated();
        let playing = self.scheduler.zoom().is_some_and(|slot| self.is_playing_back(slot));
        // Upright and not turned the strip has a band of its own under the
        // picture, and the wall laid the bar out in that band: this copy has to
        // be laid out against the same one, or the two are a band apart. The
        // band's depth and whether it carries a timeline are the wall's answers
        // too - `split_panel` is where both are worked out, and the width it
        // asks about is the same width. See `split_panel`.
        //
        // The band is asked of the wall rather than of this tile: the tile
        // carries the page's slide, and the band does not travel with the page.
        // A band measured on the sliding tile left the picture's canvas standing
        // where the band had been, for as long as the finger was down. The
        // picture's region is still this tile's, and travels with it.
        let panel = self.panel_split(body);
        // The band comes from the wall's own frame, carried across into this
        // one. The division of the tile is the fallback for the one frame the
        // wall drew nothing at all - a page with no cells, a window down to a
        // postage stamp - and there is no band to stand in then, nor a picture
        // under it to show.
        let band = self
            .strip_band()
            .map(|band| band.translate(picture.min - strip.min))
            .or_else(|| panel.map(|panel| panel.bar));
        let (bar_body, timeline, backdrop) = match (panel, band) {
            (Some(panel), Some(band)) => (band, panel.timeline, tilebar::Backdrop::Bare),
            _ => (
                body,
                playing && tilebar::fits_timeline(body, rotated, margin, &widths, &breaks),
                tilebar::Backdrop::Floating,
            ),
        };
        // The picture's region, which is the tile everywhere but upright and
        // unturned: the corner display, the ring and the letterbox are all the
        // picture's, and the band below is the strip's.
        let video = panel.map_or(body, |panel| panel.video);
        let layout = tilebar::layout(
            bar_body, rotated, dock, margin, &widths, &breaks, timeline,
        );
        let controls: Vec<tilebar::Control> = entries.iter().map(|(control, _)| *control).collect();
        let muted: Vec<bool> = entries
            .iter()
            .map(|(_, action)| !self.tile_bar_available(*action))
            .collect();
        let tips = self.tile_bar_tips(&entries);
        // A navigation state of its own, thrown away with the frame. The walk is
        // the wall's - it is the wall's frame that reads the arrows - and these
        // same controls register there already; registering them here as well
        // would put every one of them on that walk twice.
        let mut nav = crate::nav::Nav::new(&[]);
        egui::Area::new(egui::Id::new("xgview-bar-surface"))
            .fixed_pos(strip.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                // The wall paints the whole of its area in `GRID_LINE` and then
                // lays the tile one point inside it, so a magnified picture has
                // a one-point frame around it; `live_canvas` paints the rest of
                // the tile in `CANVAS`, which is what shows beside a letterboxed
                // picture. Both are the wall's own frame, and that frame is
                // under the picture here - so both have to be painted on this
                // layer instead, or the picture comes up inside a black band
                // with no frame at all.
                //
                // The bands cover the box's outside only: a pinch scales the
                // view past its box, and the spill is covered rather than
                // clipped, which a surface cannot be. Without a pinch they are
                // the letterbox's own bars, in the same colour the wall used.
                // They are the picture's and reach no further than the
                // picture's region, so the band under them is never covered by
                // one. See `paint_band`.
                if let Some(band) = band {
                    paint_band(ui.painter(), band);
                }
                if let Some(box_) = self.picture_rect {
                    let box_ = box_.translate(picture.min - strip.min);
                    for letterbox in bands_around(video, box_) {
                        ui.painter().rect_filled(
                            letterbox,
                            egui::CornerRadius::ZERO,
                            theme::CANVAS,
                        );
                    }
                }
                ui.painter().rect_stroke(
                    body,
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(grid::LINE, theme::GRID_LINE),
                    egui::StrokeKind::Inside,
                );
                // The corner display first, in the place the wall would put it.
                // The wall draws it too, and that copy is the one nobody sees:
                // the picture is on a surface above it, and the wall's frame is
                // left transparent where the picture is - so the compositor
                // drops it entirely. These corners are the copy that can be
                // seen, and they are the same corners: `grid::osd`, the same
                // tile, the same place on the picture.
                if let Some(index) = self.scheduler.zoom() {
                    let cameras = self.config.active_cameras();
                    // Interactive, so the corners are drawn at full strength:
                    // `dim` is the page sliding out, and half a brightness here
                    // would be a corner display that is dimmer than the one the
                    // wall draws on the same picture. Nothing is pressed on this
                    // copy - the presses are the wall's - but the ring is the
                    // wall's too, and it is drawn here because this is the only
                    // layer that is not under the picture.
                    let tile = self.tile_of(
                        Id::new("xgview-overlay-tile"),
                        Some(index),
                        index,
                        &cameras,
                        1,
                        true,
                        self.ring_shown(),
                    );
                    // What the cell says when its picture is not simply
                    // playing - a failure panel, a suspended panel, the wait
                    // before a first frame, the band a stream switch leaves -
                    // for the reason the corners below are here: the wall draws
                    // it, and the picture is over the wall. Without this the
                    // screen is a frozen picture saying nothing, which is what
                    // the older front end never did.
                    if let Some(channel) = tile.channel {
                        // The wall's copy of this panel is laid out against the
                        // letterboxed picture, not the whole tile. This copy
                        // uses the same box, carried into the strip's frame.
                        let state_frame = tile
                            .state_frame
                            .map(|frame| frame.translate(picture.min - strip.min))
                            .unwrap_or(video);
                        grid::state_layer(ui.painter(), state_frame, &tile, channel.state);
                    }
                    grid::osd(ui.painter(), video, &tile);
                    // The cursor, on the tile: the ring the wall draws on that
                    // same rectangle. It is here and not there for the reason
                    // the corners are - the picture is over the wall - and on a
                    // letterboxed picture the wall's copy shows only where the
                    // two bands are, which reads as a frame with its top and
                    // bottom run through.
                    if tile.focused {
                        grid::ring(ui.painter(), video);
                    }
                }
                if open {
                    tilebar::draw(ui, &mut nav, &layout, rotated, backdrop, &controls, &muted, &tips);
                    // What the recording holds, in the row the strip's own
                    // geometry put there. The wall draws the same thing over the
                    // picture; this is the copy that can be seen.
                    if let Some(row) = layout.timeline {
                        self.draw_timeline(ui, row);
                    }
                }
                // The portrait viewport's page arrows, which the wall draws at
                // the edges of the area it lays the tile out in - so the picture
                // covers them. Up on the same terms as there, asked of the one
                // method both copies ask. A paint only: the presses are found
                // where the pointer and the remote are, in the wall's frame,
                // which already registered both circles.
                if self.page_arrows_shown() {
                    let focused = ctx.memory(|memory| memory.focused());
                    for (forward, id, has, center) in self.portrait_page_arrow_sides(video) {
                        if has {
                            Self::paint_page_arrow(
                                ui.painter(),
                                center,
                                forward,
                                focused == Some(Id::new(id)),
                            );
                        }
                    }
                }
            });
        // And the toast, which is not the bar's: a projection, an error, or the
        // name of the fit the switch has just moved to - this frame's news, in
        // the middle of the screen or at the foot of it. The wall paints the
        // same toast for every frame in which no picture is over it; while one
        // is, the wall's copy is under that picture and this is the copy that
        // can be seen. See `draw_toast`.
        self.paint_toast(ctx);
        // The question the first Back asks, which the wall puts in the middle of
        // the screen for the same reason - and which, being drawn after the
        // tiles, is on top of the wall's own frame and so of every picture over
        // it. This frame is the one that can be seen while a picture is up.
        self.draw_exit_hint(ctx);
        // And the blank, last of all and over everything: the wall paints it on
        // a layer above the toolbar and the tiles, and here the toolbar and the
        // corners have been painted above the picture, so it goes on last for
        // the same reason. It is painted rather than suspending the picture's
        // view because the blank is not a thing laid over a picture - it is the
        // screen going away - and a view taken off the layer for it would have
        // to be placed again on the way back, once per schedule period.
        if self.blackout_on {
            self.blackout.draw(ctx, self.time);
        }
    }

    /// One tile of the wall, as everything that paints it needs it.
    ///
    /// The magnified viewport is painted twice - by the wall, which is under the
    /// picture, and by the overlay over it - and both have to describe it the same
    /// way, or the corner display and the bar would disagree about what is on
    /// screen. `ring` is passed in rather than read here so that every tile of a
    /// frame gets the same answer; see `paint_view`.
    fn tile_of<'a>(
        &'a self,
        id: Id,
        index: Option<usize>,
        number: usize,
        cameras: &'a [CameraSource],
        cells: usize,
        interactive: bool,
        ring: bool,
    ) -> Tile<'a> {
        Tile {
            id,
            index,
            number,
            camera: index.and_then(|index| cameras.get(index)),
            // The picture, and the channel behind it, come from the
            // slot's view: a camera showing a recording is still that
            // camera for everything else - its name, its display, its
            // settings, its place on the wall. Until the recording's first
            // frame arrives the camera's own picture stays, so pressing the
            // control does not blank the viewport.
            channel: index.and_then(|index| self.view_channel(index)),
            video: index.and_then(|index| self.view_texture(index)).map(|entry| entry.surface),
            // The one picture scanned out of the activity's surface: the
            // magnified viewport. A single cell alone is not enough - a 1x1
            // grid has one too, and it paints its own picture from a frame.
            //
            // Nor is "a magnified viewport is up" enough: a page being slid
            // away is painted beside the one the view is showing, and the view
            // can only stand over one of them. Left on the layer, that second
            // page draws nothing at all - the wall leaves its cell blank and
            // the black slab slides out with the animation. So only the tile
            // the view is actually showing is left to it; the page leaving is
            // painted from its own frame, the way it was before the view
            // existed. See `picture_rect`, which follows the same tile.
            on_video_layer: cells == 1
                && self.picture_on_layer()
                && index == self.scheduler.zoom(),
            // The ring is a cursor, and a wall nobody is operating is just a
            // picture: it is drawn from the last key or press on and goes
            // once the wall has been left alone for the shorter of the ring's
            // own idle and the bars'. Which bars are up does not come into
            // it. See `ring_shown`.
            focused: interactive && index == self.scheduler.focus() && ring,
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
            // The moment the picture is of. A recording's frame carries its
            // own; before the first one has arrived the cursor's is the
            // moment being fetched, and "now" is the one thing it is not.
            clock: index
                .and_then(|index| self.view_texture(index))
                .and_then(|entry| entry.at)
                .or_else(|| index.and_then(|index| self.playback_clock(index))),
            aspect: index
                .and_then(|index| cameras.get(index))
                .map(|camera| self.tile_aspect(camera))
                .unwrap_or_default(),
            rotated: self.magnified_rotated(),
            // The magnified viewport is the one picture that can be pushed
            // in - it is the whole wall while it stands - and it is the only
            // tile that carries the window. Nothing else is zoomable, so
            // nothing else is given one.
            picture_zoom: (index.is_some() && index == self.scheduler.zoom())
                .then_some(self.picture_zoom),
            // The magnified picture is letterboxed, and a panel centred on the
            // whole tile lands above it - exactly what a viewer sees while the
            // next camera connects on a portrait wall. See `grid::state_layer`.
            // Only a magnified wall has a letterboxed picture, and its tile
            // does not always carry the slot's index - the page view asks for
            // a picture without one. The magnification is the answer here.
            state_frame: self
                .magnified()
                .then_some(self.picture_rect)
                .flatten(),
            // A recording keeps its picture through a seek, which reports
            // the state a live stream reports when it has nothing to show.
            // Everything the corners say about one comes with it: where it
            // is being read from, and what the picture is doing - moving,
            // held, or out of range. See [`grid::Recording`].
            recording: index.and_then(|slot| self.recording_mark(slot)),
            }
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
        // Read once for the frame: the same answer for every tile, and it is the
        // frame's clock rather than each cell's turn at it.
        let ring = self.ring_shown();

        for (cell, index) in view.cells.iter().enumerate() {
            let rect = grid::tile_rect(origin, view.layout, cell, grid::LINE);
            if rect.max.x < area.min.x - 4.0 || rect.min.x > area.max.x + 4.0 {
                continue;
            }
            let tile =
                self.tile_of(
                    Id::new(("xgview-tile", tag, cell, index.unwrap_or(usize::MAX))),
                    *index,
                    view.first + cell,
                    cameras,
                    view.cells.len(),
                    interactive,
                    ring,
                );
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
                // This needs no guard against the frame a finger lands on, the
                // way `drag_frame` does: a tile senses a click as well as a drag,
                // and `egui` postpones that decision until `is_decidedly_dragging`
                // - which is false on any frame a button went down (`any_pressed`).
                // So `dragged` cannot be true before the pointer has moved away
                // from where it landed, and the distance between two touches never
                // arrives as a page's travel. A tile that ever sensed *only* a
                // drag would be marked dragged on the press frame and would need
                // the same guard; see `drag_frame`.
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
        // The band is what this frame paints, and no other frame's: a frame that
        // draws no wall has none to show. See [`Self::strip_band`].
        self.strip_band = None;
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

        let view = self.current_view(total);

        // A page past the last camera has no cells at all. It is not a wall of
        // empty channels: the cameras exist, they are simply on another page -
        // which happens when cameras are removed and the page has not stepped
        // back yet. Normal paging never lands here, and the magnified view
        // always names a camera, so this is a guard rather than a state.
        if view.cells.iter().all(Option::is_none) {
            grid::empty_state(
                ui,
                area,
                &monitor_i18n::tr("wall-empty-page-title"),
                &monitor_i18n::tr_args("wall-empty-page-hint", &[("count", total.into())]),
            );
            return;
        }

        // The wall's own colour is the grid line: the tiles leave one line's
        // width around and between themselves, and it shows through. Drawing
        // it here rather than as a border on each tile is what keeps every line
        // one line wide - two neighbours would otherwise double up in the
        // middle while the outer ring stayed half as thick.
        //
        // Not while the picture is being scanned out under this window: that
        // fill would be opaque over it. A magnified view has one cell, so the
        // area being left alone is the picture's own. A one-cell grid is not
        // magnified and keeps its lines like every other grid.
        if !(view.cells.len() == 1 && self.picture_on_layer()) {
            ui.painter().rect_filled(area, egui::CornerRadius::ZERO, theme::GRID_LINE);
        }
        let wall = area.shrink(grid::LINE);

        // The strip's band, when it has one. Upright and not turned, the
        // magnified viewport is the picture above and the strip's band below
        // rather than one picture with a strip floating over it, and every
        // gesture that belongs to the picture - a pinch, a wheel, a drag - is
        // read against the picture's own region: two fingers on the band are
        // the band's, and a drag that starts there is not the picture moving.
        // `None` in landscape and for a picture drawn turned, where there is no
        // band and the whole area is the picture. See `split_panel`.
        let panel = self.panel_split(wall);
        let picture_area = panel.map_or(wall, |panel| panel.video);
        // Recorded for the overlay, which paints a copy of this band over a
        // picture the surface is showing and is handed only the tile the page
        // has slid to: the band is the wall's frame and stands where the wall
        // put it, whatever the picture under it is doing. See `strip_band`.
        self.strip_band = panel.map(|panel| panel.bar);

        // Where the focused tile is: the arrow that walks off the top of the
        // wall should come down into the bar above that tile rather than at one
        // end of the row.
        self.grid_focus_x = self.scheduler.focus().and_then(|index| {
            let cell = view.cells.iter().position(|slot| *slot == Some(index))?;
            Some(grid::tile_rect(wall, view.layout, cell, grid::LINE).center().x)
        });

        // Page sliding out, or the neighbour page revealed by a finger drag.
        //
        // A one-cell view is the magnified picture, whichever way it is going:
        // it slides through the picture's region, the same region the wall
        // draws the picture at rest in, so the one arriving and the one leaving
        // are the same size and stand at the same height as they pass. A grid
        // left behind by a page turn that was still running when the picture
        // was magnified is a grid, and goes out at the size it came in at - the
        // band covers its foot, because the band is painted after it.
        let slide_area = |layout: GridLayout| match layout {
            GridLayout::G1x1 => picture_area,
            _ => wall,
        };
        if let Some(outgoing) = self.slide.outgoing.clone() {
            let dx = (self.slide.offset - self.slide.dir) * area.width() + self.slide.drag;
            self.paint_view(ui, slide_area(outgoing.layout), &cameras, &outgoing, dx, false, 3);
        } else if self.slide.dragging && self.slide.drag.abs() > 1.0 {
            let forward = self.slide.drag < 0.0;
            if let Some(neighbour) = self.neighbour_view(forward, total) {
                let dx = if forward { self.slide.drag + area.width() } else { self.slide.drag - area.width() };
                self.paint_view(ui, slide_area(neighbour.layout), &cameras, &neighbour, dx, false, 3);
            }
        }

        // The band is the strip's ground, and the wall's own colour is under the
        // whole of its area: without this the band would be a slab of
        // `GRID_LINE` with the controls on it. Painted here rather than by the
        // strip, because it is the wall's frame - the ring around the area
        // starts on this paint - and after the pages rather than before them,
        // because a page on its way past is a picture and the band is not: it
        // goes behind the band, not over it. See `paint_band`.
        if let Some(panel) = panel {
            paint_band(ui.painter(), panel.bar);
        }

        let dx = self.slide.offset * area.width() + self.slide.drag;
        // What the magnified viewport shows of its picture, read and moved before
        // the tiles are drawn, so that this frame draws the window the gesture
        // has just left it at rather than the one before it. The gestures are the
        // frame's own input: while the magnification stands there is one picture
        // over the whole wall, and no tile beside it to catch a drag as one. See
        // `PictureZoom`.
        //
        // A picture on a surface of its own is the same window, sent to the view
        // that draws it: `android_surface` turns it into a scale of that view,
        // about the same box. It used to be kept whole here - the window moved a
        // rectangle the wall painted nowhere, and cost the drag its only gesture -
        // and a picture the surface draws is now one the surface can show a part
        // of. See `picture_on_layer`.
        // The box the picture occupies on the screen, for the view that is
        // showing it. Where a tile's picture lands inside its cell is the fit
        // switch's answer, and a surface of its own has to be given the same
        // one: left full screen, the picture is stretched into a screen its
        // stream never had, and the switch changes nothing on it.
        //
        // This is the *box*, not what the box shows: a window a pinch pushes
        // in is a crop of it, applied to the view as a scale of this rectangle
        // rather than as another rectangle. See `picture_zoom`.
        // Measured from what the frame *wants*, not from whether the wall has
        // stood aside: the view is given its box before it is shown, including
        // on the one frame the wall is still painting the sub-stream picture.
        // Left on the other answer the view came back at the last camera's box.
        self.picture_rect = match self.picture_layer_wanted() {
            true => self.magnified_area(&cameras, &view, wall, dx),
            false => None,
        };
        // How far the page is from settled, for the view that shows the picture:
        // its box is placed by a layout pass and the travel is a translation, so
        // the two arrive in time with different costs. See `picture_slide`.
        self.picture_slide = dx;
        // And the tile itself - not the picture's box: the ring is a cursor on
        // the tile, and the tile is the whole wall whether the picture is
        // letterboxed inside it or not. `paint_view` is given the same three
        // numbers below - the inset wall, the layout, the slide - so the overlay
        // draws the ring on exactly the rectangle the wall would have. See
        // `magnified_rect`.
        //
        // The tile stays the whole of the area upright and not turned too: the
        // band the strip stands in is part of the wall's frame, and the ring
        // around the whole frame is the one the viewer sees. The picture's own
        // region - which is what the ring is drawn on - is `picture_area`, and
        // `paint_view` and the overlay both use it. See `split_panel`.
        //
        // Recorded whether or not a surface is drawing the picture there: the
        // overlay needs it while one is, and the toast needs it while the wall
        // is drawing a turned picture itself - a toast over a turned picture is
        // laid out in that picture's frame, which is this tile. See
        // `paint_toast`.
        self.magnified_rect = self
            .scheduler
            .is_zoomed()
            .then(|| grid::tile_rect(wall.translate(vec2(dx, 0.0)), view.layout, 0, grid::LINE));
        let pinched = ui.ctx().input(|input| pinch_frame(input, picture_area));
        // Unless it is over the strip: two fingers there are asking for a wider
        // or narrower timeline, and the picture keeps the window it has.
        let pinched =
            pinched.filter(|(_, at)| !self.timeline_row(area).is_some_and(|row| row.contains(*at)));
        // A wheel is the same thing to the picture as a pinch - the fingers or
        // the mouse, pushed in about the point between them - and it is read
        // separately only because egui hands it over as a *scroll*: the interface
        // scrolls elsewhere, and the magnified viewport is the one place where
        // there is nothing to scroll. See `wheel_frame`.
        let speed = ui.ctx().options(|options| options.input_options.scroll_zoom_speed);
        let wheeled = ui.ctx().input(|input| wheel_frame(input, speed, picture_area));
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
                    // Two fingers that are already moving the window may also
                    // carry it: a pinch and a slide are one gesture to the hands.
                    // `drag_frame` declines while two are down - the pointer
                    // follows the first one and would double the movement - so
                    // this is where that half of it comes from, on both paths.
                    if self.picture_on_layer() {
                        if let Some(moved) = ui
                            .ctx()
                            .input(|input| input.multi_touch().map(|touch| touch.translation_delta))
                        {
                            if moved != egui::Vec2::ZERO {
                                self.picture_zoom = self.picture_zoom.panned(moved, dest);
                            }
                        }
                    }
                } else if self.picture_zoom.magnified() {
                    // A drag on the picture moves it; one that starts on the bar
                    // is the bar's. A finger's tap carries a little movement, and
                    // taking that as a pan would move the picture under the
                    // press that was meant for the strip.
                    //
                    // This is the same rule on a surface of its own, and it was
                    // wrong to make it different there: a picture the viewer has
                    // pushed in is a picture they are looking *at*, and one finger
                    // is how they look around it. A picture left whole still steps
                    // to the next channel on a drag, which is the gesture a
                    // magnified viewport answers when there is nothing to look
                    // around. See `keeps_the_page`.
                    let drag = ui.ctx().input(|input| {
                        drag_frame(input, picture_area).map(|delta| (delta, input.pointer.press_origin()))
                    });
                    if let Some((delta, origin)) = drag {
                        let on_bar = self.tile_bar.is_open()
                            && origin.is_some_and(|origin| match panel {
                                // Upright and not turned, the band is the bar:
                                // a finger put down anywhere in it belongs to
                                // the strip, whether or not it landed on a
                                // control. See `split_panel`.
                                Some(panel) => panel.bar.contains(origin),
                                None => in_tile_bar_strip(
                                    origin,
                                    wall,
                                    self.magnified_rotated(),
                                    self.tile_bar.dock(),
                                ),
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

        let mut actions = self.paint_view(ui, picture_area, &cameras, &view, dx, true, 2);

        // A drag on the magnified picture is the picture moving, not the page:
        // the wall has one picture on it and nowhere to turn to, and the channel
        // after it is reached from the toolbar's arrows or the page keys, which
        // step a magnified viewport already. A pinch is not a drag either, from
        // the frame the second finger lands: the pointer follows the first one,
        // and letting this through would turn a page under the fingers. See
        // `step_zoom`.
        //
        // A picture left whole is the other way round: a drag on it is the step
        // to the next channel, which is what a magnified viewport answers when
        // there is nothing to look around. That is the rule on both paths - the
        // picture on a surface of its own pans while it is pushed in and steps
        // while it is not, exactly as the one the wall draws does.
        //
        // A finger on the timeline is not a finger on the page: what it is
        // dragging is a recording, and the picture must not slide out from under
        // it or the page turn over when it lets go. Read before the bar is
        // answered, because this frame's release is the bar's to act on and not
        // the wall's.
        let scrubbing = self.playback.as_ref().is_some_and(|playback| playback.scrubbing);
        let keeps_the_page = pinched.is_some() || scrubbing || self.picture_zoom.magnified();
        if self.scheduler.is_zoomed() && keeps_the_page {
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
        // Upright and not turned the strip has a band of its own under the
        // picture, and that band is the tile the bar is laid out in: `layout`
        // adds this tile's corner to everything it places, so a band measured up
        // from the bottom of the wall lands exactly where the wall put it. See
        // `split_panel`.
        //
        // Measured on the wall, not on the tile the slide has carried: the band
        // is the wall's frame and stands still while the picture travels, and a
        // bar laid out in the travelling tile would cross the band it belongs
        // to. Drawn and pressed must be the same answer, so it is the band's.
        let panel = self.panel_split(wall);
        // A window too small to hold the strip and the room it keeps clear of
        // the edge is not given one; this is a picture down to a postage stamp.
        // A pinned strip is left standing rather than taken down: the net at the
        // start of every frame holds it up for as long as the picture is
        // magnified, so hiding it here would be a flag put down every frame and
        // a strip drawn on none of them. The band stays, empty. See
        // `tile_bar_pinned`.
        let picture = panel.map_or(rect, |panel| panel.video);
        if picture.width() < 120.0 || picture.height() < 120.0 {
            if !self.tile_bar_pinned() {
                self.tile_bar.hide();
            }
            return false;
        }

        let rotated = self.magnified_rotated();
        // The panel carries a timeline whenever a recording is playing, and the
        // row goes wherever the panel is: in the black a letterboxed picture
        // leaves, or over the picture itself when it fills the tile - which is
        // what a landscape wall does with a landscape camera, and a wall with no
        // timeline at all is a recording that cannot be scrubbed. The row is
        // given only to a tile with room for both of the panel's rows: a panel
        // deeper than the picture is no longer a strip of controls.
        let playing = self.scheduler.zoom().is_some_and(|slot| self.is_playing_back(slot));
        // The controls are read again below, after the press, so a press that
        // changes the picture fit also redraws the bar with the set it has in the
        // same frame it changes it. Every control is a square.
        let controls = self.tile_bar_controls();
        let widths = vec![tilebar::icon_width(); controls.len()];
        let breaks = self.tile_bar_break();
        // How deep the panel is depends on how many lines the controls need, so
        // the timeline is only promised to a tile that has room for both.
        //
        // A pinned strip has answered both questions already: its band is as
        // deep as the row it was measured with, and it gave the row up rather
        // than take it out of the picture's half. See `split_panel`.
        let margin = self.tile_bar_margin();
        let (tile, timeline, backdrop) = match panel {
            Some(panel) => (panel.bar, panel.timeline, tilebar::Backdrop::Bare),
            None => (
                rect,
                playing && tilebar::fits_timeline(rect, rotated, margin, &widths, &breaks),
                tilebar::Backdrop::Floating,
            ),
        };
        let layout = tilebar::layout(
            tile,
            rotated,
            self.tile_bar.dock(),
            margin,
            &widths,
            &breaks,
            timeline,
        );
        let now = Instant::now();
        let mut consumed = false;

        // A press on the picture closes the bar only when it was a tap: quick,
        // still, and the only finger down. A press that lingered is a thumb left
        // on the glass, one that slid is a pan being reached for, and a pinch is
        // two fingers - the bar stays through all of them. See `tap_release`.
        let tap = self.tap_release(ui.ctx());
        if let (Some(pos), Some(panel)) = (actions.click_pos, panel) {
            // Upright and not turned, the strip is pinned: it is up and stays up,
            // and its band is the bar. A press in the band belongs to the strip
            // whether or not it landed on a control - the padding between them is
            // still the bar being used - and a press on the picture above is the
            // picture's, with nothing here to open or close. See
            // `tile_bar_pinned`.
            if panel.bar.contains(pos) {
                self.tile_bar.touch(now);
                consumed = true;
            }
        } else if let Some(pos) = actions.click_pos {
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
        let entries = self.tile_bar_entries().0;
        let widths = vec![tilebar::icon_width(); entries.len()];
        let breaks = self.tile_bar_break();
        let layout = tilebar::layout(
            tile,
            rotated,
            self.tile_bar.dock(),
            margin,
            &widths,
            &breaks,
            timeline,
        );

        // A double press in the band is the bar being asked for, not the
        // viewport being put back: its first press has already opened the bar,
        // and the second is the same gesture.
        if let Some(pos) = actions.double_click_pos {
            let on_bar = match panel {
                Some(panel) => panel.bar.contains(pos),
                None => !matches!(layout.hit(pos), tilebar::Hit::Outside),
            };
            if on_bar {
                consumed = true;
            }
        }

        if self.tile_bar.is_open() {
            // The controls are egui's own buttons, placed in the picture's own
            // frame: a press lights the one it went down on, and the ring is the
            // focus the navigation layer puts there. The confirm key is answered
            // by `handle_keys` before this frame is drawn and is taken out of
            // the queue there, so it cannot arrive a second time as egui's fake
            // click on the focused widget. What is left here is the pointer
            // press, the guard - the gesture that opened the bar must not also
            // press it - and the act.
            let controls: Vec<tilebar::Control> =
                entries.iter().map(|(control, _)| *control).collect();
            // A control with nothing beyond it in the direction it points - a step
            // off the end of the recording, a wider strip than the ladder has - is
            // drawn grey rather than offering a press that does nothing. It keeps
            // its place: see `tilebar::draw`.
            let muted: Vec<bool> = entries
                .iter()
                .map(|(_, action)| !self.tile_bar_available(*action))
                .collect();
            // What each control will do, for a pointer resting on it. The bar's
            // shapes are read by a viewer who knows them; the words are here for
            // the one who does not, the way the toolbars carry theirs.
            let tips = self.tile_bar_tips(&entries);
            if let Some(index) =
                tilebar::draw(ui, &mut self.nav, &layout, rotated, backdrop, &controls, &muted, &tips)
            {
                // A window of the viewer's own owns the screen while it is up;
                // the bar stays drawn behind it. This guard is the counterpart
                // of the one on the manual confirm-key branch in `handle_keys`.
                if !self.modal_is_up() && !self.tile_bar.ignoring(now) {
                    // Was the ring on a control, or still on the seat? The
                    // difference is whether the remote is driving the bar.
                    let driven = ui
                        .ctx()
                        .memory(|memory| memory.focused())
                        .is_some_and(|id| id != tilebar::seat());
                    let talking = self.talking;
                    self.tile_bar.touch(now);
                    if let Some((_, action)) = entries.get(index) {
                        self.tile_bar_activate(*action, ui.ctx());
                        // A press with the ring on the strip is the same
                        // question the remote asks, and it gets the same
                        // answer: the ring follows the control that undoes the
                        // switch. See [`Self::ring_follows_the_switch`].
                        self.ring_follows_the_switch(ui.ctx(), *action);
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
            // On top of the strip and its controls: what the recording holds is
            // drawn by the one thing that knows it, and the row it is drawn in
            // is the strip's own geometry.
            self.timeline_shown = layout.timeline.is_some();
            if let Some(row) = layout.timeline {
                // Answered first, so a finger that moves this frame moves the
                // cursor in it.
                self.timeline_input(ui, row);
                self.draw_timeline(ui, row);
            }
        }

        // A finger on the strip is not the remote taking it. The ring is the
        // keyboard's cursor, and a viewer who pressed a button with a finger
        // has not asked for the next arrow to walk the strip rather than the
        // picture. The press itself is answered above; what it must not do is
        // leave the ring behind - egui puts the focus on a widget it is clicked
        // on, which is exactly the state that would take the arrows.
        //
        // Read from the pointer rather than from the tile's own actions: a
        // press that lands on the strip never reaches the tile, so the tile has
        // no press to report and a rule asked of it would never fire.
        //
        // A pinned strip only. A floating one is a layer the viewer opened, and
        // a layer keeps the keys until it is closed: that is what it always did,
        // and nothing about a band under the picture changes it.
        if self.tile_bar_pinned() && ui.input(|input| input.pointer.any_pressed()) {
            let pos = ui.input(|input| input.pointer.interact_pos());
            let on_bar = pos.is_some_and(|pos| match panel {
                Some(panel) => panel.bar.contains(pos),
                None => !matches!(layout.hit(pos), tilebar::Hit::Outside),
            });
            if on_bar {
                self.leave_controls(ui.ctx());
            }
        }

        consumed
    }

    /// The first control on the strip that can be pressed, in walk order.
    ///
    /// A greyed control is not in the walk and takes no press, so it is not a
    /// place the ring may land: the confirm key on a pinned strip asks for the
    /// ring, and a ring put on a control that answers nothing is the same as no
    /// ring at all. `None` when the whole strip is grey - the ring then falls
    /// back to the seat, which draws nothing and is where it belongs.
    pub(super) fn first_live_control(&self) -> Option<usize> {
        let (entries, _) = self.tile_bar_entries();
        (0..entries.len()).find(|index| self.tile_bar_available(entries[*index].1))
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
    /// The controls the bar carries, and where its second line begins.
    ///
    /// The break is the bar's own rather than a width's: a recording has twelve
    /// controls and wants them as the transport and the timeline's own, which is
    /// a grouping a greedy fill would not find on a wide picture. `None` is one
    /// line, which is what a live camera has.
    /// Whether a control on the bar would do anything.
    ///
    /// One pair has an end this side can know: the two that widen the strip, where
    /// `step_zoom` has no next step at either end of the ladder. They are drawn
    /// grey and take no press. See `tilebar::draw`.
    ///
    /// **The rest are always drawn live**, because none of them has an answer that
    /// is a fact before the press. A step off the end of a recording takes the
    /// picture to the end, which is what the press asked for; an event jump reads
    /// a day of events before it says there is none; and a recording just opened
    /// with no event ahead of it is the case a viewer notices, where greying would
    /// be a claim this side cannot make. **A control that is right most of the
    /// time is worse than one that never claims to know.**
    pub(super) fn tile_bar_available(&self, action: TileBarAction) -> bool {
        // A grey speaker and a waiting mark are places in the row, not
        // controls: nothing can land on either or walk onto either. `tilebar`
        // draws them inert on its own as well, so the two answers cannot
        // disagree about them.
        if matches!(action, TileBarAction::NoSound | TileBarAction::Waiting) {
            return false;
        }
        // One download at a time. The mute is what stops a second press, and
        // `start_download` checks again in case a path around the focus walk
        // ever reaches it.
        if matches!(action, TileBarAction::Download) {
            return !self.download.is_active();
        }
        let Some(playback) = self.playback.as_ref() else {
            return true;
        };
        match action {
            TileBarAction::ZoomIn => {
                playback.timeline == playback::Timeline::Span
                    && playback::step_zoom(playback.zoom, true).is_some()
            }
            TileBarAction::ZoomOut => {
                playback.timeline == playback::Timeline::Span
                    && playback::step_zoom(playback.zoom, false).is_some()
            }
            _ => true,
        }
    }

    /// Where the ring goes when the control under it has gone grey.
    ///
    /// `focused` is the focus egui still holds at the end of the frame, and
    /// `focus_before` is where the ring stood when the frame began. egui gives
    /// the focus up *as it draws* a control that has gone grey - a muted button
    /// is a `Sense::hover()` widget, which is not focusable, so `create_widget`
    /// calls `surrender_focus` on it. A press answered before the bar is drawn
    /// (the confirm key in `handle_keys`) therefore greys its own control in the
    /// same frame it is drawn: by the end of that frame there is no focus left
    /// to read, even though the ring was plainly on a control a moment ago.
    /// The place it started from is the one the next live control takes.
    ///
    /// A focus that moved somewhere else on purpose is not ours to override:
    /// only a frame that ended with no focus at all looks back at the start.
    pub(super) fn grey_handoff(
        focused: Option<Id>,
        focus_before: Option<Id>,
        live: &[bool],
    ) -> Option<Id> {
        let index = match focused {
            Some(id) => tilebar::index_of(id),
            None => focus_before.and_then(tilebar::index_of),
        }?;
        let Some(&is_live) = live.get(index) else {
            return None;
        };
        if is_live {
            return None;
        }
        Some(tilebar::next_live(live, index).map_or_else(tilebar::seat, tilebar::control))
    }

    /// Hands the focus on from a control that has just gone grey.
    ///
    /// A greyed control takes no press and is not in the walk, so a focus left on
    /// one is a focus with nowhere to go: the arrows stop dead at it, which is
    /// what a viewer pressing "more detail" at the end of the ladder saw - the
    /// ring went, and the row could not be walked past it. The focus goes to the
    /// next live control instead, wrapping round the row, and to the seat when the
    /// whole row is grey. See [`grey_handoff`] for why the frame's starting focus
    /// has to be part of the answer.
    pub(super) fn step_off_a_greyed_control(
        &mut self,
        ctx: &egui::Context,
        focus_before: Option<Id>,
    ) {
        let focused = ctx.memory(|memory| memory.focused());
        // Only the wall can be given a bar control back. A window over it owns
        // whatever focus is left, and a frame that ends with none while a window
        // is up belongs to that window's hand-off, not to this one.
        let before = if focused.is_none()
            && !self.modal_is_up()
            && self.panel_anchor().is_none()
        {
            focus_before
        } else {
            None
        };
        let entries = self.tile_bar_entries().0;
        let live: Vec<bool> = entries
            .iter()
            .map(|(_, action)| self.tile_bar_available(*action))
            .collect();
        if let Some(target) = Self::grey_handoff(focused, before, &live) {
            ctx.memory_mut(|memory| memory.request_focus(target));
        }
    }

    pub(super) fn tile_bar_entries(&self) -> (Vec<(tilebar::Control, TileBarAction)>, Vec<usize>) {
        if self.talking {
            return (vec![(tilebar::Control::Hangup, TileBarAction::Hangup)], Vec::new());
        }
        // The box owes this picture an answer. Until it lands the strip is one
        // waiting mark and nothing else: a live row would still offer the
        // microphone, a second way into the recording, a turn of the picture
        // and a walk of controls that are all about to be replaced. A press
        // answered by any of them is a press answered by a picture that is no
        // longer there. See [`Self::playback_is_asking`].
        if self.playback_is_asking() {
            return (vec![(tilebar::Control::Waiting, TileBarAction::Waiting)], Vec::new());
        }
        let mut entries = Vec::new();
        let playing = self.scheduler.zoom().is_some_and(|channel| self.is_playing_back(channel));
        // The quarter turn belongs to the live picture, and to the live picture
        // only: a recording is not something to turn - the strip under it is the
        // same length whichever way up the picture is, and the button would sit
        // beside play and pause where a mis-press costs the picture.
        if !playing && self.is_portrait() {
            entries.push((tilebar::Control::Icon(Icon::Rotate), TileBarAction::Rotate));
        }
        // A recording has its own controls, and they are not the live ones: what
        // a viewer watching the past wants is the picture held and moved, not
        // the microphone and the way in.
        if playing {
            let play = if self.playback_still() { Icon::Play } else { Icon::Pause };
            let speed = tilebar::Control::Label(self.playback_speed_label());
            let sound = tilebar::SOUND && self.magnified_has_audio();
            let sound_icon = if self.sound_on { Icon::Sound } else { Icon::Mute };
            let (controls, breaks) = Self::recording_entries(
                play,
                speed,
                sound.then_some(sound_icon),
                self.download_control(),
                self.is_portrait() && !self.magnified_rotated(),
            );
            return (controls, breaks);
        }
        entries.push((tilebar::Control::Icon(Icon::AspectRatio), TileBarAction::Fit));
        // The sound switch keeps its place whether the stream has an audio
        // track or not. A switch that is taken and given back takes every
        // control after it with it, and the row is drawn again for every
        // picture: a row that shifts under the remote's ring is a row the
        // viewer cannot aim at. The place is kept, with a grey speaker in it.
        entries.push(if tilebar::SOUND && self.magnified_has_audio() {
            let icon = if self.sound_on { Icon::Sound } else { Icon::Mute };
            (tilebar::Control::Icon(icon), TileBarAction::Sound)
        } else {
            (tilebar::Control::Silent, TileBarAction::NoSound)
        });
        // The live bar only: a microphone is something to hold to a camera, and
        // there is nothing behind a recording to hear it.
        if self.magnified_has_backchannel() {
            entries.push((tilebar::Control::Icon(Icon::Mic), TileBarAction::Mic));
        }
        // The way into this camera's recordings, for a camera that came from a
        // server which keeps them. It is the last control: the ones before it
        // are about watching this stream, and this one is about leaving it.
        // Not while it is already playing: the way in is not also the way out,
        // and pressing it again would ask for the same stretch from its first
        // frame. Back leaves the viewport, and the recording with it.
        if self.magnified_camera().is_some_and(|camera| camera.frigate.is_some()) {
            entries.push((tilebar::Control::Icon(Icon::History), TileBarAction::Playback));
        }
        (entries, Vec::new())
    }

    /// The save button is a shape until a clip is being saved, and the word of
    /// its progress while one is. The width does not change: a label asks for
    /// the same square an icon does.
    fn download_control(&self) -> tilebar::Control {
        match self.download.progress() {
            Some(progress) => tilebar::Control::Progress(progress),
            None => tilebar::Control::Icon(Icon::Download),
        }
    }

    /// Where the save button stands in the strip this frame, as the focus knows
    /// it.
    ///
    /// The button's place rather than a copy of its id: the set of controls
    /// changes with the picture - a recording has no microphone, a live picture
    /// has no save - so the place is asked of the entries being drawn rather
    /// than remembered. `None` while the bar carries no save button at all,
    /// which is every picture but a recording.
    pub(super) fn download_button(&self) -> Option<egui::Id> {
        Self::download_button_in(&self.tile_bar_entries().0)
    }

    /// The same, read off the controls a strip is drawn with.
    ///
    /// Split out so the answer can be pinned by a test: the save button's
    /// place is not the same on a phone held upright and on a wide or turned
    /// picture, and the close of the panel hands the focus to it by that place.
    pub(super) fn download_button_in(entries: &[(tilebar::Control, TileBarAction)]) -> Option<egui::Id> {
        entries
            .iter()
            .position(|(_, action)| *action == TileBarAction::Download)
            .map(tilebar::control)
    }

    /// The recording's controls and where their lines break, as a pure rule.
    ///
    /// Kept apart from the wall's state so the order a remote walks can be
    /// pinned down by a test: on a wide picture - and on a portrait one turned
    /// a quarter turn - one line with the save just after the day picker; on a
    /// phone held upright three lines, `[Live]` alone first and the six
    /// timeline keys last. The sound switch is the one control a camera may
    /// not have; its place is kept either way, so the breaks do not move.
    fn recording_entries(
        play: Icon,
        speed: tilebar::Control,
        sound: Option<Icon>,
        download: tilebar::Control,
        three_lines: bool,
    ) -> (Vec<(tilebar::Control, TileBarAction)>, Vec<usize>) {
        let keys = [
            (Icon::EventBack, TileBarAction::PrevEvent),
            (Icon::EventForward, TileBarAction::NextEvent),
            (Icon::StepBack, TileBarAction::StepBack),
            (Icon::StepForward, TileBarAction::StepForward),
            (Icon::TimelineZoomIn, TileBarAction::ZoomIn),
            (Icon::TimelineZoomOut, TileBarAction::ZoomOut),
        ];
        let icon = |icon: Icon, action: TileBarAction| (tilebar::Control::Icon(icon), action);
        let mut entries = Vec::new();
        let mut breaks = Vec::new();
        if three_lines {
            entries.push(icon(Icon::Live, TileBarAction::Live));
            breaks.push(entries.len());
            entries.push(icon(play, TileBarAction::PlayPause));
            entries.push((speed, TileBarAction::Speed));
            entries.push(icon(Icon::Calendar, TileBarAction::Date));
            entries.push(icon(Icon::AspectRatio, TileBarAction::Fit));
            // The sound switch keeps its place whether the recording carries
            // sound or not - see the live row above.
            entries.push(match sound {
                Some(sound) => icon(sound, TileBarAction::Sound),
                None => (tilebar::Control::Silent, TileBarAction::NoSound),
            });
            entries.push((download, TileBarAction::Download));
            breaks.push(entries.len());
            for (glyph, action) in keys {
                entries.push(icon(glyph, action));
            }
        } else {
            entries.push(icon(play, TileBarAction::PlayPause));
            entries.push((speed, TileBarAction::Speed));
            for (glyph, action) in keys {
                entries.push(icon(glyph, action));
            }
            entries.push(icon(Icon::Calendar, TileBarAction::Date));
            // A wide picture and a portrait picture turned a quarter turn read
            // the same one line. The save follows the day picker: both ask
            // which stretch of the recording is in question, so a remote walks
            // from one to the other without crossing the picture's controls.
            entries.push((download, TileBarAction::Download));
            entries.push(icon(Icon::AspectRatio, TileBarAction::Fit));
            // The sound switch keeps its place whether the recording carries
            // sound or not - see the live row above.
            entries.push(match sound {
                Some(sound) => icon(sound, TileBarAction::Sound),
                None => (tilebar::Control::Silent, TileBarAction::NoSound),
            });
            entries.push(icon(Icon::Live, TileBarAction::Live));
        }
        (entries, breaks)
    }

    /// Where the timeline row is on screen this frame, if there is one.
    ///
    /// The wall asks before it gives a pinch to the picture: a gesture made over
    /// the strip is the strip's - it changes how much of the recording is shown,
    /// not how much of the picture - the same way one made over a panel belongs
    /// to the panel. See [`XgViewApp::timeline_input`].
    pub(super) fn timeline_row(&self, tile: Rect) -> Option<Rect> {
        let playing = self.scheduler.zoom().is_some_and(|slot| self.is_playing_back(slot));
        if !playing {
            return None;
        }
        let rotated = self.magnified_rotated();
        let (controls, breaks) = self.tile_bar_entries();
        let widths = vec![tilebar::icon_width(); controls.len()];
        let margin = self.tile_bar_margin();
        if !tilebar::fits_timeline(tile, rotated, margin, &widths, &breaks) {
            return None;
        }
        let layout = tilebar::layout(
            tile,
            rotated,
            self.tile_bar.dock(),
            margin,
            &widths,
            &breaks,
            true,
        );
        layout.timeline
    }

    /// How far in from the docked edge the panel sits.
    ///
    /// Twelve on a wide picture and on a turned one, where the panel is over the
    /// picture and every point of it is picture given away; twenty-four on a
    /// phone held upright, where it sits in the black under the picture and
    /// there is nothing to give away. The margin along the strip does not change
    /// - that one keeps the bar clear of the screen's own gesture band.
    pub(super) fn tile_bar_margin(&self) -> f32 {
        if self.is_portrait() && !self.magnified_rotated() {
            tilebar::EDGE
        } else {
            tilebar::EDGE * 0.5
        }
    }

    /// The indices at which a recording's controls start a new line. Empty on
    /// a wide picture, `[1, 7]` on a phone held upright, whichever the
    /// stream's audio turns out to be: live alone, then the transport and the
    /// save, then
    /// the timeline's own keys.
    pub(super) fn tile_bar_break(&self) -> Vec<usize> {
        self.tile_bar_entries().1
    }

    /// The controls alone, which is what the layout and the paint need.
    pub(super) fn tile_bar_controls(&self) -> Vec<tilebar::Control> {
        self.tile_bar_entries().0.into_iter().map(|(control, _)| control).collect()
    }

    /// The word under each control of the magnified bar, for a pointer resting
    /// on it.
    ///
    /// The bar is a row of shapes, and a shape is only a word to a viewer who
    /// already knows it - which is what the toolbars' bubbles are for. Each says
    /// what the press will do from where the picture is now, so a triangle that
    /// will pause says "pause" and a calendar that will go back says so.
    pub(super) fn tile_bar_tips(
        &self,
        entries: &[(tilebar::Control, TileBarAction)],
    ) -> Vec<String> {
        entries.iter().map(|(_, action)| self.tile_bar_tip(*action)).collect()
    }

    fn tile_bar_tip(&self, action: TileBarAction) -> String {
        use monitor_i18n::tr;
        match action {
            TileBarAction::Rotate => tr("tilebar-rotate-tip"),
            TileBarAction::Fit => tr("tilebar-fit-tip"),
            TileBarAction::Sound => {
                tr(if self.sound_on { "tilebar-sound-off-tip" } else { "tilebar-sound-on-tip" })
            }
            TileBarAction::Mic => tr("tilebar-mic-tip"),
            TileBarAction::Hangup => tr("tilebar-hangup-tip"),
            TileBarAction::Playback => tr("tilebar-playback-tip"),
            TileBarAction::StepBack => tr("tilebar-step-back-tip"),
            TileBarAction::StepForward => tr("tilebar-step-forward-tip"),
            TileBarAction::PlayPause => {
                tr(if self.playback_still() { "tilebar-play-tip" } else { "tilebar-pause-tip" })
            }
            TileBarAction::PrevEvent => tr("tilebar-prev-event-tip"),
            TileBarAction::NextEvent => tr("tilebar-next-event-tip"),
            TileBarAction::ZoomIn => tr("tilebar-zoom-in-tip"),
            TileBarAction::ZoomOut => tr("tilebar-zoom-out-tip"),
            TileBarAction::Speed => tr("tilebar-speed-tip"),
            TileBarAction::Date => {
                let days = self
                    .playback
                    .as_ref()
                    .is_some_and(|playback| playback.timeline == playback::Timeline::Days);
                tr(if days { "tilebar-date-back-tip" } else { "tilebar-date-tip" })
            }
            TileBarAction::Live => tr("tilebar-live-tip"),
            // The grey speaker says why it cannot be pressed, and the waiting
            // mark says what the strip is waiting for.
            TileBarAction::NoSound => tr("tilebar-no-sound-tip"),
            TileBarAction::Waiting => tr("tilebar-waiting-tip"),
            TileBarAction::Download => {
                if self.download.is_active() {
                    monitor_i18n::tr_args(
                        "tilebar-download-running-tip",
                        &[("camera", self.download.camera().unwrap_or_default().into())],
                    )
                } else {
                    match self.download.hint() {
                        Some(super::download::Last::Done { path, .. }) => monitor_i18n::tr_args(
                            "download-hint-done",
                            &[("path", path.clone().into())],
                        ),
                        Some(super::download::Last::Failed { reason, .. }) => monitor_i18n::tr_args(
                            "download-hint-failed",
                            &[("error", reason.clone().into())],
                        ),
                        None => tr("tilebar-download-tip"),
                    }
                }
            }
        }
    }

    /// Answers a control of the magnified bar: the handset that ends a call, the
    /// quarter turn, the picture fit, the sound switch when the camera has sound,
    /// or the microphone when it has a back channel.
    ///
    /// It is the action of the control the cursor is on for a key and of the one
    /// a finger landed on - a press acts on what it hit without moving the
    /// cursor. `ctx` is passed on to the one control that opens a window, which
    /// has to know where the focus stood when it was asked for.
    pub(super) fn tile_bar_activate(&mut self, action: TileBarAction, ctx: &egui::Context) {
        tracing::debug!(target: "xgview::app", ?action, "tile bar control pressed");
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
            TileBarAction::Playback => {
                // The control is only drawn for a camera that has recordings,
                // and only while magnified - which is the one viewport a
                // recording can be played in.
                self.enter_playback();
            }
            TileBarAction::StepBack => self.step_playback(-playback::STEP),
            TileBarAction::StepForward => self.step_playback(playback::STEP),
            TileBarAction::PlayPause => self.toggle_play(),
            TileBarAction::PrevEvent => self.jump_event(false),
            TileBarAction::NextEvent => self.jump_event(true),
            TileBarAction::ZoomIn => self.zoom_timeline(true),
            TileBarAction::ZoomOut => self.zoom_timeline(false),
            TileBarAction::Speed => self.cycle_speed(),
            TileBarAction::Date => self.toggle_calendar(),
            TileBarAction::Download => {
                // The button is muted while a clip is being saved, but a key
                // held down can reach the action twice in one frame; the state
                // is the second line of defence.
                if !self.download.is_active() {
                    self.open_download_panel(ctx);
                }
            }
            TileBarAction::Live => self.leave_playback(),
            // A grey speaker and a waiting mark answer nothing, and there is
            // nothing to press: they are places kept, not controls.
            TileBarAction::NoSound | TileBarAction::Waiting => {}
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

    /// Keeps the ring on the control the press turned into.
    ///
    /// Starting a call replaces the whole row with the handset, and ending one
    /// puts the microphone back at an index the call's single control never had.
    /// What the focus holds is `control(index)`, and after the change that index
    /// no longer names the thing the viewer pressed: the widget is gone, egui
    /// drops the focus with it, and the handset stood there unlit - a viewer who
    /// had just started a call from the microphone had to walk to the one
    /// control left. The ring goes where the press went, because the microphone
    /// and the handset are the two ends of the same key.
    ///
    /// Only a ring that was on the row is moved. A finger that never took the
    /// keyboard leaves the focus where the pointer rules put it, and the drawn
    /// bar answers that press itself - see the pointer branch of
    /// `tile_bar_overlay`, which asks the same question for a finger.
    pub(super) fn ring_follows_the_call(&self, ctx: &egui::Context, action: TileBarAction) {
        if !matches!(action, TileBarAction::Mic | TileBarAction::Hangup) {
            return;
        }
        if !self.bar_has_focus(ctx) {
            return;
        }
        let turned_into = if self.talking { TileBarAction::Hangup } else { TileBarAction::Mic };
        let index =
            self.tile_bar_entries().0.iter().position(|(_, entry)| *entry == turned_into);
        if let Some(index) = index {
            ctx.memory_mut(|memory| memory.request_focus(tilebar::control(index)));
        }
    }

    /// Keeps the ring on the strip across the switch between the picture and its
    /// recording, and back.
    ///
    /// The two rows have nothing in common: a live picture carries the way into
    /// the camera's recordings and none of the recording's controls, and a
    /// recording carries the way back and none of the live ones. Upright the
    /// strip is pinned - it outlives the row - so a ring left on a place in the
    /// old row names a control that is no longer drawn, egui drops it with the
    /// widget, and the keyboard falls back to the picture: the arrows change
    /// channel where the viewer was walking the controls, which is one press
    /// away from a channel they did not ask for. The ring goes to the control
    /// that undoes the switch instead - the way into the recording after
    /// leaving it, the way back to the picture after opening it - because that
    /// is the control a viewer who changed their mind would press. It is the
    /// same answer the microphone and the handset give.
    ///
    /// Only a ring that was on the strip is moved. A viewer watching the picture
    /// has not asked for a cursor, and one made with a finger never had one: the
    /// press itself is the answer, and the ring stays where the pointer rules
    /// left it.
    ///
    /// Opening a recording is answered by the box, a request later, so the press
    /// only records what it was for; the ring is put down when the row it
    /// belongs to is drawn. Leaving one is answered at once.
    ///
    /// The row is the same row wherever the strip is: upright and not turned it
    /// is pinned under the picture, landscape and a turned picture float it over
    /// one, and the controls are laid out - and lost - by the same rules either
    /// way. So the ring is kept either way. What the floating strip adds is a
    /// clock: it can be put away while the box answers, and a ring on a strip
    /// that is no longer drawn is only the seat it falls back to.
    pub(super) fn ring_follows_the_switch(&mut self, ctx: &egui::Context, action: TileBarAction) {
        if !self.tile_bar.is_open() || !self.bar_has_focus(ctx) {
            return;
        }
        // The anchor is the control that undoes the switch, and the channel it
        // belongs to: a picture the viewer has since stepped away from is not
        // the one the ring is waiting for.
        let Some(channel) = self.scheduler.zoom() else {
            return;
        };
        match action {
            TileBarAction::Playback => {
                self.ring_anchor = Some(RingAnchor {
                    action: TileBarAction::Live,
                    channel,
                    since: Instant::now(),
                });
            }
            TileBarAction::Live => {
                self.ring_anchor = Some(RingAnchor {
                    action: TileBarAction::Playback,
                    channel,
                    since: Instant::now(),
                });
            }
            _ => {
                // Any other control is the viewer's own choice, and it is the
                // end of the anchor: the ring is where the viewer put it, and
                // an anchor that pulls it back to a control they have not asked
                // for would take the cursor away from the button they just
                // pressed.
                self.ring_anchor = None;
            }
        }
    }

    /// Puts the ring on the control its anchor names, if the row carries it.
    ///
    /// Asked of the row as it is drawn now, every frame: the anchor is a
    /// control - "the way back to the picture" - and a row is a list of
    /// controls that is put together again for every picture and every frame.
    /// Its length is not fixed - a row that carries a sound switch is one
    /// longer than a row that does not, and the switch itself comes and goes
    /// with the picture - so the place of the anchor moves, and a ring that was
    /// put there once is a ring on a control that has been drawn over by the
    /// next row. egui drops such a focus with the widget it named, and the
    /// keyboard falls back to the picture: the arrows change channel where the
    /// viewer was walking the controls.
    ///
    /// So the ring is put on the anchor again as long as the anchor holds, and
    /// it cannot be lost by a row that changed under it. The anchor is let go
    /// by the remote walking the strip itself - that is the viewer taking the
    /// ring back - and by the strip going down.
    ///
    /// And by the ring being somewhere else entirely: the timeline under the
    /// strip, a panel, the picture. The anchor is the strip's business, and a
    /// ring that has left the strip is not the ring the anchor was holding.
    pub(super) fn place_the_anchored_ring(&mut self, ctx: &egui::Context) {
        let Some(anchor) = self.ring_anchor else {
            return;
        };
        let (action, channel, since) = (anchor.action, anchor.channel, anchor.since);
        // A picture the viewer has stepped away from, or a strip that is down,
        // is not a ring to keep: the first has another row under it and the
        // second has no row at all.
        if self.scheduler.zoom() != Some(channel) || !self.tile_bar.is_open() {
            self.ring_anchor = None;
            return;
        }
        // An anchor with no question behind it is not one to keep either: the
        // press that asked for a recording the box never answered leaves a row
        // that will never carry the control the anchor names, and an anchor
        // that outlived its question would put the ring on a control the
        // viewer was never shown.
        if since.elapsed() > ANCHOR_WAIT {
            self.ring_anchor = None;
            return;
        }
        // A ring that is on something else is the viewer's own: the timeline
        // under the strip, a panel, the picture. The anchor is the strip's
        // business, and a ring that has left the strip is not the one it was
        // holding. A ring that is on nothing at all is a ring egui dropped with
        // the widget it named, and that one is put back.
        let focused = ctx.memory(|memory| memory.focused());
        if focused.is_some_and(|id| !tilebar::owns(id)) {
            self.ring_anchor = None;
            return;
        }
        // The row that has not been drawn yet does not carry the anchor: the
        // recording's row only appears when the box answers, and until then
        // the live row has no way back in it.
        let Some(id) = self.the_control_for(action) else {
            return;
        };
        if ctx.memory(|memory| memory.focused()) != Some(id) {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    /// Where a control that answers `action` stands in the row as it is drawn
    /// now.
    ///
    /// Asked by what the control does rather than by its place in the old row:
    /// the rows are not the same length either, and a control that arrives or
    /// leaves - a sound switch, a microphone - moves every place after it.
    pub(super) fn the_control_for(&self, action: TileBarAction) -> Option<egui::Id> {
        self.tile_bar_entries()
            .0
            .iter()
            .position(|(_, entry)| *entry == action)
            .map(tilebar::control)
    }
}

/// Where the picture and the strip go when the strip is given a band of its own.
///
/// Upright and not turned, the magnified viewport is two regions rather than one
/// picture with a strip floating over it: the picture above, the bar in a band
/// along the bottom. This is that division, and the one place it is worked out -
/// the wall's own copy of the picture, the copy drawn over a picture the surface
/// is showing, and every gesture that belongs to the picture all read it from
/// here, so a press and the thing it presses cannot land in different places.
#[derive(Debug, Clone, Copy)]
pub(super) struct PanelSplit {
    /// The picture's region: everything above the band.
    pub video: Rect,
    /// The strip's band, measured up from the bottom of the area.
    pub bar: Rect,
    /// Whether the band carries a timeline under the controls.
    pub timeline: bool,
}

/// How deep the band is, and therefore where the picture ends.
///
/// As deep as the panel asks - the controls, the timeline when one is coming,
/// and `margin` above and below them both - and never more than half the area: a
/// viewer who magnified a picture asked for the picture, and half the screen is
/// the most the controls may take of it. A band the half clips gives up the
/// timeline rather than crushing both rows into what is left; the controls alone
/// are still the bar, and a timeline that cannot be read is not worth the
/// picture it costs.
///
/// Nothing here asks the band how deep it is before measuring it. The panel's
/// depth is a question for the width alone - see [`tilebar::panel_depth`] - so
/// the picture and the band are placed in one pass, in the frame they are drawn.
pub(super) fn split_panel(
    area: Rect,
    widths: &[f32],
    breaks: &[usize],
    playing: bool,
    margin: f32,
) -> PanelSplit {
    let half = area.height() * 0.5;
    let across = area.width();
    let plain = tilebar::panel_depth(across, widths, breaks, false) + 2.0 * margin;
    let with_row = tilebar::panel_depth(across, widths, breaks, true) + 2.0 * margin;
    let (want, timeline) = match (playing, with_row <= half) {
        (false, _) => (plain, false),
        (true, true) => (with_row, true),
        (true, false) => (plain, false),
    };
    let bar_h = want.min(half);
    let edge = area.bottom() - bar_h;
    PanelSplit {
        video: Rect::from_min_max(area.min, egui::pos2(area.right(), edge)),
        bar: Rect::from_min_max(egui::pos2(area.left(), edge), area.max),
        timeline,
    }
}

/// Paints the ground the band stands on, and the seam above it.
///
/// The band is part of the wall's frame: its floor is [`theme::BACKGROUND`],
/// darker than the picture's own canvas, and the seam between the two is one
/// point of the wall's line at three tenths. The ring around the whole of the
/// area is the strong line; a seam as strong would read as a second frame
/// inside it. Both copies - the wall's, and the one drawn over a picture the
/// surface is showing - paint this, and this is why it is one function: two
/// shades a hand-written pair apart would be a seam that moves when the picture
/// goes onto the surface.
fn paint_band(painter: &egui::Painter, bar: Rect) {
    painter.rect_filled(bar, egui::CornerRadius::ZERO, theme::BACKGROUND);
    let seam = Rect::from_min_max(
        bar.left_top(),
        egui::pos2(bar.right(), bar.top() + grid::LINE),
    );
    painter.rect_filled(seam, egui::CornerRadius::ZERO, theme::GRID_LINE.gamma_multiply(0.3));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band the strip stands in is as deep as the panel asks, never more
    /// than half the area, and the picture has everything above it: the whole
    /// width, the foot of the screen for the band, and no point shared between
    /// the two - a picture and a band that overlapped would leave the seam
    /// whichever one was drawn second.
    #[test]
    fn the_band_is_the_panels_depth_at_the_foot_of_the_area() {
        let phone = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(411.0, 800.0));
        let margin = tilebar::EDGE;
        let three = vec![tilebar::icon_width(); 3];

        // Not a recording: one line of controls, and the band is its depth with
        // the margin above and below it.
        let plain = split_panel(phone, &three, &[], false, margin);
        assert!(!plain.timeline);
        assert_eq!(plain.bar.height(), 52.0 + 2.0 * margin);
        assert_eq!(plain.bar.width(), phone.width(), "the whole width");
        assert_eq!(plain.bar.bottom(), phone.bottom(), "at the foot of the area");
        assert_eq!(plain.video.bottom(), plain.bar.top(), "and the picture ends where it starts");
        assert_eq!(plain.video.top(), phone.top());
        assert_eq!(plain.video.height(), 800.0 - plain.bar.height());

        // A recording is a row deeper, and that row still comes out of the half.
        let playing = split_panel(phone, &three, &[], true, margin);
        assert!(playing.timeline);
        assert_eq!(playing.bar.height(), 109.0 + 2.0 * margin);
        assert!(playing.video.height() < plain.video.height(), "the picture gives up the row");

        // Three lines with the row under them: the deepest the panel goes.
        let recording = vec![tilebar::icon_width(); 13];
        let three_lines = split_panel(phone, &recording, &[1, 7], true, margin);
        assert!(three_lines.timeline);
        assert_eq!(three_lines.bar.height(), 203.0 + 2.0 * margin);
        assert!(three_lines.bar.height() < phone.height() * 0.5, "still inside the half");

        // A screen too shallow for that gives up the row rather than the
        // picture's half - and then the half itself is what clips the band.
        let stub = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(411.0, 300.0));
        let clipped = split_panel(stub, &recording, &[1, 7], true, margin);
        assert!(!clipped.timeline, "no room for both rows");
        assert_eq!(clipped.bar.height(), stub.height() * 0.5, "clipped at the half");
        assert_eq!(clipped.video.height(), stub.height() * 0.5);
        let alone = split_panel(stub, &recording, &[1, 7], false, margin);
        assert!(!alone.timeline);
        assert_eq!(alone.bar.height(), stub.height() * 0.5, "even the controls alone");
    }

    /// One method's source, from its signature to the next method's.
    ///
    /// The guards below are about the order of two calls and about which
    /// rectangle a call is made with, and both are things the type system cannot
    /// hold: a guard reads the source, and reading it to the end of the method
    /// keeps it from seeing the *other* copy's answer.
    fn method<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source.find(signature).expect("the method is there");
        let rest = &source[start..];
        let end = rest
            .match_indices('\n')
            .map(|(at, _)| at)
            .find(|at| {
                let next = &rest[at + 1..];
                next.starts_with("    fn ")
                    || next.starts_with("    pub fn ")
                    || next.starts_with("    pub(super) fn ")
            })
            .unwrap_or(rest.len());
        &rest[..end]
    }

    /// The band is the wall's frame and the pictures are the wall's pictures: a
    /// page on its way past goes behind the band, not over it. Painted before
    /// them - which it was - a page leaving the viewport stood in the strip's
    /// place until it had gone, and the strip came back as it left.
    #[test]
    fn the_band_is_painted_after_the_pages() {
        let grid = method(include_str!("wall.rs"), "fn draw_grid");
        let pages = grid.find("self.paint_view(ui, slide_area(").expect("the pages still slide");
        let band = grid.find("paint_band(ui.painter(), panel.bar)").expect("the band is painted");
        assert!(pages < band, "a page on its way past goes behind the band");
    }

    /// The band stands where the wall put it. Both copies of the strip - the
    /// wall's, and the one drawn over a picture the surface is showing - lay the
    /// bar out in it, and neither may measure it on the tile a slide has carried:
    /// a band measured there went sideways with the finger, leaving the picture's
    /// own canvas standing where the band had been. The band is the wall's frame;
    /// the picture is the thing that travels.
    #[test]
    fn the_band_stands_where_the_wall_put_it() {
        let source = include_str!("wall.rs");
        assert!(
            method(source, "fn tile_bar_overlay").contains("self.panel_split(wall)"),
            "the wall's copy divides the wall, not the tile the slide carried"
        );
        assert!(
            method(source, "fn draw_overlay_only").contains(".strip_band()"),
            "the overlay paints the band the wall recorded"
        );
    }

    /// The order a remote walks, with and without a sound switch, on a wide
    /// picture and on a phone held upright.
    fn controls(sound: bool, three_lines: bool) -> (Vec<TileBarAction>, Vec<usize>) {
        let (entries, breaks) = XgViewApp::recording_entries(
            Icon::Pause,
            tilebar::Control::Label("1x"),
            sound.then_some(Icon::Sound),
            tilebar::Control::Icon(Icon::Download),
            three_lines,
        );
        (entries.into_iter().map(|(_, action)| action).collect(), breaks)
    }

    #[test]
    fn a_wide_or_turned_recording_puts_the_save_after_the_date_picker() {
        let (actions, breaks) = controls(true, false);
        assert!(breaks.is_empty(), "one line on a wide or turned picture");
        assert_eq!(actions.len(), 13);
        let date = actions.iter().position(|a| *a == TileBarAction::Date).expect("a day picker");
        assert_eq!(actions[date + 1], TileBarAction::Download, "the save follows the day picker");
        assert_eq!(actions[date + 2], TileBarAction::Fit);
        assert_eq!(actions.last(), Some(&TileBarAction::Live));
        // A camera without audio keeps the place: the row does not shift, so
        // the control after it stays where it was.
        let (without, breaks) = controls(false, false);
        assert_eq!(without.len(), 13, "the row keeps its length");
        assert!(breaks.is_empty(), "one line on a wide or turned picture");
        assert!(
            without.contains(&TileBarAction::NoSound),
            "the sound's place is kept, and it says there is no sound"
        );
        let date = without.iter().position(|a| *a == TileBarAction::Date).expect("a day picker");
        assert_eq!(without[date + 1], TileBarAction::Download);
        assert_eq!(without.last(), Some(&TileBarAction::Live));
    }

    /// The ring goes back to the button that opened the save panel, and it is
    /// found by its place in the row rather than by a remembered id: the set of
    /// controls is not the same on a phone held upright, and a remembered id
    /// would point at whatever took the save's place there.
    #[test]
    fn the_save_button_is_where_the_focus_goes_back_to() {
        for (sound, three_lines) in [(true, false), (false, false), (true, true), (false, true)] {
            let (entries, _) = XgViewApp::recording_entries(
                Icon::Pause,
                tilebar::Control::Label("1x"),
                sound.then_some(Icon::Sound),
                tilebar::Control::Icon(Icon::Download),
                three_lines,
            );
            let place = entries
                .iter()
                .position(|(_, action)| *action == TileBarAction::Download)
                .expect("a recording always has a save button");
            assert_eq!(
                XgViewApp::download_button_in(&entries),
                Some(crate::tilebar::control(place)),
                "sound={sound} three_lines={three_lines}"
            );
        }
        // A picture that is not a recording has no save button, and the answer
        // is no focus at all rather than the wrong one.
        let live = vec![
            (tilebar::Control::Icon(Icon::AspectRatio), TileBarAction::Fit),
            (tilebar::Control::Icon(Icon::History), TileBarAction::Playback),
        ];
        assert_eq!(XgViewApp::download_button_in(&live), None);
    }

    #[test]
    fn a_portrait_recording_starts_with_live_and_ends_with_the_timeline() {
        let (actions, breaks) = controls(true, true);
        assert_eq!(breaks, vec![1, 7]);
        assert_eq!(actions[0], TileBarAction::Live);
        assert_eq!(actions[6], TileBarAction::Download, "the save took live's old place");
        assert_eq!(
            actions[7..].to_vec(),
            vec![
                TileBarAction::PrevEvent,
                TileBarAction::NextEvent,
                TileBarAction::StepBack,
                TileBarAction::StepForward,
                TileBarAction::ZoomIn,
                TileBarAction::ZoomOut,
            ]
        );
        // The break before the timeline keys is where it is with sound as well: the
        // sound's place is kept, so the row does not shift.
        let (without, breaks) = controls(false, true);
        assert_eq!(breaks, vec![1, 7]);
        assert_eq!(without[6], TileBarAction::Download);
        assert!(without.contains(&TileBarAction::NoSound), "the sound's place is kept");
    }

    /// The sound switch keeps its place, and the place says what is missing.
    ///
    /// The switch is the one control that comes and goes with the picture: a
    /// stream that has not said whether it carries an audio track yet, and one
    /// that has said it does not, are both rows without it. A control that
    /// comes and goes takes every control after it with it, and the row is
    /// drawn again for every picture - so the place is kept, drawn as a grey
    /// speaker rather than left empty, and the row does not shift under the
    /// remote's ring.
    #[test]
    fn the_sound_switch_keeps_its_place() {
        let wall = include_str!("wall.rs");
        // Both rows keep the place: the live one and the recording's.
        assert!(
            wall.matches("(tilebar::Control::Silent, TileBarAction::NoSound)").count() >= 3,
            "the live row and both of the recording's rows keep the place"
        );
        // The place answers nothing and takes no press: it is a place kept,
        // not a control, and the ring may not land on it.
        let at = wall.find("fn tile_bar_available(").unwrap();
        let body = &wall[at..(at + 700).min(wall.len())];
        assert!(
            body.contains("TileBarAction::NoSound | TileBarAction::Waiting"),
            "a grey speaker and a waiting mark are not controls"
        );
        // It is drawn rather than left as a hole, but the drawing is inert:
        // a dim muted speaker, with no press and no stop on the walk. The
        // greyness is the place's own, not a `muted` flag a caller could get
        // wrong - the branch passes `true` to `button_in` and skips `nav`.
        let tilebar = include_str!("../tilebar.rs");
        let at = tilebar.find("Control::Silent =>").unwrap();
        let body = &tilebar[at..(at + 700).min(tilebar.len())];
        assert!(body.contains("icons::button_in("), "a button is drawn");
        assert!(body.contains("Icon::Mute"), "the speaker is the muted one");
        assert!(body.contains("true,"), "and it is drawn grey");
        assert!(body.contains("continue;"), "and it takes no press or focus");
        // A viewer who hovers it is told why it cannot be pressed.
        assert!(
            wall.contains("TileBarAction::NoSound => tr(\"tilebar-no-sound-tip\")"),
            "the grey speaker says why it is grey"
        );
    }

    /// The ring moves on from the control that went grey, in the row's order,
    /// skipping a run of grey and wrapping round the end.
    #[test]
    fn a_greyed_control_hands_the_ring_to_the_next_live_one() {
        let live = [true, true, false, false, true];
        assert_eq!(
            XgViewApp::grey_handoff(Some(tilebar::control(2)), None, &live),
            Some(tilebar::control(4)),
            "a focus that survived the draw goes to the next live control"
        );
        assert_eq!(
            XgViewApp::grey_handoff(None, Some(tilebar::control(2)), &live),
            Some(tilebar::control(4)),
            "and so does one egui has already given up"
        );
        assert_eq!(
            XgViewApp::grey_handoff(None, Some(tilebar::control(2)), &[true, false, false]),
            Some(tilebar::control(0)),
            "the last grey control wraps round the end"
        );
    }

    /// Nothing is handed on when the ring was on a live control, when it moved
    /// somewhere that is not the bar, or when it simply was not on the bar.
    #[test]
    fn the_handoff_leaves_every_other_focus_alone() {
        assert_eq!(
            XgViewApp::grey_handoff(None, Some(tilebar::control(1)), &[true, true, false]),
            None,
            "a live control keeps its ring"
        );
        assert_eq!(
            XgViewApp::grey_handoff(
                Some(Id::new("a-window's-field")),
                Some(tilebar::control(1)),
                &[false, false]
            ),
            None,
            "a focus that moved on purpose is not overridden"
        );
        assert_eq!(
            XgViewApp::grey_handoff(None, None, &[false, false]),
            None,
            "no ring at the start of the frame means nothing to hand on"
        );
        assert_eq!(
            XgViewApp::grey_handoff(None, Some(tilebar::control(9)), &[false, false]),
            None,
            "a place the row no longer has is not a control"
        );
        assert_eq!(
            XgViewApp::grey_handoff(None, Some(tilebar::control(1)), &[false, false]),
            Some(tilebar::seat()),
            "a row with nothing live in it goes to the seat"
        );
    }
}
