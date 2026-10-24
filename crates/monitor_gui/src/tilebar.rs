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
//! The bar keeps its own state - whether it is up, which edge it is docked to,
//! which of its controls is selected and when it was last operated - and the
//! application owns an instance of [`Bar`]. Nothing here takes egui's focus: the
//! bar is part of the wall rather than one of its panels, and a wall that woke
//! its top and bottom bars every time the viewer touched this one would resize
//! the picture under their finger.

use std::time::{Duration, Instant};

use egui::epaint::TextShape;
use egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, vec2};

use crate::icons::{self, Icon};
use crate::theme;

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

/// The range a word's control is given along the strip.
///
/// A word is measured and its padding added, so a short one is not a wide empty
/// button and a long one is not clipped; the ends keep the strip from becoming a
/// speck or a scrollbar because one label happens to be short or long.
const TEXT_MIN: f32 = 56.0;
const TEXT_MAX: f32 = 96.0;

/// Gap between two controls.
const GAP: f32 = 8.0;

/// Padding between the controls and the strip's rounded end.
const PAD: f32 = 6.0;

/// Corner radius of the strip and of a control.
const RADIUS: u8 = 12;
const CONTROL_RADIUS: u8 = 8;

/// What one control of the bar carries.
#[derive(Debug, Clone, Copy)]
pub enum Control<'a> {
    /// A word, drawn in the bar's font.
    Text(&'a str),
    /// A shape, drawn square.
    Icon(Icon),
}

/// How much room a word asks for, measured in the font it is drawn with.
///
/// The label is the caller's, so the geometry cannot measure it; this is the
/// measurement, and the two must use the same font. The padding is added here
/// and the result is kept within [`TEXT_MIN`] and [`TEXT_MAX`].
pub fn text_width(ctx: &egui::Context, text: &str) -> f32 {
    let galley = ctx.fonts(|fonts| {
        fonts.layout_no_wrap(text.to_owned(), FontId::monospace(14.0), theme::TEXT)
    });
    (galley.size().x + 2.0 * PAD).clamp(TEXT_MIN, TEXT_MAX)
}

/// How much room an icon control asks for: square, the strip's depth.
pub fn icon_width() -> f32 {
    BUTTON_H
}

impl Control<'_> {
    /// How much room along the strip the control asks for.
    ///
    /// A word is measured in the font it is drawn with; an icon is square, so
    /// the strip of them reads as a row of squares beside the words.
    pub fn width(&self, ctx: &egui::Context) -> f32 {
        match self {
            Control::Text(text) => text_width(ctx, text),
            Control::Icon(_) => icon_width(),
        }
    }
}

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
    /// On one of the bar's controls.
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
/// was left; the selection starts at the first control each time.
#[derive(Debug)]
pub struct Bar {
    open: bool,
    dock: Dock,
    focus: usize,
    seen: Instant,
    ignore_until: Option<Instant>,
}

