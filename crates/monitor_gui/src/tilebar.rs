//! The floating bar inside the magnified viewport.
//!
//! The magnified view is one camera filling the wall, and the bar is the small
//! strip of controls that a press in the picture's top or bottom band brings up.
//! Everything about it is laid out in the *picture's* own frame and mapped onto
//! the screen at the end, so where the picture is drawn turned a quarter turn -
//! portrait, and magnified, see [`crate::grid`] - the bar turns with it and its
//! text comes out the right way up when the device is turned to watch.
//!
//! That mapping is the only place an orientation is read. [`layout`] takes
//! `rotated`, and `false` is the screen-aligned bar the landscape wall uses;
//! with it the same layout, hit test and painting serve both. If the turned bar
//! ever wants to be the level one, the switch is made in one place.
//!
//! The controls themselves are egui's: each is
//! [`crate::icons::button_in`], a button the caller places rather than one egui
//! lays out, and only the glyph inside it is drawn by hand - the same bargain
//! the toolbar's icon buttons make. That is what makes the bar behave like the
//! rest of the interface for free: a press lights its control while the pointer
//! is down, the keyboard's focus draws the ring, and the confirm key reaches it
//! because egui turns a press of it into a click on whatever has the focus. The
//! bar keeps only what is its own - whether it is up, which edge it is docked
//! to, and when it was last operated.
//!
//! It opens on a *seat*: a focusable, invisible, non-interactive widget that
//! draws nothing and answers nothing - the same trick the portrait picture fit
//! picker uses for its hint. The remote's first arrow walks off the seat into
//! the controls, and the ring appears then; until it does, the bar is on screen
//! without a cursor, and the confirm key has nothing to press.

use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Id, Pos2, Rect, Sense, Stroke, StrokeKind, vec2};

use crate::icons::{self, Icon};
use crate::nav::Nav;

/// How long the bar stays after the last operation on it.
///
/// Its own clock, like the focus ring's [`crate::app`] idle and deliberately
/// not the bars': the viewer asks for this one when they want a control, and it
/// has no business staying once they have stopped using it.
pub const IDLE: Duration = Duration::from_secs(8);

/// How long after it opens the bar ignores presses on itself.
///
/// A double click in the activation band is two clicks: the first opens the bar,
/// and the second arrives where the bar has just appeared - on a button, if the
/// bar is docked there. This window is egui's own double-click delay, so the
/// second click of the gesture that opened the bar can never press it.
const GUARD: Duration = Duration::from_millis(300);

/// Whether the bar carries the sound switch.
///
/// Audio playback landed for 1.5 (the plan is `docs/audio-plan.md` in the
/// private tree). This is the switch the magnified bar draws and the toggle the
/// core's audio hub follows.
pub const SOUND: bool = true;

/// Distance kept from the picture edge, in points.
///
/// Also what keeps the bar out of the band a touch screen's own gesture
/// navigation claims along the screen's edge.
const EDGE: f32 = 24.0;

/// Thickness of the strip, across the picture's edge.
const THICK: f32 = 56.0;

/// How far the strip reaches in from the picture edge.
///
/// The edge margin plus the thickness. The activation band is shallower, so
/// this is the whole depth a gesture for the bar can start within; see
/// `app::in_tile_bar_strip`.
pub const DEPTH: f32 = EDGE + THICK;

/// Side of a control across the strip, and of a square icon control along it.
const BUTTON_H: f32 = 44.0;

/// Gap between two controls.
const GAP: f32 = 8.0;

/// Padding between the controls and the strip's rounded end.
const PAD: f32 = 6.0;

/// Corner radius of the strip.
const RADIUS: u8 = 12;

/// What one control of the bar carries.
#[derive(Debug, Clone, Copy)]
pub enum Control {
    /// A shape, drawn square.
    Icon(Icon),
    /// The one control a call has: a red button carrying the handset that ends
    /// the talk-back. It takes over the whole bar while the viewer is talking,
    /// so that hanging up is one press and nothing else can be pressed by
    /// mistake.
    Hangup,
}

/// How much room an icon control asks for: square, the strip's depth.
pub fn icon_width() -> f32 {
    BUTTON_H
}

/// How many controls the bar can carry at once: the picture fit, the sound
/// switch and the microphone. A call takes all of them away and leaves the
/// handset in the first place.
///
/// A focus on any of these ids is a focus the bar owns, whether or not that
/// control is on screen this frame - the microphone a call has just turned into
/// the handset is one, and the handset it becomes is another.
pub const CONTROLS: usize = 3;

