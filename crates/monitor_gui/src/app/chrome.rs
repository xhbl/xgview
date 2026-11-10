//! When the bars are up, the focus ring's clock and the toasts: what the frame
//! puts around the wall, and when it takes it away.
//!
//! The bars' controls are in `bars`; this is their life - which one the remote is
//! walking, when they fade, and what replaces them for a moment.

use egui::epaint::TextShape;
use egui::Color32;

use super::*;

impl XgViewApp {
    pub(super) fn flash(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast { text: text.into(), kind, at: Instant::now(), center: false });
    }

    /// [`Self::flash`] in the middle of the screen, for news that belongs to the
    /// picture rather than to a channel - the magnified bar's fit switch.
    pub(super) fn flash_centered(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast { text: text.into(), kind, at: Instant::now(), center: true });
    }

    /// Hands the remote's focus from the wall to the top bar.
    ///
    /// The bar is where the settings panel and the device window are opened
    /// from, and a television remote has no F1: the arrow that walks off the top
    /// of the wall is the only way in. The focus lands above the tile the viewer
    /// was on rather than at one end of the row, because the row is long and the
    /// walk back to the other end is one press per control.
    pub(super) fn enter_toolbar(&mut self, ctx: &egui::Context) {
        let nearest = match self.grid_focus_x {
            Some(x) => self
                .toolbar_items
                .iter()
                .min_by(|(_, left), (_, right)| {
                    (left.center().x - x).abs().total_cmp(&(right.center().x - x).abs())
                })
                .map(|(id, _)| *id),
            None => self.toolbar_items.first().map(|(id, _)| *id),
        };
        if let Some(id) = nearest {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    /// Hands the remote's focus from the wall to the author in the status bar.
    ///
    /// The status row holds the one control below the wall, so the arrow that
    /// walks off the bottom of the wall is the way onto it - the mirror of the
    /// arrow that walks off the top into the bar.
    pub(super) fn enter_status(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.status_author {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    /// Hands the remote's focus back to the wall.
    pub(super) fn leave_controls(&mut self, ctx: &egui::Context) {
        if let Some(id) = ctx.memory(|memory| memory.focused()) {
            ctx.memory_mut(|memory| memory.surrender_focus(id));
        }
    }

    /// Whether the top bar is what currently has the remote's focus.
    pub(super) fn toolbar_has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.focused())
            .is_some_and(|focused| self.toolbar_items.iter().any(|(id, _)| *id == focused))
    }

    /// Whether the author in the status bar is what currently has the remote's
    /// focus.
    pub(super) fn status_has_focus(&self, ctx: &egui::Context) -> bool {
        self.status_author
            .is_some_and(|id| ctx.memory(|memory| memory.focused()) == Some(id))
    }

    /// Whether one of the two page-turn buttons is where the remote's focus is.
    ///
    /// The pair is the landscape toolbar's and the upright bar's floating arrows
    /// alike - whichever bar was up last set the two ids, so this answers for
    /// either. What it guards is the bounce a turn at the edge makes; see
    /// [`Self::turn_page`].
    pub(super) fn page_button_has_focus(&self, ctx: &egui::Context) -> bool {
        Self::focus_on_page_button(
            ctx.memory(|memory| memory.focused()),
            self.page_prev_id,
            self.page_next_id,
        )
    }

    /// Whether `focused` is one of the two page-turn buttons.
    ///
    /// A page key turns the page from wherever the focus is; only a focus that
    /// stood on a page button follows the turn past the edge into the other
    /// one, because the turn is what took the button it stood on away. A tile's
    /// focus, or a focus on anything else in the bars, turns the page and stays
    /// where it was put.
    pub(super) fn focus_on_page_button(focused: Option<Id>, prev: Option<Id>, next: Option<Id>) -> bool {
        focused.is_some_and(|focused| Some(focused) == prev || Some(focused) == next)
    }

    /// Hands the remote's focus from the wall to the bar an arrow walked off it
    /// for.
    ///
    /// Only into a bar that is on screen. With the bars hidden there is nothing
    /// on the far side of the wall to land on, and waking them for an arrow that
    /// walked off it is exactly the churn this is meant to stop: the bars come
    /// back for Back, for a function key, or for a press in their band, and the
    /// arrow finds them then. See [`Self::viewer_active`].
    pub(super) fn hand_off(&mut self, ctx: &egui::Context, to: Bar) {
        if !self.chrome.visible {
            return;
        }
        match to {
            Bar::Toolbar => self.enter_toolbar(ctx),
            Bar::Status => self.enter_status(ctx),
        }
    }

    /// Whether what the viewer did this frame is a request for the bars.
    ///
    /// Called before the keys are handed out: the application takes the ones it
    /// acts on out of the queue, so a key this has to answer on must be read
    /// here or not at all.
    ///
    /// There are exactly three ways to ask for the bars, and everything else is
    /// the wall:
    ///
    /// * a press in the band a bar occupies - the pointer's way in, and the only
    ///   one a finger has;
    /// * Back, which is the one key on a remote control that reads as "the
    ///   menus";
    /// * the function keys, which are what opens a panel in the first place.
    ///
    /// None of the rest of it - walking the channels, turning a page, magnifying
    /// one, picking a grid - is a request for the bars, and treating it as one is
    /// what made the picture resize under the viewer every time they touched it:
    /// the bars take their height from the wall rather than lying over it, so
    /// bringing them back for a page turn moves every tile, and then they slide
    /// away again 3.5 seconds later. See [`Self::wall_event`] for the whole of
    /// the decision, one event at a time.
    pub(super) fn viewer_active(&self, ctx: &egui::Context) -> bool {
        // Read from this frame's focus rather than from `owner`: Escape is taken
        // by egui as "drop the focus" before this runs, so an answer stored at
        // the end of the last frame still says "the viewer is in the menus" on
        // the very press that left them - which is how closing the viewport's bar
        // with Escape woke the bars behind it, while the remote's Back, which
        // egui leaves alone, did not.
        let focused = ctx.memory(|memory| memory.focused());
        // The magnified viewport's bar is the picture's own surface, not one of
        // the wall's bars: using it is the viewer at the picture. Waking the
        // bars for it would take the next press out of the strip's hands - and
        // move the wall under it - so none of it counts as asking for them. That
        // holds while the remote is *in* the strip: a strip that is merely up,
        // the upright default, leaves the remote on the wall, and an arrow off
        // it is still a press that asks for the bars.
        //
        // The strip itself is the same surface, and it is not one of the bar's
        // ids - without it here, Up out of the strip put the focus on a control
        // and this read the frame's key as "the viewer is in the menus", so the
        // bars came up over a full screen picture.
        if self.tile_bar.is_open()
            && (focused.is_some_and(tilebar::owns) || self.strip_has_focus(ctx))
        {
            return false;
        }
        // The remote's focus is in a bar, in the settings panel or in a window:
        // the viewer is in the menus, and everything they do belongs to them -
        // except a press on the wall, which takes the focus back and must not
        // move the wall under the click that did it. See [`Self::menus_input`].
        //
        // The bands are held to this frame's screen: a rectangle from another
        // window size is a band in the middle of this one, and a press there is
        // a press on a tile. See [`Self::bands`].
        let bands = Self::bands(self.chrome.bars, ctx.screen_rect());
        if focused.is_some() {
            return ctx.input(|input| Self::menus_input(input, bands));
        }
        ctx.input(|input| {
            // While the picture is magnified and the bars are away, the band
            // along its top and bottom edge is the viewport's own bar being
            // asked for, not the menus: the two are the same part of the screen,
            // and one press cannot be both. With the bars on screen their band is
            // their own as it always was.
            let band_is_a_door = !self.scheduler.is_zoomed() || self.chrome.visible;
            input.events.iter().any(|event| {
                Self::wall_event(bands, self.back_is_a_door(), band_is_a_door, event)
            })
        })
    }

    /// Whether the top and bottom bars are drawn this frame.
    ///
    /// Outside full screen they always are - the window has a frame around it
    /// anyway, and the bars are where the controls live. In full screen they
    /// fade once the wall has been left alone for the configured idle time,
    /// which is the difference between a monitor and a television. The answer is
    /// kept in [`Chrome::visible`], and what the focus ring makes of it is read
    /// where it changes: the bars arm the ring as they appear and take it with
    /// them as they go, and its own clock rules in between. See
    /// [`Self::ring_follows_bars`].
    pub(super) fn chrome_idle(&self) -> Duration {
        let secs = self.config.chrome_idle_secs;
        if secs <= 0.0 {
            Duration::MAX
        } else {
            Duration::from_secs_f32(secs)
        }
    }

    /// Whether the focus ring is drawn this frame.
    ///
    /// Its own clock, and only its own: armed by the keys that walk it - see
    /// [`Self::ring_keys`] - and by a click that lands on a tile, and off until
    /// the viewer makes one, which is what keeps a fresh run's wall free of a
    /// cursor. The bars have no say here; what they do to the ring they do at
    /// the moment they come and go. See [`Self::ring_follows_bars`].
    pub(super) fn ring_shown(&self) -> bool {
        Self::ring_standing(self.ring_seen, Instant::now())
    }

    /// [`Self::ring_shown`], with its clock given: the rule we can test.
    pub(super) fn ring_standing(armed: Option<Instant>, now: Instant) -> bool {
        armed.is_some_and(|at| now.saturating_duration_since(at) < RING_IDLE)
    }

    /// What the bars do to the ring as they come and go: they arm it as they
    /// appear, and take it away with them as they go.
    ///
    /// It is the bars' *ends* and not their state, and that is what makes the
    /// ring usable in full screen. The wall's own keys are deliberately not a
    /// request for the bars - see [`Self::viewer_active`], and the height the
    /// bars take from the wall - so a viewer driving the wall with the bars away
    /// has nothing that could bring them back, and a ring that asked whether the
    /// bars were up before it drew itself would never draw at all in the one
    /// case it exists for. Read as a gate, `chrome.visible` did exactly that.
    ///
    /// `was` is whether the bars were up as the frame *began*, and the other
    /// side of the comparison has to be the state the frame *ends* in: the bars
    /// are taken away by a key before anything is drawn (`handle_keys`) and by
    /// the toolbar's button or the Display tab's checkbox while the frame is
    /// being drawn, so a state read in the middle of the frame misses the
    /// second, and one read at the start of the next frame misses both.
    pub(super) fn ring_follows_bars(seen: Option<Instant>, bars: bool, was: bool, now: Instant) -> Option<Instant> {
        match (bars, was) {
            // The bars coming up: the ring comes back with them.
            (true, false) => Some(now),
            // The bars going: they take the ring with them.
            (false, true) => None,
            // Either of them on its own: the ring keeps its own clock.
            _ => seen,
        }
    }

    /// What a change of the wall's cursor does to the ring: it takes one that is
    /// showing down.
    ///
    /// The cursor is the page together with the focused channel, and a change of
    /// either without the viewer aiming at it leaves the ring standing on the
    /// wrong tile: a page turned this frame re-places the focus into the new page
    /// (`Scheduler::normalize` keeps it on the visible one), and a layout that
    /// shrinks under the focus does the same - a 4x4's ninth channel is the third
    /// one in a 2x2 - as does a camera switched off, which clamps it. A cursor
    /// pointing at the wrong place is worse than no cursor, and the next aim
    /// lights it where it now stands. A frame that **aimed** is the exception: an
    /// arrow walking off the edge of the wall turns the page itself, and that is
    /// the viewer walking the cursor, so the ring follows it. See
    /// [`Self::ring_keys`].
    pub(super) fn ring_follows_cursor(
        seen: Option<Instant>,
        cursor: (usize, Option<usize>),
        was: (usize, Option<usize>),
        aimed: bool,
    ) -> Option<Instant> {
        if cursor != was && !aimed {
            None
        } else {
            seen
        }
    }

    pub(super) fn chrome_visible(&mut self) -> bool {
        // An open panel owns the bars: taking away the bar of a panel the
        // viewer just opened would strand it. The layout picker is a modal of
        // its own and counts the same way - it is opened from the toolbar, and
        // a bar that faded under it would take its own trigger away.
        if self.show_settings || self.discovery.open || self.layout_picker.is_open() || self.portrait_aspect_picker.is_open() || self.combined_info_picker.is_open() {
            self.chrome.visible = true;
            self.chrome.hide_now = false;
            self.chrome.last_input = Instant::now();
            return true;
        }
        // Not full screen: the bars are the point of the mode. On Android that
        // is all the mode is - the window is full screen either way, and this
        // is the viewer asking for their own toolbar and status bar to stay
        // put; see `AppConfig::android_pin_bars`.
        if !self.fullscreen {
            self.chrome.visible = true;
            self.chrome.hide_now = false;
            return true;
        }
        // No live cameras: the wall is empty, and hiding the bars would leave
        // the viewer with no way to reach the settings or the device window.
        //
        // A recording is not an empty wall. Its channel is not a live one, so
        // this branch used to keep both bars up for as long as it played, and
        // they claim their strip as panels over the central one: a magnified
        // recording lost 56 points of height, and its 4:3 picture came up at
        // 1208x906 where the live picture it replaced had filled 1440x1080.
        // The controls a recording needs are the viewport's own bar, which
        // `end_frame` holds up for it and which claims no room.
        if self.playback.is_none()
            && self.scheduler.current().and_then(|s| s.live().next()).is_none()
        {
            self.chrome.visible = true;
            self.chrome.hide_now = false;
            return true;
        }
        // Entering full screen asks for the picture and nothing else, and the
        // request is answered at once - except on the frames before the bars have
        // ever been drawn in this run. What they cover is only known as they are
        // laid out, and the band they leave behind is where a pointer asks for
        // them back: starting in full screen asks for the picture straight away,
        // so they are drawn once - this frame - to be measured, and taken away on
        // the next one. Waiting for a frame that draws them instead would wait
        // for ever: this is the request that keeps them from being drawn.
        if self.chrome.hide_now {
            if self.chrome.bars.is_none() {
                self.chrome.visible = true;
                return true;
            }
            self.chrome.hide_now = false;
            self.chrome.visible = false;
            return false;
        }
        if self.chrome.input_seen {
            self.chrome.last_input = Instant::now();
            self.chrome.visible = true;
        } else if self.chrome.last_input.elapsed() >= self.chrome_idle() {
            self.chrome.visible = false;
        }
        self.chrome.visible
    }

    /// The toast, on the layer that is on top.
    ///
    /// Two layers carry frames once a magnified picture is scanned out of a
    /// surface of its own: the wall's, and the overlay over the picture. Only
    /// the upper one is seen - the picture's surface is above the wall - so a
    /// toast the wall paints while that picture is up is a toast nobody sees,
    /// which is what the fit switch's own name was: drawn every frame, invisible
    /// every frame. The wall therefore paints it only while nothing is over it,
    /// and the overlay (`draw_overlay_only`) paints it otherwise. One copy, on
    /// the layer that is on top.
    pub(super) fn draw_toast(&self, ctx: &egui::Context) {
        if self.picture_on_layer() {
            return;
        }
        self.paint_toast(ctx);
    }

    /// The toast itself, wherever it is being drawn.
    ///
    /// The place it takes is the whole screen's, in either context: the
    /// overlay's frame is the screen too, see `android_surface`.
    pub(super) fn paint_toast(&self, ctx: &egui::Context) {
        let Some(toast) = &self.toast else {
            return;
        };
        if toast.at.elapsed() > Duration::from_secs(6) {
            return;
        }
        let color = if toast.kind == ToastKind::Error { theme::ERROR } else { theme::LIVE };
        // A picture drawn a quarter turn takes the screen with it: the corners
        // and the strip are read in the picture's frame, and news about that
        // picture has to be too. Level text over a sideways picture is text the
        // viewer has to turn their head for.
        if let (true, Some(tile)) = (self.magnified_rotated(), self.magnified_tile()) {
            paint_toast_turned(ctx, &toast.text, color, toast.center, tile);
            return;
        }
        let (anchor, offset) = if toast.center {
            (egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        } else {
            (egui::Align2::CENTER_BOTTOM, vec2(0.0, -56.0))
        };
        egui::Area::new(Id::new("xgview-toast"))
            // Above every modal. A toast is often the answer to a press made
            // inside one - the test tone's "the address is not complete", say -
            // and a modal's backdrop would otherwise cover it, which reads as a
            // button that did nothing.
            .order(egui::Order::Tooltip)
            .anchor(anchor, offset)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // Never wrapped: an area starts with no width of its own, and
                    // a label given one character's room stacks the rest under
                    // it - which a two-word fit name and a longer sentence both
                    // showed.
                    let label = egui::Label::new(RichText::new(&toast.text).color(color))
                        .wrap_mode(egui::TextWrapMode::Extend);
                    ui.add(label);
                });
            });
    }

    /// The question the wall asks on the first Back, in the middle of the screen.
    ///
    /// Middle, and not one of the corners the toasts use: this one is not news
    /// about a channel, it is the only thing on screen that wants an answer, and
    /// a viewer with a remote is looking at the middle.
    pub(super) fn draw_exit_hint(&self, ctx: &egui::Context) {
        let Some(at) = self.exit_armed else {
            return;
        };
        if at.elapsed() >= EXIT_WINDOW {
            return;
        }
        egui::Area::new(Id::new("xgview-exit-hint"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::symmetric(20, 14))
                    .show(ui, |ui| {
                        ui.label(RichText::new(monitor_i18n::tr("exit-hint")).size(18.0).strong());
                    });
            });
    }
}