impl Bar {
    pub fn new() -> Self {
        Self {
            open: false,
            dock: Dock::Top,
            focus: 0,
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

    /// The control the remote is on.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Moves the selection to `index`.
    pub fn set_focus(&mut self, index: usize) {
        self.focus = index;
    }

    /// Brings the bar up on `dock`, with its first control selected.
    pub fn show(&mut self, dock: Dock, now: Instant) {
        self.open = true;
        self.dock = dock;
        self.focus = 0;
        self.touch(now);
        self.ignore_until = Some(now + GUARD);
    }

    /// Takes the bar down.
    pub fn hide(&mut self) {
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

    /// Moves the selection one control forward or back, stopping at the ends.
    pub fn step(&mut self, forward: bool, count: usize) {
        if count == 0 {
            return;
        }
        self.focus = if forward {
            (self.focus + 1).min(count - 1)
        } else {
            self.focus.saturating_sub(1)
        };
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
/// `widths` is how much room each control asks for along the strip; a word
/// through [`text_width`], an icon through [`icon_width`], so that the icons are
/// square. The strip is centred along the picture's horizontal axis - the edge's
/// whole length - and its thickness runs in from the edge by [`EDGE`]; both ends
/// stay at least that far inside the picture except for a tile too small to hold
/// it, which the caller declines to draw.
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

/// Paints the bar: its backdrop, its controls, and the selection among them.
///
/// `hover` is the control under the pointer, if any; `focus` is the one the
/// remote is on. Both are drawn, and both are told apart from a control that is
/// merely there by the same colours the rest of the interface uses. `controls`
/// carries one entry per control, in the same order as the layout's buttons; a
/// control with no entry keeps its shape and its selection ring.
pub fn paint(
    painter: &Painter,
    layout: &Layout,
    rotated: bool,
    focus: usize,
    hover: Option<usize>,
    controls: &[Control],
) {
    painter.rect_filled(layout.bar, CornerRadius::same(RADIUS), Color32::from_black_alpha(190));
    painter.rect_stroke(
        layout.bar,
        CornerRadius::same(RADIUS),
        Stroke::new(1.0_f32, Color32::from_white_alpha(28)),
        StrokeKind::Inside,
    );
    for (index, rect) in layout.buttons.iter().enumerate() {
        let selected = index == focus;
        if selected {
            painter.rect_filled(*rect, CornerRadius::same(CONTROL_RADIUS), theme::FOCUS_FILL);
        } else if hover == Some(index) {
            painter.rect_filled(*rect, CornerRadius::same(CONTROL_RADIUS), Color32::from_white_alpha(22));
        }
        if selected {
            painter.rect_stroke(
                *rect,
                CornerRadius::same(CONTROL_RADIUS),
                Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS),
                StrokeKind::Inside,
            );
        }
        // Clipped to the control: a label that grew with a translation must not
        // run out over its neighbour.
        let clip = painter.with_clip_rect(rect.shrink(2.0));
        match controls.get(index) {
            Some(Control::Text(text)) => label_text(&clip, *rect, text, rotated),
            Some(Control::Icon(icon)) => {
                let glyph = Rect::from_center_size(rect.center(), vec2(BUTTON_H * 0.5, BUTTON_H * 0.5));
                icons::paint_turned(&clip, glyph, *icon, Stroke::new(1.6_f32, theme::TEXT), rotated);
            }
            None => {}
        }
    }
}

/// Draws `text` centred in `rect`, turned a quarter turn with the picture.
fn label_text(painter: &Painter, rect: Rect, text: &str, rotated: bool) {
    let font = FontId::monospace(14.0);
    if rotated {
        let galley = painter.layout_no_wrap(text.to_owned(), font, theme::TEXT);
        let size = galley.size();
        // A line turned 90° clockwise at `pos` occupies `(pos.x - h, pos.y)` to
        // `(pos.x, pos.y + w)`; that box is centred on `rect`.
        let pos = egui::pos2(rect.center().x + size.y * 0.5, rect.center().y - size.x * 0.5);
        painter.add(TextShape::new(pos, galley, theme::TEXT).with_angle(std::f32::consts::FRAC_PI_2));
    } else {
        painter.text(rect.center(), Align2::CENTER_CENTER, text, font, theme::TEXT);
    }
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
                    let layout = placed(tile, rotated, dock, &[TEXT_MIN]);
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
        let two = placed(landscape(), false, Dock::Top, &[TEXT_MIN, TEXT_MIN]);
        assert!(two.buttons[0].center().x < two.buttons[1].center().x);

        let turned = placed(portrait(), true, Dock::Top, &[TEXT_MIN, TEXT_MIN]);
        assert!(turned.buttons[0].center().y < turned.buttons[1].center().y, "the picture's left is the screen's top");
        assert!((turned.buttons[0].center().x - turned.buttons[1].center().x).abs() < 0.01);
    }

    /// An icon control is square: beside a word it takes the strip's depth, not
    /// another word's width, and the strip grows by exactly that.
    #[test]
    fn an_icon_control_is_square() {
        let level = placed(landscape(), false, Dock::Top, &[TEXT_MIN, BUTTON_H]);
        let word = level.buttons[0];
        let icon = level.buttons[1];
        assert!((word.width() - TEXT_MIN).abs() < 0.01);
        assert!((icon.width() - icon.height()).abs() < 0.01, "the icon is square");
        assert!(
            (level.bar.width() - (2.0 * PAD + TEXT_MIN + GAP + BUTTON_H)).abs() < 0.01,
            "the strip is the two controls and the gaps"
        );
    }

    /// A word's control is as wide as the word and its padding, and no wider or
    /// narrower than the strip allows.
    #[test]
    fn a_word_is_measured_within_the_strips_limits() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let short = text_width(&ctx, "4:3");
        let long = text_width(&ctx, "stretch");
        assert!(short >= TEXT_MIN && short <= TEXT_MAX, "{short}");
        assert!(long >= TEXT_MIN && long <= TEXT_MAX, "{long}");
        assert!(long > short, "{long} is not wider than {short}");
    }

    /// The bands lie along the picture's top and bottom edges: the screen's top
    /// and bottom on a landscape wall, and the screen's right and left once the
    /// picture is turned for a portrait one.
    #[test]
    fn the_bands_sit_on_the_pictures_top_and_bottom_edges() {
        let level = placed(landscape(), false, Dock::Top, &[TEXT_MIN]);
        assert!((level.top.top() - landscape().top()).abs() < 0.01);
        assert!((level.top.height() - TOP_BAND).abs() < 0.01, "as deep as the toolbar");
        assert!((level.bottom.bottom() - landscape().bottom()).abs() < 0.01);
        assert!((level.bottom.height() - BOTTOM_BAND).abs() < 0.01, "as deep as the status bar");

        let turned = placed(portrait(), true, Dock::Top, &[TEXT_MIN]);
        assert!((turned.top.right() - portrait().right()).abs() < 0.01, "the picture's top is the screen's right");
        assert!((turned.top.width() - TOP_BAND).abs() < 0.01);
        assert!((turned.bottom.left() - portrait().left()).abs() < 0.01);
        assert!((turned.bottom.width() - BOTTOM_BAND).abs() < 0.01);
    }

    /// A press is read as the control it is on, the strip's background, the band
    /// that asked for it, or the picture - in that order.
    #[test]
    fn a_press_is_read_by_what_it_landed_on() {
        let tile = landscape();
        let level = placed(tile, false, Dock::Top, &[TEXT_MIN]);

        assert_eq!(level.hit(level.buttons[0].center()), Hit::Button(0));
        // The strip's padding, below the control.
        let inside = egui::pos2(level.bar.center().x, level.buttons[0].bottom() + 3.0);
        assert!(level.bar.contains(inside), "{inside:?} in {:?}", level.bar);
        assert_eq!(level.hit(inside), Hit::Inside);
        // The band, above the strip.
        assert_eq!(level.hit(egui::pos2(tile.center().x, 8.0)), Hit::Band(Dock::Top));
        // A bar that is down has no controls to take a press, and the part of
        // the band a control will be drawn over is still the band asking for
        // the bar. The band is only as deep as the toolbar that used to be
        // there, so it stops short of the control's middle.
        assert_eq!(level.band_at(egui::pos2(level.buttons[0].center().x, 30.0)), Some(Dock::Top));
        assert_eq!(level.band_at(level.buttons[0].center()), None, "the band is shallower than the control");
        // The picture, away from both.
        assert_eq!(level.hit(tile.center() + vec2(0.0, 100.0)), Hit::Outside);
        assert_eq!(level.band_at(tile.center() + vec2(0.0, 100.0)), None);

        let turned = placed(portrait(), true, Dock::Bottom, &[TEXT_MIN]);
        assert_eq!(turned.hit(turned.buttons[0].center()), Hit::Button(0));
        assert_eq!(turned.band_at(turned.buttons[0].center()), None, "the band is shallower than the control");
        assert_eq!(turned.band_at(egui::pos2(10.0, 960.0)), Some(Dock::Bottom), "the status bar's depth");
        assert_eq!(turned.hit(egui::pos2(20.0, 400.0)), Hit::Band(Dock::Bottom), "the picture's bottom is the screen's left");
    }

    /// The bar keeps its own clock: eight seconds after the last operation, and
    /// not at the moment the idle itself arrives.
    #[test]
    fn the_bar_stands_for_eight_seconds_after_the_last_operation() {
        let now = Instant::now();
        assert!(standing(now, now));
        assert!(standing(now - IDLE + Duration::from_millis(100), now));
        assert!(!standing(now - IDLE, now), "the idle itself");
        assert!(!standing(now - IDLE - Duration::from_secs(1), now));
    }

    /// Opening ignores presses on the bar for the double-click delay, so the
    /// second click of a double click in the band cannot press a control; the
    /// selection walks its controls and stops at the ends.
    #[test]
    fn the_open_gesture_cannot_press_the_bar_and_the_selection_stops_at_its_ends() {
        let now = Instant::now();
        let mut bar = Bar::new();
        assert!(!bar.is_open());
        bar.show(Dock::Bottom, now);
        assert!(bar.is_open());
        assert_eq!(bar.dock(), Dock::Bottom);
        assert!(bar.ignoring(now + Duration::from_millis(100)));
        assert!(!bar.ignoring(now + GUARD + Duration::from_millis(1)));

        bar.step(false, 1);
        assert_eq!(bar.focus(), 0, "nothing before the first control");
        bar.step(true, 1);
        assert_eq!(bar.focus(), 0, "nothing after the last one");

        bar.step(true, 3);
        assert_eq!(bar.focus(), 1);
        bar.step(false, 3);
        assert_eq!(bar.focus(), 0);

        assert!(bar.remaining(now).is_some());
        bar.hide();
        assert!(!bar.is_open());
        assert!(bar.remaining(now).is_none());
    }
}