/// Which edge of the picture the bar is docked to, in the picture's own frame.
///
/// `Top` is the edge the picture's own top is on, which is the screen's right
/// edge while the picture is turned for a portrait wall - not the screen's top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dock {
    Top,
    Bottom,
}

/// Where a press landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// On one of the bar's controls. These are egui's buttons now, so the press
    /// itself is theirs; the answer is here for the caller's reading of a press
    /// that reached the picture underneath.
    Button(usize),
    /// On the strip, but not on a control.
    Inside,
    /// In the band along one picture edge, which is how the bar is asked for.
    Band(Dock),
    /// On the picture, away from all of it.
    Outside,
}

/// The bar's state, one per viewer.
///
/// `open` and the clock behind it are the whole of its life. The dock is
/// remembered across openings so a second press brings the bar back where it
/// was left; which control has the remote's focus is egui's, and the bar opens
/// with none - see the module's note on the seat.
#[derive(Debug)]
pub struct Bar {
    open: bool,
    dock: Dock,
    seen: Instant,
    ignore_until: Option<Instant>,
}

impl Bar {
    pub fn new() -> Self {
        Self {
            open: false,
            dock: Dock::Top,
            seen: Instant::now(),
            ignore_until: None,
        }
    }

    /// Whether the bar is on screen.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The edge the bar is docked to, which is where the next opening puts it.
    pub fn dock(&self) -> Dock {
        self.dock
    }

    /// Brings the bar up on `dock`, its controls unselected.
    ///
    /// The caller hands the focus to [`seat`]: the bar appears without a cursor,
    /// and the first arrow is what puts one on a control.
    pub fn show(&mut self, dock: Dock, now: Instant) {
        self.open = true;
        self.dock = dock;
        self.touch(now);
        self.ignore_until = Some(now + GUARD);
        tracing::debug!(target: "xgview::tilebar", ?dock, "the magnified bar is up");
    }

    /// Takes the bar down.
    pub fn hide(&mut self) {
        if self.open {
            tracing::debug!(target: "xgview::tilebar", "the magnified bar is down");
        }
        self.open = false;
        self.ignore_until = None;
    }

    /// Records an operation on the bar, which is what its clock measures from.
    pub fn touch(&mut self, now: Instant) {
        self.seen = now;
    }

    /// Whether a press on the bar is part of the gesture that opened it.
    pub fn ignoring(&self, now: Instant) -> bool {
        self.ignore_until.is_some_and(|until| now < until)
    }

    /// How long the bar has left, or `None` while it is down.
    ///
    /// The caller asks for a repaint after it, so a wall with no live picture -
    /// which is not repainting on its own - still takes the bar down on time.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.open.then(|| IDLE.saturating_sub(now.saturating_duration_since(self.seen)))
    }
}

impl Default for Bar {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the bar is still standing `now`, given when it was last operated.
pub fn standing(seen: Instant, now: Instant) -> bool {
    now.saturating_duration_since(seen) < IDLE
}

/// The seat the bar opens on: focusable, invisible, and not a control.
///
/// Nothing is drawn for it and nothing can be pressed on it - a confirm key there
/// is spent, which is the point: the ring is the arrows' cursor, and a bar that
/// has not been walked carries none.
pub fn seat() -> Id {
    Id::new(("xgview-tile-bar", "seat"))
}

/// The id of the control at `index`.
///
/// It is derived from the place in the strip rather than from the glyph, so a
/// control that changes its shape in place - the sound switch, the microphone
/// that becomes a call - keeps the remote's focus across the change.
pub fn control(index: usize) -> Id {
    Id::new(("xgview-tile-bar", index))
}

/// Whether `id` is one of the bar's own: its seat or one of its controls.
///
/// The seat is not a member of the navigation layer, so asking that layer where
/// an id belongs says nothing about it; this is the question the application
/// asks instead - is this focus the bar's, or has something else taken it. A
/// control that is not drawn this frame is still the bar's: the microphone a
/// call has just turned into the handset is one, and the handset it becomes is
/// another.
pub fn owns(id: Id) -> bool {
    id == seat() || (0..CONTROLS).any(|index| id == control(index))
}

/// The navigation scope the controls register under.
///
/// A row when the bar lies across the wall, a column when the picture has been
/// turned and the strip runs down the screen; the arrows that walk it are the
/// scope's axis and no others. Neither has an exit: a strip has nothing above or
/// below it to walk to, and `Back` is the way out.
pub fn scope(rotated: bool) -> &'static str {
    if rotated { "tile-bar-column" } else { "tile-bar-row" }
}