/// A toast over a picture that is drawn a quarter turn.
///
/// The picture's frame is the screen turned clockwise: its `x` runs down the
/// screen and its `y` runs left, which is the mapping `tilebar::to_screen` gives
/// the strip and `grid::osd_corner_rotated` gives the corners. The toast is
/// placed in that frame - the middle of it, or its foot 56 points up, the two
/// places the level toast uses - and then drawn turned, so a viewer who has
/// turned the device to look at the picture reads it upright.
///
/// Painted rather than shown as an `Area`: egui turns no layer, so the frame and
/// the text are both laid out here. The frame is `Frame::popup`'s own fill and
/// stroke, taken from the style, so the turned toast and the level one are the
/// same box.
fn paint_toast_turned(
    ctx: &egui::Context,
    text: &str,
    color: Color32,
    centered: bool,
    tile: egui::Rect,
) {
    let style = ctx.style();
    let font = style.text_styles[&egui::TextStyle::Body].clone();
    let galley = ctx.fonts(|fonts| fonts.layout_no_wrap(text.to_owned(), font, color));
    let frame = egui::Frame::popup(&style);
    let pad = frame.inner_margin.sum();
    let box_ = turned_toast_box(tile, galley.size() + pad * 2.0, centered);
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        Id::new("xgview-toast-turned"),
    ));
    painter.rect(
        box_,
        frame.corner_radius,
        frame.fill,
        frame.stroke,
        egui::StrokeKind::Inside,
    );
    // A turned text shape is drawn from its top-left corner and extends left of
    // it and down, so the corner it starts at is the box's top-right, less the
    // padding - which the turn has swapped: the padding that was across the text
    // is now down the screen, and the one that was above it is across.
    let at = egui::pos2(box_.max.x - pad.y, box_.min.y + pad.x);
    painter.add(TextShape::new(at, galley, color).with_angle(std::f32::consts::FRAC_PI_2));
}

