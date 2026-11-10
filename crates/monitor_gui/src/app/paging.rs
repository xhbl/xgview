//! Which page, which layout, which channel is magnified, and the window the
//! wall is drawn in.

use super::*;

/// How long a magnification keeps the rest of the page live before it lets it
/// go.
///
/// A magnification parks the other channels: one picture, and the bandwidth and
/// the decoders of the rest given up for as long as it is up. That is right for
/// watching and wrong for looking - the sessions of a whole page are torn down
/// on the way in and opened again on the way out, which on a box that cannot
/// afford nine of them at once is seconds of blank tiles, and starves decoders
/// past the deadline that ends a session and waits out a backoff. Holding the
/// page for this long makes a look and a step back cost one stream switch;
/// whoever stays pays the bandwidth of the rest for these seconds and no
/// longer.
const ZOOM_PARK_GRACE: Duration = Duration::from_secs(10);

impl XgViewApp {
    pub(super) fn set_layout(&mut self, layout: GridLayout) {
        if self.scheduler.layout() == layout && !self.scheduler.is_zoomed() {
            return;
        }
        self.tile_bar.hide();
        // The viewport is closing, and the recording it was showing goes with
        // it: there is nowhere else for it to be drawn.
        self.leave_playback();
        self.scheduler.zoom_out();
        self.scheduler.set_layout(layout);
        self.slide = Slide::default();
        self.needs_sync = true;
        self.mark_dirty();
        self.apply_audio();
    }

    /// Whether the wall stands upright. False whenever the device cannot.
    pub(super) fn is_portrait(&self) -> bool {
        self.orientation.is_portrait()
    }

    /// Scales a bar's text and spacing down on a screen narrower than the
    /// reference width. The larger screen dimension is used so portrait and
    /// landscape on the same device share the same scale.
    pub(super) fn scale_bar(ui: &mut egui::Ui) {
        let size = ui.ctx().screen_rect().size();
        let width = size.x.max(size.y);
        let scale = (width / TOOLBAR_REFERENCE_WIDTH).clamp(TOOLBAR_MIN_SCALE, 1.0);
        if scale < 1.0 {
            let style = ui.style_mut();
            for font in style.text_styles.values_mut() {
                font.size *= scale;
            }
            style.spacing.button_padding.x *= scale;
            style.spacing.interact_size.x *= scale;
            style.spacing.item_spacing.x = (style.spacing.item_spacing.x * scale).max(2.0);
        }
    }

    /// Switches the wall between landscape and portrait.
    ///
    /// The grid of the orientation being left is parked first - through
    /// [`Self::sync`], which writes the scheduler's layout back to the field of
    /// the current orientation - so the other side's grid is not overwritten.
    /// The grid of the orientation being entered is loaded next. Page and focus
    /// are shared and are clamped to the new grid.
    pub(super) fn set_orientation(&mut self, portrait: bool) {
        let wanted = if portrait && self.supports_portrait {
            Orientation::Portrait
        } else {
            Orientation::Landscape
        };
        if self.orientation == wanted {
            return;
        }
        self.needs_sync = true;
        self.sync();
        self.orientation = wanted;
        self.tile_bar.hide();
        let view = self.config.group().clone();
        let layout = if wanted.is_portrait() { view.portrait_layout } else { view.layout };
        self.leave_playback();
        self.scheduler.zoom_out();
        self.scheduler.set_layout(layout);
        self.slide = Slide::default();
        // The settings panel and the layout picker are built for landscape.
        self.show_settings = false;
        self.layout_picker.close();
        self.portrait_aspect_picker.close();
        self.combined_info_picker.close();
        self.config.android_orientation = wanted;
        self.needs_sync = true;
        self.mark_dirty();
        self.apply_audio();
    }

    /// The orientation button, shared by both bars: the shape is the mode it
    /// switches to. Drawn only where the device can stand upright.
    pub(super) fn orientation_button(&mut self, ui: &mut egui::Ui, items: &mut Vec<(Id, Rect)>) {
        if !self.supports_portrait {
            return;
        }
        let (icon, key, tip) = if self.is_portrait() {
            (Icon::Landscape, "toolbar-orientation-landscape", "toolbar-orientation-landscape-tip")
        } else {
            (Icon::Portrait, "toolbar-orientation-portrait", "toolbar-orientation-portrait-tip")
        };
        let response = icons::button(ui, icon, false, &monitor_i18n::tr(tip));
        items.push((response.id, response.rect));
        self.name(&response, key);
        if response.clicked() {
            self.set_orientation(!self.is_portrait());
        }
    }

    pub(super) fn turn_page(&mut self, forward: bool, keyboard: bool) -> bool {
        let previous = self.scheduler.page();
        let changed = if forward { self.scheduler.next_page() } else { self.scheduler.prev_page() };
        if changed {
            let layout = self.scheduler.layout();
            let total = self.config.enabled_count();
            let outgoing = PageView { layout, cells: grid::page_cells(layout, previous, total) };
            self.slide.start(if forward { 1.0 } else { -1.0 }, Some(outgoing));
            self.needs_sync = true;
            self.mark_dirty();
            if keyboard {
                self.last_turn = Some(forward);
            }
        }
        self.slide.drag = 0.0;
        changed
    }