/// The bar and its two activation bands, in screen coordinates.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The strip itself.
    pub bar: Rect,
    /// One rectangle per control, in the picture's left-to-right order.
    pub buttons: Vec<Rect>,
    /// The band along the picture's top edge.
    pub top: Rect,
    /// The band along the picture's bottom edge.
    pub bottom: Rect,
}

impl Layout {
    /// The band along `dock`.
    pub fn band(&self, dock: Dock) -> Rect {
        match dock {
            Dock::Top => self.top,
            Dock::Bottom => self.bottom,
        }
    }

    /// Where a press at `pos` landed.
    ///
    /// The controls come before the strip and the strip before the bands: the
    /// strip lies inside a band, and a press on it is the strip's even though
    /// it is also in the band that asked for it.
    pub fn hit(&self, pos: Pos2) -> Hit {
        if let Some(index) = self.buttons.iter().position(|rect| rect.contains(pos)) {
            return Hit::Button(index);
        }
        if self.bar.contains(pos) {
            return Hit::Inside;
        }
        if let Some(dock) = self.band_at(pos) {
            return Hit::Band(dock);
        }
        Hit::Outside
    }

    /// The band along the picture edge `pos` is in, if it is in one.
    ///
    /// Read on its own, without the strip or its controls: a bar that is down
    /// has no controls to take a press, and the part of the band a control will
    /// be drawn over is still the band asking for the bar. The band is
    /// shallower than the strip, so it stops short of a control's middle - it
    /// is the edge that is being tapped, not the picture. See [`hit`](Self::hit)
    /// for the order a bar that *is* up is read in.
    pub fn band_at(&self, pos: Pos2) -> Option<Dock> {
        if self.top.contains(pos) {
            Some(Dock::Top)
        } else if self.bottom.contains(pos) {
            Some(Dock::Bottom)
        } else {
            None
        }
    }
}

/// Lays the bar out for `tile`, turned with the picture when `rotated`.
///
/// `widths` is how much room each control asks for along the strip - an icon
/// through [`icon_width`], so that the controls are square. The strip is centred
/// along the picture's horizontal axis - the edge's whole length - and its
/// thickness runs in from the edge by [`EDGE`]; both ends stay at least that far
/// inside the picture except for a tile too small to hold it, which the caller
/// declines to draw.
///
/// `top_band` and `bottom_band` are how deep the activation bands are, in
/// points. They are the heights of the bars the grid would have shown there -
/// the top toolbar and the bottom status bar - so the press that used to summon
/// a bar summons this one, in the same place and of the same depth. See
/// [`crate::app::XgViewApp`], which reads them from `Chrome::bars`.
pub fn layout(
    tile: Rect,
    rotated: bool,
    dock: Dock,
    widths: &[f32],
    top_band: f32,
    bottom_band: f32,
) -> Layout {
    let local = if rotated { vec2(tile.height(), tile.width()) } else { tile.size() };
    let length = widths.iter().sum::<f32>()
        + widths.len().saturating_sub(1) as f32 * GAP
        + 2.0 * PAD;
    let x = ((local.x - length) * 0.5).max(0.0);
    let y = match dock {
        Dock::Top => EDGE,
        Dock::Bottom => (local.y - EDGE - THICK).max(EDGE),
    };

    let bar = Rect::from_min_size(egui::pos2(x, y), vec2(length, THICK));
    let mut left = x + PAD;
    let buttons: Vec<Rect> = widths
        .iter()
        .map(|width| {
            let top = y + (THICK - BUTTON_H) * 0.5;
            let rect = Rect::from_min_size(egui::pos2(left, top), vec2(*width, BUTTON_H));
            left += *width + GAP;
            rect
        })
        .collect();
    let top = Rect::from_min_size(Pos2::ZERO, vec2(local.x, top_band.max(0.0)));
    let bottom = Rect::from_min_size(
        egui::pos2(0.0, local.y - bottom_band.max(0.0)),
        vec2(local.x, bottom_band.max(0.0)),
    );

    Layout {
        bar: to_screen_rect(bar, tile, rotated),
        buttons: buttons.into_iter().map(|rect| to_screen_rect(rect, tile, rotated)).collect(),
        top: to_screen_rect(top, tile, rotated),
        bottom: to_screen_rect(bottom, tile, rotated),
    }
}