/// Where a toast's box lands on the screen over a picture drawn a quarter turn.
///
/// The picture's own frame has its sides traded - what the tile is tall across is
/// wide down - so the box is placed in that frame and then mapped: `x` runs down
/// the screen, `y` runs left from the tile's right edge, which is the mapping
/// `tilebar::to_screen` gives the strip and `grid::osd_corner_rotated` gives the
/// corners. The middle of the picture is the middle of the screen either way; the
/// foot of it is the screen's left edge.
fn turned_toast_box(tile: egui::Rect, size: egui::Vec2, centered: bool) -> egui::Rect {
    let local = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(tile.height(), tile.width()));
    let (anchor, offset) = if centered {
        (egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
    } else {
        (egui::Align2::CENTER_BOTTOM, vec2(0.0, -56.0))
    };
    let block = anchor.align_size_within_rect(size, local).translate(offset);
    let to_screen =
        |pos: egui::Pos2| egui::pos2(tile.right() - pos.y, tile.top() + pos.x);
    egui::Rect::from_two_pos(to_screen(block.min), to_screen(block.max))
}

#[cfg(test)]
mod turned_toast_tests {
    use super::*;

    /// The two places the level toast uses, read in the picture's frame.
    #[test]
    fn a_turned_toast_is_laid_out_in_the_picture_frame() {
        // A portrait screen with a landscape picture turned to fill it.
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 97.0), vec2(360.0, 700.0));
        let size = vec2(120.0, 34.0);

        let middle = turned_toast_box(tile, size, true);
        assert!((middle.center().x - tile.center().x).abs() < 0.01, "{middle:?}");
        assert!((middle.center().y - tile.center().y).abs() < 0.01, "{middle:?}");
        // Turned with the picture: the sides have traded.
        assert!((middle.width() - size.y).abs() < 0.01, "{middle:?}");
        assert!((middle.height() - size.x).abs() < 0.01, "{middle:?}");

        // At the foot of the picture, which the turn puts along the screen's
        // left edge - and inside the picture, not off it.
        let foot = turned_toast_box(tile, size, false);
        assert!(tile.contains_rect(foot), "{foot:?}");
        assert!(foot.left() < tile.left() + 60.0, "{foot:?}");
        assert!((foot.right() - (foot.left() + size.y)).abs() < 0.01, "{foot:?}");
    }
}