    /// Walks the grid one cell, with the slide a page turn uses.
    ///
    /// Only the grid: a magnified viewport is not a grid and steps along the
    /// channel list instead. See [`Self::step_zoom`].
    pub(super) fn navigate(&mut self, dir: Direction) -> NavigateOutcome {
        let previous_page = self.scheduler.page();
        let outcome = self.scheduler.navigate(dir);
        if matches!(outcome, NavigateOutcome::Blocked) {
            return outcome;
        }

        let sign = if dir == Direction::Left { -1.0 } else { 1.0 };
        let outgoing = if matches!(outcome, NavigateOutcome::PageTurned { .. }) {
            let layout = self.scheduler.layout();
            let total = self.config.enabled_count();
            Some(PageView { layout, cells: grid::page_cells(layout, previous_page, total) })
        } else {
            None
        };
        if let Some(outgoing) = outgoing {
            self.slide.start(sign, Some(outgoing));
        }
        self.needs_sync = true;
        self.mark_dirty();
        outcome
    }

    /// Moves the wall one step the way a swipe or a page key asked for: the
    /// page turn on the grid, the channel step while magnified.
    pub(super) fn advance(&mut self, forward: bool, keyboard: bool) -> bool {
        if self.scheduler.is_zoomed() {
            self.step_zoom(forward)
        } else {
            self.turn_page(forward, keyboard)
        }
    }

    /// Whether the page-turn buttons have a step to take.  Magnified, the
    /// boundary is the channel list; on the grid it is the page count.
    pub(super) fn can_advance(&self, forward: bool) -> bool {
        if let Some(zoom) = self.scheduler.zoom() {
            if forward {
                zoom + 1 < self.scheduler.channel_count()
            } else {
                zoom > 0
            }
        } else {
            let info = self.scheduler.page_info();
            if forward { info.has_next() } else { info.has_previous() }
        }
    }

    /// Steps the magnified viewport to the previous or next channel, with the
    /// slide a page turn uses.
    ///
    /// The magnified view is one channel, not a grid, so its step is the
    /// channel order the swipe preview already showed - see
    /// [`Scheduler::zoom_step`] - and the ends are a stop. Without this a swipe
    /// turned the grid page behind the magnification: the picture did not
    /// change, and leaving the magnification landed on a different camera.
    pub(super) fn step_zoom(&mut self, forward: bool) -> bool {
        let Some(current) = self.scheduler.zoom() else {
            return false;
        };
        // The bar belongs to one picture: stepping to the next one leaves it
        // behind, the way the window a pinch left behind is.
        self.tile_bar.hide();
        if !self.scheduler.zoom_step(forward) {
            self.slide.drag = 0.0;
            return false;
        }
        // The recording belonged to the camera being left, and it is shown
        // nowhere else: it goes with the picture, and its channel stops. Left
        // behind it would play on unseen - a request to the server and a
        // decoder held for a camera nobody is looking at.
        self.leave_playback();
        let outgoing = PageView { layout: GridLayout::G1x1, cells: vec![Some(current)] };
        self.slide.start(if forward { 1.0 } else { -1.0 }, Some(outgoing));
        // The channel that slides in comes up whole, the way a magnify does: the
        // window was the last camera's, and a part of it is not a part of this
        // one.
        self.picture_zoom = PictureZoom::FULL;
        self.needs_sync = true;
        self.mark_dirty();
        // The viewport is a different camera now, and the sound and the back
        // channel follow the viewport: without this the new camera never asked
        // its session for a back channel, so its bar carried no microphone, and
        // the sound stayed on the camera that was left.
        self.apply_audio();
        self.settle_tile_bar();
        true
    }

    pub(super) fn zoom_in(&mut self) {
        if self.scheduler.zoom_in() {
            // The page is held for a while yet: see [`ZOOM_PARK_GRACE`].
            self.zoom_park_at = Some(Instant::now() + ZOOM_PARK_GRACE);
            // The viewport comes up whole: the window the last one was left at is
            // a part of another picture, and there is none of this one to keep.
            self.picture_zoom = PictureZoom::FULL;
            self.needs_sync = true;
            self.mark_dirty();
            self.apply_audio();
        }
        self.settle_tile_bar();
    }

    pub(super) fn zoom_out(&mut self) {
        // Leaving the viewport is leaving the recording: playback has no
        // picture of its own anywhere else, and a channel left running unseen
        // is a request to the server for nothing.
        self.leave_playback();
        // Whatever the next magnification does, it starts its own wait. The
        // scheduler puts its parking back itself, so a magnification left by a
        // group switch or a page turn is covered too.
        self.zoom_park_at = None;
        if self.scheduler.zoom_out() {
            self.needs_sync = true;
            self.mark_dirty();
            self.apply_audio();
        }
        self.settle_tile_bar();
    }