/// Maps a point of the picture's frame onto the screen.
///
/// Unturned, the picture's frame is the tile. Turned a quarter turn clockwise -
/// which is how [`crate::grid`] draws a landscape picture into an upright tile -
/// the picture's left edge is the screen's top edge and its top edge is the
/// screen's right one, so its `x` runs down the screen and its `y` runs left.
fn to_screen(point: Pos2, tile: Rect, rotated: bool) -> Pos2 {
    if rotated {
        egui::pos2(tile.right() - point.y, tile.top() + point.x)
    } else {
        tile.min + point.to_vec2()
    }
}

/// [`to_screen`] for a rectangle. A quarter turn keeps an axis-aligned
/// rectangle axis-aligned, so both corners are enough.
fn to_screen_rect(rect: Rect, tile: Rect, rotated: bool) -> Rect {
    Rect::from_two_pos(to_screen(rect.min, tile, rotated), to_screen(rect.max, tile, rotated))
}

/// Draws the bar over the picture and answers the control it was pressed on.
///
/// The strip's backdrop is painted here, and the controls are egui buttons
/// placed where [`layout`] put them - their lit look, their ring and their
/// answer to the confirm key are all egui's. `rotated` turns their glyphs with
/// the picture, the way the picture itself is drawn.
///
/// The bar opens on [`seat`]: it is registered as the scope's first member so
/// the arrows have somewhere to stand before the first control, and so the
/// confirm key on it is spent rather than pressing the first control unseen.
pub fn draw(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    layout: &Layout,
    rotated: bool,
    controls: &[Control],
) -> Option<usize> {
    let painter = ui.painter();
    painter.rect_filled(layout.bar, CornerRadius::same(RADIUS), Color32::from_black_alpha(190));
    painter.rect_stroke(
        layout.bar,
        CornerRadius::same(RADIUS),
        Stroke::new(1.0_f32, Color32::from_white_alpha(28)),
        StrokeKind::Inside,
    );

    nav.open(scope(rotated));
    // The seat is a focus target, not a stop of the walk: it is claimed (as
    // nothing) so the focus can be put on it, but it is not registered with the
    // navigation layer. A member would be reachable from the first control by
    // the arrow that walks back, and the remote would step off the strip onto a
    // control that answers nothing; the application maps the first arrow off the
    // seat onto the first control instead. See `app::handle_keys`.
    let _seat = ui.interact(
        Rect::from_min_size(layout.bar.min, vec2(0.0, 0.0)),
        seat(),
        Sense::focusable_noninteractive(),
    );

    let mut pressed = None;
    for (index, rect) in layout.buttons.iter().enumerate() {
        let Some(control) = controls.get(index) else {
            continue;
        };
        let response = match control {
            Control::Icon(icon) => {
                icons::button_in(ui, *rect, self::control(index), *icon, false, false, "", rotated)
            }
            Control::Hangup => {
                icons::danger_in(ui, *rect, self::control(index), Icon::Hangup, "", rotated)
            }
        };
        nav.item(&response);
        if response.clicked() {
            pressed = Some(index);
        }
    }
    nav.close();
    pressed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn landscape() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(1920.0, 1080.0))
    }

    fn portrait() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(1080.0, 1920.0))
    }

    fn inside(outer: Rect, inner: Rect) -> bool {
        outer.contains(inner.min) && outer.contains(inner.max)
    }

    /// The bands a landscape wall shows: the toolbar over the picture and the
    /// status bar under it, at their drawn heights.
    const TOP_BAND: f32 = 33.0;
    const BOTTOM_BAND: f32 = 25.0;

    fn placed(tile: Rect, rotated: bool, dock: Dock, widths: &[f32]) -> Layout {
        layout(tile, rotated, dock, widths, TOP_BAND, BOTTOM_BAND)
    }

    /// The strip is inside the tile and against the picture edge it was docked
    /// to, in both orientations - which is the whole point of the mapping.
    #[test]
    fn the_bar_lies_inside_the_tile_against_the_picture_edge() {
        for tile in [landscape(), portrait()] {
            for rotated in [false, true] {
                for dock in [Dock::Top, Dock::Bottom] {
                    let layout = placed(tile, rotated, dock, &[icon_width()]);
                    assert!(inside(tile, layout.bar), "{tile:?} {rotated} {dock:?}: {:?}", layout.bar);
                    for button in &layout.buttons {
                        assert!(inside(layout.bar, *button), "{button:?} is not on the strip");
                    }

                    // The outer edge is `EDGE` in from the picture edge.
                    match (rotated, dock) {
                        (false, Dock::Top) => {
                            assert!((layout.bar.top() - (tile.top() + EDGE)).abs() < 0.01)
                        }
                        (false, Dock::Bottom) => {
                            assert!((layout.bar.bottom() - (tile.bottom() - EDGE)).abs() < 0.01)
                        }
                        (true, Dock::Top) => {
                            assert!((layout.bar.right() - (tile.right() - EDGE)).abs() < 0.01)
                        }
                        (true, Dock::Bottom) => {
                            assert!((layout.bar.left() - (tile.left() + EDGE)).abs() < 0.01)
                        }
                    }
                }
            }
        }
    }

    /// The controls are laid out along the picture's horizontal axis: left to
    /// right in the picture, which is the screen's top to bottom once turned.
    #[test]
    fn the_controls_run_from_the_pictures_left_to_its_right() {
        let two = placed(landscape(), false, Dock::Top, &[icon_width(), icon_width()]);
        assert!(two.buttons[0].center().x < two.buttons[1].center().x);

        let turned = placed(portrait(), true, Dock::Top, &[icon_width(), icon_width()]);
        assert!(turned.buttons[0].center().y < turned.buttons[1].center().y, "the picture's left is the screen's top");
        assert!((turned.buttons[0].center().x - turned.buttons[1].center().x).abs() < 0.01);
    }

    /// Every control is square - the strip's depth across and along - so a row
    /// of three is the three squares and their gaps, and nothing else.
    #[test]
    fn every_control_is_a_square_the_depth_of_the_strip() {
        let widths = [icon_width(); 3];
        let row = placed(landscape(), false, Dock::Top, &widths);
        for button in &row.buttons {
            assert!((button.width() - BUTTON_H).abs() < 0.01);
            assert!((button.height() - BUTTON_H).abs() < 0.01);
        }
        let span = row.buttons.last().unwrap().right() - row.buttons[0].left();
        assert!((span - (3.0 * BUTTON_H + 2.0 * GAP)).abs() < 0.01);
    }

    /// A press on a control is that control's, then the strip's, then the band
    /// it is docked in, and anything else is the picture's.
    #[test]
    fn a_press_is_read_from_the_control_outwards() {
        let one = [icon_width()];
        let layout = placed(landscape(), false, Dock::Top, &one);
        let button = layout.buttons[0].center();
        assert_eq!(layout.hit(button), Hit::Button(0));
        // The padding between the strip's edge and the control is the strip's.
        let padding = Pos2::new(layout.bar.left() + 2.0, layout.bar.center().y);
        assert_eq!(layout.hit(padding), Hit::Inside);
        // The band runs the width of the picture, and the strip lies inside it;
        // the edge of the strip is the strip's, not the band's.
        let band = layout.top;
        assert_eq!(layout.hit(Pos2::new(band.center().x, EDGE)), Hit::Inside);
        assert_eq!(layout.hit(Pos2::new(band.left() + 4.0, EDGE)), Hit::Band(Dock::Top));
        assert_eq!(layout.hit(Pos2::new(960.0, 540.0)), Hit::Outside);
    }

    /// The bar opens on the seat and carries no cursor: nothing to press until
    /// an arrow walks into the controls, and then the bar's clock is the only
    /// thing that can take it down.
    #[test]
    fn the_bar_opens_without_a_cursor_and_closes_when_told() {
        let now = Instant::now();
        let mut bar = Bar::new();
        assert!(!bar.is_open());
        bar.show(Dock::Bottom, now);
        assert!(bar.is_open());
        assert_eq!(bar.dock(), Dock::Bottom);
        assert!(bar.ignoring(now + Duration::from_millis(100)));
        assert!(!bar.ignoring(now + GUARD + Duration::from_millis(1)));

        assert!(bar.remaining(now).is_some());
        bar.hide();
        assert!(!bar.is_open());
        assert!(bar.remaining(now).is_none());
    }

    /// The idle is the bar's own clock and it is what takes it down; a call's
    /// bar is kept up by the caller touching it, see `app`.
    #[test]
    fn the_idle_ends_the_bar() {
        let now = Instant::now();
        assert!(standing(now, now));
        assert!(standing(now - IDLE + Duration::from_millis(100), now));
        assert!(!standing(now - IDLE, now), "the idle itself");
        assert!(!standing(now - IDLE - Duration::from_secs(1), now));
    }

    /// The scope is a row across the wall and a column down a turned picture, so
    /// that the arrows which walk the strip are the ones the viewer would push.
    #[test]
    fn the_scope_turns_with_the_picture() {
        assert_eq!(scope(false), "tile-bar-row");
        assert_eq!(scope(true), "tile-bar-column");
        assert_ne!(seat(), control(0), "the seat is not a control");
        assert_ne!(control(0), control(1));
    }
}
