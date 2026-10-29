//! Which page, which layout, which channel is magnified, and the window the
//! wall is drawn in.

use super::*;

impl XgViewApp {
    pub(super) fn set_layout(&mut self, layout: GridLayout) {
        if self.scheduler.layout() == layout && !self.scheduler.is_zoomed() {
            return;
        }
        self.tile_bar.hide();
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
        self.scheduler.zoom_out();
        self.scheduler.set_layout(layout);
        self.slide = Slide::default();
        // The settings panel and the layout picker are built for landscape.
        self.show_settings = false;
        self.layout_picker.open = false;
        self.portrait_aspect_picker.open = false;
        self.combined_info_picker.open = false;
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
        if self.scheduler.zoom_out() {
            self.needs_sync = true;
            self.mark_dirty();
            self.apply_audio();
        }
        self.settle_tile_bar();
    }

    pub(super) fn zoom_to(&mut self, index: usize) {
        // A double press toggles, the way Enter does: the magnified viewport
        // goes back to the grid, and any other one is magnified in its place.
        if self.scheduler.zoom() == Some(index) {
            self.zoom_out();
        } else {
            self.scheduler.set_focus(Some(index));
            self.zoom_in();
        }
    }

    pub(super) fn set_fullscreen(&mut self, ctx: &egui::Context, enabled: bool) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(enabled));
        self.fullscreen = enabled;
        // A window created full screen was never given a windowed size - asking
        // for one at creation is what drops it back out of full screen, see
        // `native_options` - so the first time full screen is left it is handed
        // one rather than coming back at whatever size it happens to be.
        if !enabled && std::mem::take(&mut self.windowed_size_pending) {
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