    /// Lets the rest of the page go once the magnification has lasted long
    /// enough.
    ///
    /// Called once a frame, before the schedule is applied. The wall repaints
    /// while it has pictures to draw, so the wait needs no clock of its own: a
    /// magnification that is up is a wall that is being drawn.
    pub(super) fn settle_zoom_park(&mut self) {
        if !self.scheduler.is_zoomed() {
            self.zoom_park_at = None;
            return;
        }
        let Some(at) = self.zoom_park_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.zoom_park_at = None;
        if self.scheduler.set_park_others(true) {
            self.needs_sync = true;
        }
    }

    pub(super) fn zoom_to(&mut self, index: usize) {
        // A double press toggles, the way Enter does: the magnified viewport
        // goes back to the grid, and any other one is magnified in its place.
        if self.scheduler.zoom() == Some(index) {
            self.zoom_out();
        } else {
            // Another camera's picture is a different camera's recordings.
            self.leave_playback();
            self.scheduler.set_focus(Some(index));
            self.zoom_in();
        }
    }

    pub(super) fn set_fullscreen(&mut self, ctx: &egui::Context, enabled: bool) {
        // On Android this is not about the window at all. The activity owns it
        // and keeps it full screen - the system's own bars are never shown
        // there - so leaving full screen asks only that the wall keep its two
        // bars up. The viewport commands below would reach no window manager on
        // the surfaces front end and, on the older one, would take the system
        // bars back out of hiding; skipped rather than sent and ignored.
        let window = !cfg!(target_os = "android");
        if window {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(enabled));
        }
        self.fullscreen = enabled;
        // A window created full screen was never given a windowed size - asking
        // for one at creation is what drops it back out of full screen, see
        // `native_options` - so the first time full screen is left it is handed
        // one rather than coming back at whatever size it happens to be.
        let resized = std::mem::take(&mut self.windowed_size_pending);
        if window && !enabled && resized {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                crate::WINDOWED_SIZE[0],
                crate::WINDOWED_SIZE[1],
            )));
        }
        // Leaving full screen brings the bars back on an explicit action, not
        // on a timer: the viewer is about to use them. Entering it is the
        // opposite request - the picture and nothing else - so they go at once
        // rather than after the idle delay. A configured idle of zero means the
        // bars never hide, so entering full screen leaves them up as well.
        let hide = enabled && self.config.chrome_idle_secs > 0.0;
        self.chrome.visible = !hide;
        self.chrome.hide_now = hide;
        self.chrome.last_input = Instant::now();
        // On Android the choice is about the wall's own bars rather than a
        // viewport's, and a wall that is not a kiosk should come back the way
        // the viewer left it - the launch after this one reads it. Written only
        // when it changes: the first frame calls this with the state the window
        // was created in, which is not a choice the viewer made.
        #[cfg(target_os = "android")]
        {
            let pinned = !enabled;
            if self.config.android_pin_bars != pinned {
                self.config.android_pin_bars = pinned;
                self.mark_dirty();
            }
        }
        // The option to stay on top is about the wall that has the screen to
        // itself, so entering and leaving full screen is when it moves.
        self.apply_window_level(ctx);
    }

    /// Asks the window manager for the window level the current state wants.
    ///
    /// Sent when something the answer is decided from moves - the option, full
    /// screen, a suspension - rather than every frame: it is a request to the
    /// window manager, and one per frame would be a repaint's worth of noise for
    /// an answer that has not changed.
    pub(super) fn apply_window_level(&mut self, ctx: &egui::Context) {
        let level = window::level(
            window::supported(),
            self.config.always_on_top_fullscreen,
            self.fullscreen,
            self.on_top,
            Instant::now(),
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    }

    /// Says what the window's place among the others is doing from now on.
    ///
    /// Used around anything the viewer opened that has to come up in front of
    /// the wall; [`crate::window`] describes the two shapes its end can take.
    /// Nothing is remembered where the platform cannot hold a window above the
    /// others, since nothing can be asked of it.
    pub(super) fn set_on_top(&mut self, ctx: &egui::Context, on_top: OnTop) {
        if !window::supported() {
            return;
        }
        self.on_top = on_top;
        if let OnTop::SuspendedUntil(until) = on_top {
            // Nothing else would wake the app to put the wall back when the
            // clock runs out.
            ctx.request_repaint_after(until.saturating_duration_since(Instant::now()));
        }
        self.apply_window_level(ctx);
    }

    /// Turns the magnified picture a quarter turn, or back.
    ///
    /// The choice is the configuration's own, one answer for every camera: the
    /// viewer who wants an upright screen filled wants it filled for whatever
    /// they step to next, and the letterbox is not a property of a camera. Only
    /// the magnified picture is turned: the wall's tiles are drawn upright
    /// whatever this says, and a landscape wall never turns one.
    pub(super) fn toggle_zoom_rotate(&mut self) {
        self.config.zoom_rotate = !self.config.zoom_rotate;
        self.mark_dirty();
        // The picture's shape is the bar's default too: turned, it goes; back
        // upright, it comes.
        self.settle_tile_bar();
    }
}
