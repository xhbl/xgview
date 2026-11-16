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
use monitor_core::recording_download::Progress;

/// How long the bar stays after the last operation on it.
///
/// Its own clock, like the focus ring's [`crate::app`] idle and deliberately
/// not the bars': the viewer asks for this one when they want a control, and it
/// has no business staying once they have stopped using it.
pub const IDLE: Duration = Duration::from_secs(8);

/// How long a question the box has not answered yet keeps the bar up.
///
/// The bar's own clock takes it down after eight seconds of no answer to it.
/// A question that is on its way - a recording being opened - is an answer the
/// viewer is waiting for, and the bar is where that answer lands: a bar that
/// went down in the middle of the wait took the ring with it, and the ring is
/// the viewer's cursor on the controls the answer is about to replace. The
/// hold is for a while rather than for ever, so a question the box never
/// answers does not keep the bar up for good.
pub const HOLD: Duration = Duration::from_secs(30);

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

/// Height of the timeline row, under the controls.
///
/// Deep enough for a row of times, the ruler under them, the track and the
/// moment under the cursor, and no deeper: the panel covers the picture wherever
/// it has no black to sit in.
pub const TIMELINE: f32 = 58.0;

/// The timeline row's own depth: [`TIMELINE`] less the [`PAD`] it is inset by at
/// the docked edge.
///
/// What the row is drawn in, so a band placed as a fraction of it lands where
/// the same fraction would on any other row - see `across` in
/// `XgViewApp::draw_timeline`.
pub const TIMELINE_ROW: f32 = TIMELINE - PAD;

/// The panel's whole depth when it carries a timeline and one row of controls.
///
/// Two rows of controls are [`FULL`] plus another row; see [`depth`].
pub const FULL: f32 = INNER + THICK + ROW_GAP + TIMELINE;

/// The panel's depth with `rows` rows of controls, and a timeline under them
/// when `timeline`.
///
/// A narrow picture cannot hold every control in one line - the bar is as long
/// as the picture is wide - so the controls wrap and the panel deepens. Which is
/// why this is a function and not the constant it used to be.
pub fn depth(rows: usize, timeline: bool) -> f32 {
    let rows = rows.max(1) as f32;
    let controls = rows * THICK + (rows - 1.0) * ROW_GAP;
    if timeline {
        INNER + controls + ROW_GAP + TIMELINE
    } else {
        INNER + controls + INNER
    }
}

/// How deep the panel is across a picture `across` points long.
///
/// The same number [`layout`] arrives at, asked before the caller has a tile to
/// lay the bar out in. Upright, the strip has a band of the screen below the
/// picture rather than the picture's own black, and how deep that band is
/// depends on how many lines the controls take - which depends on the width
/// alone. Nothing here looks at a height, so a caller can ask this first and
/// place the picture afterwards, in the same frame, with no second pass: a band
/// measured from a panel it has already placed would have to be placed again.
///
/// `across` is the length of the edge the strip runs along - the picture's
/// width for a picture as it arrives, its height for one drawn turned. [`layout`]
/// asks the same question of the same length, which is what keeps the two
/// answers one.
pub fn panel_depth(across: f32, widths: &[f32], breaks: &[usize], timeline: bool) -> f32 {
    let available = (across - 2.0 * EDGE).max(0.0);
    depth(wrap(widths, available, breaks).len(), timeline)
}

/// Whether the bar paints the plate it floats on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    /// A rounded, half-transparent plate over the picture.
    ///
    /// What the strip has always had, and what it still has wherever it lies
    /// over the picture itself: landscape, and a picture drawn turned.
    Floating,
    /// Nothing: the caller has painted the ground the strip lies on.
    ///
    /// Upright and not turned, the strip has a band of the screen of its own
    /// under the picture, and the band is its ground. A plate inside a band
    /// would be a second frame inside the wall's own - the wall draws one ring
    /// around the whole of its area, and the bar must not draw another. See
    /// `XgViewApp::panel_split`.
    Bare,
}

/// Which controls go on which line, in order.
///
/// Greedy, and the line breaks where the next control would not fit, or where
/// `per_row` says it must: a strip laid out by arithmetic rather than by eye,
/// where the alternative - balancing the lines - would move a control the viewer
/// has just learned the place of every time one was added.
///
/// `breaks` is what makes a recording's lines the lines it means: on a phone
/// held upright the way back to live on the first, the transport and the save
/// on the second, and the timeline's own controls on the third, rather than
/// whatever a greedy fill happens to leave on each. Each entry is the index a
/// new line starts at; an empty slice leaves the width to decide.
fn wrap(widths: &[f32], available: f32, breaks: &[usize]) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut row: Vec<usize> = Vec::new();
    let mut used = 0.0;
    for (index, width) in widths.iter().enumerate() {
        let wider = if row.is_empty() { *width } else { used + GAP + *width };
        let broken = breaks.contains(&index);
        if !row.is_empty() && (broken || wider + 2.0 * PAD > available) {
            rows.push(std::mem::take(&mut row));
            used = *width;
        } else {
            used = wider;
        }
        row.push(index);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// How much room the controls of `row` ask for along the strip.
fn row_width(row: &[usize], widths: &[f32]) -> f32 {
    row.iter().map(|index| widths[*index]).sum::<f32>()
        + row.len().saturating_sub(1) as f32 * GAP
        + 2.0 * PAD
}

/// The widest the panel is drawn, timeline or not.
///
/// A timeline wider than this asks the eye to travel further than it can follow
/// a scrub; the picture may be wider still.
const WIDEST: f32 = 1180.0;

/// Distance kept from the picture edge along the strip, in points.
///
/// Also the default distance from the docked edge: a caller may ask for less,
/// and a recording does on a wide picture - see `XgViewApp::tile_bar_margin`.
pub const EDGE: f32 = 24.0;

/// Thickness of the strip, across the picture's edge.
///
/// Forty-four: a forty-point control with a point above and below it. Two lines
/// of them are then [`ROW_GAP`] and two points apart, which is the seven a
/// viewer sees - half of what two forty-eight-point lines with six between them
/// left.
const THICK: f32 = 44.0;

/// How far the strip reaches in from the picture edge.
///
/// The edge margin plus the thickness. The activation band is shallower, so
/// this is the whole depth a gesture for the bar can start within; see
/// `app::in_tile_bar_strip`.
pub const DEPTH: f32 = EDGE + THICK;

/// Side of a control across the strip, and of a square icon control along it.
///
/// Forty, where the plan said forty-four. A recording carries thirteen
/// controls - the transport, the timeline's zoom, the way back to live and the
/// save - and a phone held upright puts them on three deliberately short lines:
/// `[Live]`, `[play 1x calendar fit sound save]`, and the six timeline keys.
/// Six at forty points, their gaps and the edge margins fit the 363 a 411-wide
/// picture leaves. Three lines fit because an unrotated portrait tile is about
/// 1350 points deep and the bar sits in the letterbox's black below the picture:
/// the old worry that a third line would make the panel deeper than the picture
/// belonged to a narrow fit where the bar covered the picture itself.
const BUTTON_H: f32 = 40.0;

/// Gap between two controls on one line.
const GAP: f32 = 6.0;

/// Room between the controls and the panel's own edge, across the strip.
///
/// Four, because a control flush with the panel's rounded corner reads as
/// clipped by it. At the far edge always, and at the docked edge only when there
/// is no timeline under the controls - a timeline brings its own inset, and the
/// row is where the panel's edge is.
const INNER: f32 = 4.0;

/// Gap between two lines of controls, across the strip.
///
/// Three, where the gap along a line is six: the lines are two halves of one
/// panel and read as a block, so they sit closer to each other than the controls
/// within a line do. With a forty-point control in a forty-four-point line, the
/// space a viewer sees between two lines is seven points - half of the fourteen
/// it was.
const ROW_GAP: f32 = 3.0;

/// Padding between the controls and the strip's rounded end.
const PAD: f32 = 6.0;

/// Corner radius of the strip.
const RADIUS: u8 = 12;

/// What one control of the bar carries.
#[derive(Debug, Clone, Copy)]
pub enum Control {
    /// A shape, drawn square.
    Icon(Icon),
    /// A short word, drawn square: the speed the recording plays at, `1×` or
    /// `4×`. The one control whose state cannot be a shape - a picture cannot
    /// say whether it is two times or four.
    Label(&'static str),
    /// A download in flight, drawn in the save button's own place: a percentage
    /// when the box said how long the file would be, a count of megabytes when
    /// it did not. It is a label rather than a shape because no shape can say
    /// how much of a clip has arrived.
    Progress(Progress),
    /// The one control a call has: a red button carrying the handset that ends
    /// the talk-back. It takes over the whole bar while the viewer is talking,
    /// so that hanging up is one press and nothing else can be pressed by
    /// mistake.
    Hangup,
    /// A place in the row that carries no action, drawn as a grey speaker.
    ///
    /// The sound switch is the one control that comes and goes with the
    /// picture: a stream that has not said whether it carries an audio track
    /// yet, and one that has said it does not, are both rows without it. The
    /// place it would have stood is kept, so that the controls after it do not
    /// move when it does - a row that shifts under the remote's ring is a row
    /// the viewer cannot aim at. It is kept *visible* too: an empty hole where
    /// a button belongs reads as something missing, while a grey speaker says
    /// what is missing. See `XgViewApp::tile_bar_tips`.
    Silent,
    /// The one mark a strip carries while the box answers a question about a
    /// recording. Every control is away for that wait: a live row would still
    /// offer a microphone, another way into the recording and a picture to
    /// turn, all of which are about to be replaced. The mark turns with the
    /// clock so the press is known to have been heard, and it is not a control
    /// - nothing presses it, nothing focuses it, and the walk has no stop on
    /// it. See `XgViewApp::playback_is_asking`.
    Waiting,
}

/// How much room an icon control asks for: square, the strip's depth.
pub fn icon_width() -> f32 {
    BUTTON_H
}

/// The word the save button carries while a clip is arriving.
///
/// An exported file comes from the box's disk with a length, so the answer is
/// usually a percentage; a chunked answer has none and is counted in megabytes
/// instead. Both have to fit a forty-point square, hence four characters.
pub fn progress_text(progress: Progress) -> String {
    // Nothing counted yet: the box is still cutting the file, or the first
    // bytes are still on their way. Zero percent says a download has begun and
    // is at its beginning; a dash said only that there was no number to show
    // and read as nothing happening at all.
    if progress.preparing || progress.received == 0 {
        return "0%".into();
    }
    match progress.percent() {
        Some(percent) => format!("{percent}%"),
        None => {
            let megabytes = progress.megabytes();
            if megabytes < 10.0 {
                format!("{megabytes:.1}M")
            } else {
                format!("{megabytes:.0}M")
            }
        }
    }
}

/// How many controls the bar can carry at once.
///
/// Thirteen: a recording's whole row - the quarter turn on a portrait wall, the
/// two steps, play, the two event jumps, the timeline's width, the sound switch,
/// the save and the way back to the camera - with a place for each. The live bar
/// is shorter, and a call takes all of them away and leaves the handset in the
/// first place.
///
/// A focus on any of these ids is a focus the bar owns, whether or not that
/// control is on screen this frame - the microphone a call has just turned into
/// the handset is one, and the handset it becomes is another.
pub const CONTROLS: usize = 13;

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
    /// On the picture, away from the bar.
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
    held_until: Option<Instant>,
}

impl Bar {
    pub fn new() -> Self {
        Self {
            open: false,
            dock: Dock::Top,
            seen: Instant::now(),
            ignore_until: None,
            held_until: None,
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
        // A bar that is down is a bar with nothing to hold up: the question it
        // was waiting for has nowhere to land, and the next opening is the
        // viewer's own.
        self.held_until = None;
    }

    /// Keeps the bar up while a question is on its way.
    ///
    /// The answer is the box's to give and in its own time; the bar is where it
    /// lands, and a bar that went down in the middle of the wait took the
    /// viewer's cursor with it. See [`HOLD`].
    pub fn hold(&mut self, now: Instant) {
        self.held_until = Some(now + HOLD);
    }

    /// Whether the bar is being held up by a question that has not been answered.
    pub fn held(&self, now: Instant) -> bool {
        self.held_until.is_some_and(|until| now < until)
    }

    /// Lets the bar's own clock take it down again.
    ///
    /// The clock starts again here: the hold kept the bar up while the box was
    /// thinking, and a bar that had been waiting an answer for half a minute is
    /// not a bar the viewer has stopped using - it is one they are about to
    /// use, on the row the answer has just put there.
    pub fn release(&mut self, now: Instant) {
        self.held_until = None;
        self.touch(now);
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
    /// That repaint is also what ends a hold: a bar that is held up is a bar
    /// with no time of its own to run, so the time it has left is the time the
    /// hold has left, and the wall is asked to come back when it runs out.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        if !self.open {
            return None;
        }
        let idle = IDLE.saturating_sub(now.saturating_duration_since(self.seen));
        match self.held_until {
            Some(until) if now < until => Some((until - now).max(idle)),
            _ => Some(idle),
        }
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
    id == seat() || index_of(id).is_some()
}

/// Which place in the bar's row an id is, if it is one of its controls.
///
/// The place is what the remote's focus is remembered by - the strip hands it
/// back to the control it was taken from - and what the control's own id is
/// built from.
pub fn index_of(id: Id) -> Option<usize> {
    (0..CONTROLS).find(|index| control(*index) == id)
}

/// The first arrow off the seat, whichever way it points.
///
/// The seat sits in the bar's top-left corner, and the row has nothing above
/// it or to its left. egui walks the focus geometrically when the focused
/// widget does not claim an arrow, so Down and Right happened to land on the
/// first button while Up and Left went off to the wall's own bars. Every
/// direction is the same request: light the first button. The seat keeps all
/// four arrows for itself (see `draw`), which is what makes this stick.
pub fn first_arrow_target(open: bool, focused: Option<Id>, arrow: bool) -> Option<Id> {
    (open && focused == Some(seat()) && arrow).then(|| control(0))
}

/// Whether Down from this focus is the recording strip's way in.
///
/// The seat is not a control. Its Down is the first arrow onto the first
/// button, and a strip that took it would leave the bar with no ring on any
/// control - which is why a recording used to answer Down with a ring on the
/// whole timeline row instead. From a control, Down still enters the strip
/// exactly as it always has.
pub fn down_enters_strip(focused: Option<Id>) -> bool {
    focused.and_then(index_of).is_some()
}

/// The next control in the row that is still live, counting from `index` and
/// round the end - `None` when every one of them is grey.
///
/// A greyed control is not in the walk and takes no press, so the focus it holds
/// has to go somewhere: this is where. Round the end rather than to the seat,
/// because a viewer who walked to the far end of the bar expects the row to go
/// on, and only a row with nothing live in it has nowhere to go.
pub fn next_live(live: &[bool], index: usize) -> Option<usize> {
    if live.is_empty() {
        return None;
    }
    (1..=live.len())
        .map(|step| (index + step) % live.len())
        .find(|at| live[*at])
}

/// Whether a tile has room for the panel's two rows.
///
/// The panel's depth runs along the tile's height, or along its width once the
/// picture is drawn turned. A panel deeper than the picture is no longer a strip
/// of controls over it, and a recording is scrubbed along a timeline that cannot
/// be anywhere else - so this is the one thing that keeps the row off a picture
/// down to a postage stamp.
pub fn fits_timeline(
    tile: Rect,
    rotated: bool,
    margin: f32,
    widths: &[f32],
    breaks: &[usize],
) -> bool {
    let across = if rotated { tile.width() } else { tile.height() };
    let local = if rotated { vec2(tile.height(), tile.width()) } else { tile.size() };
    let rows = wrap(widths, (local.x - 2.0 * EDGE).max(0.0), breaks);
    across >= margin + depth(rows.len(), true)
}

/// The navigation scope the controls register under.
///
/// One scope for both ways the strip lies, and it is a row either way. A turned
/// strip runs down the screen and a column of controls is what the eye sees, but
/// the walk is still the picture's own left-to-right order, and Left and Right
/// are the arrows that move it: the quarter turn is how the picture is drawn, not
/// a second axis for the hand. Walking it with Up and Down was tried - the strip
/// took the vertical arrows and the ring moved against the pair the viewer was
/// pressing, because on a remote those are the ones that leave a strip, not walk
/// it. The scope has no exit: a strip has nothing beyond its ends, and `Back` is
/// the way out.
pub fn scope() -> &'static str {
    "tile-bar-row"
}

/// The bar, in screen coordinates.
///
/// It used to carry the two activation bands as well - the strips along the
/// picture's edges a press opened the bar from. A tap now opens it anywhere on
/// the picture, so the bands are gone; what the bar answers is where it is.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The strip itself.
    pub bar: Rect,
    /// One rectangle per control, in the picture's left-to-right order.
    pub buttons: Vec<Rect>,
    /// The row under the controls, when the panel carries a timeline.
    ///
    /// In the picture's own frame like everything else here, so a turned
    /// picture carries a timeline running down the screen.
    pub timeline: Option<Rect>,
}

impl Layout {
    /// Where a press at `pos` landed.
    ///
    /// The controls come before the strip: a press on a control is that
    /// control's, the strip's padding is the strip's, and anything else is the
    /// picture's.
    pub fn hit(&self, pos: Pos2) -> Hit {
        if let Some(index) = self.buttons.iter().position(|rect| rect.contains(pos)) {
            return Hit::Button(index);
        }
        if self.bar.contains(pos) {
            return Hit::Inside;
        }
        Hit::Outside
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
/// The side it is docked to is the picture's own: `Dock::Bottom` is the edge the
/// status bar would be on in a landscape wall, and the screen's left edge once
/// the picture is turned.
pub fn layout(
    tile: Rect,
    rotated: bool,
    dock: Dock,
    margin: f32,
    widths: &[f32],
    breaks: &[usize],
    timeline: bool,
) -> Layout {
    let local = if rotated { vec2(tile.height(), tile.width()) } else { tile.size() };
    // The room a line of controls has: the picture's own length, less the margin
    // the panel keeps from each end.
    let available = (local.x - 2.0 * EDGE).max(0.0);
    let rows = wrap(widths, available, breaks);
    let widest = rows.iter().map(|row| row_width(row, widths)).fold(0.0, f32::max);
    // A timeline wants the width of the picture, less the margin it keeps from
    // the edge - and no more than `WIDEST`, where a scrub would ask the eye to
    // travel further than it can follow. The controls are centred in whatever
    // it turns out to be.
    let length = if timeline {
        widest.max(available.min(WIDEST))
    } else {
        widest
    };
    let depth = depth(rows.len(), timeline);
    let x = ((local.x - length) * 0.5).max(0.0);
    let y = match dock {
        Dock::Top => margin,
        Dock::Bottom => (local.y - margin - depth).max(margin),
    };

    let bar = Rect::from_min_size(egui::pos2(x, y), vec2(length, depth));
    // The controls sit against the picture, away from the edge the panel is
    // docked to: the timeline is the row nearest that edge, so the controls
    // rise by one row when it appears rather than jumping to another edge - and
    // by two when they wrap.
    let controls_depth = if timeline {
        depth - INNER - ROW_GAP - TIMELINE
    } else {
        depth - 2.0 * INNER
    };
    // The controls sit against the far edge of the panel with [`INNER`] between
    // them and it, and against the docked edge only when nothing is under them.
    let controls_top = match dock {
        Dock::Top => y + depth - INNER - controls_depth,
        Dock::Bottom => y + INNER,
    };
    let mut buttons = vec![Rect::NOTHING; widths.len()];
    for (line, row) in rows.iter().enumerate() {
        let top = controls_top + line as f32 * (THICK + ROW_GAP);
        // Centred in the panel, which is the picture's width when there is a
        // timeline under it: a line of controls pushed against one end of a wide
        // panel reads as belonging to that end.
        let used = row_width(row, widths) - 2.0 * PAD;
        let mut left = x + (length - used) * 0.5;
        for index in row {
            let at = top + (THICK - BUTTON_H) * 0.5;
            buttons[*index] = Rect::from_min_size(egui::pos2(left, at), vec2(widths[*index], BUTTON_H));
            left += widths[*index] + GAP;
        }
    }
    let row = match dock {
        Dock::Top => Rect::from_min_max(egui::pos2(x + PAD, y + PAD), egui::pos2(x + length - PAD, y + TIMELINE)),
        Dock::Bottom => Rect::from_min_max(
            egui::pos2(x + PAD, y + INNER + controls_depth + ROW_GAP),
            egui::pos2(x + length - PAD, y + depth - PAD),
        ),
    };
    Layout {
        bar: to_screen_rect(bar, tile, rotated),
        buttons: buttons.into_iter().map(|rect| to_screen_rect(rect, tile, rotated)).collect(),
        timeline: timeline.then(|| to_screen_rect(row, tile, rotated)),
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
/// The strip's backdrop is painted here unless the caller has already painted
/// the ground it lies on - see [`Backdrop`] - and the controls are egui buttons
/// placed where [`layout`] put them: their lit look, their ring and their
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
    backdrop: Backdrop,
    controls: &[Control],
    muted: &[bool],
    tips: &[String],
) -> Option<usize> {
    let painter = ui.painter();
    if backdrop == Backdrop::Floating {
        painter.rect_filled(layout.bar, CornerRadius::same(RADIUS), Color32::from_black_alpha(153));
        painter.rect_stroke(
            layout.bar,
            CornerRadius::same(RADIUS),
            Stroke::new(1.0_f32, Color32::from_white_alpha(28)),
            StrokeKind::Inside,
        );
    }

    // The timeline's track, drawn here so the row is a row and not a gap. What
    // goes on it - the stretches that were recorded, the events in them, the
    // cursor - is drawn by the playback view, which is the only thing that knows
    // what a camera recorded.
    if let Some(row) = layout.timeline {
        // The track runs along the row's long side, which on screen is across
        // for a picture as it arrives and down for one drawn turned.
        let middle = row.center();
        let track = if row.width() > row.height() {
            Rect::from_min_max(
                egui::pos2(row.left(), middle.y - 1.0),
                egui::pos2(row.right(), middle.y + 1.0),
            )
        } else {
            Rect::from_min_max(
                egui::pos2(middle.x - 1.0, row.top()),
                egui::pos2(middle.x + 1.0, row.bottom()),
            )
        };
        painter.rect_filled(track, CornerRadius::same(1), Color32::from_white_alpha(26));
    }

    nav.open(scope());
    // The seat is a focus target, not a stop of the walk: it is claimed (as
    // nothing) so the focus can be put on it, but it is not registered with the
    // navigation layer. A member would be reachable from the first control by
    // the arrow that walks back, and the remote would step off the strip onto a
    // control that answers nothing; the application maps the first arrow off the
    // seat onto the first control instead. See `app::handle_keys`.
    // A real rectangle, small as it is: egui leaves a zero-area widget out of the
    // widgets the pointer can reach, and a focus asked for an id that is not
    // among them does not stick - the bar then loses its seat on the next frame
    // and the focus goes with it.
    let seat_rect = Rect::from_min_size(layout.bar.min + vec2(PAD * 0.5, PAD * 0.5), vec2(2.0, 2.0));
    let seat = ui.interact(seat_rect, seat(), Sense::focusable_noninteractive());
    // The seat keeps the arrows for the bar as well: egui moves the focus to
    // whatever is in the direction of an arrow the focused widget does not claim,
    // and the rule here is that the first arrow steps off the seat onto the first
    // control - or, for Down, into the strip under it. See `Nav::finish`, which
    // does the same for the controls themselves.
    if seat.has_focus() {
        ui.ctx().memory_mut(|memory| {
            memory.set_focus_lock_filter(
                seat.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
    }

    let mut pressed = None;
    for (index, rect) in layout.buttons.iter().enumerate() {
        let Some(control) = controls.get(index) else {
            continue;
        };
        // A control with nothing beyond it - a wider strip than the ladder has -
        // is drawn grey and takes no press. It keeps its **place** in the row: a
        // bar whose controls moved about as a recording played would be a bar
        // nobody could aim at. It does not keep its place in the **walk**, though:
        // a greyed control cannot take the focus, so a walk that stopped on one
        // would stop for good. See `XgViewApp::end_frame`, which hands a focus on a
        // control that has just gone grey to the next one that is still live.
        let muted = muted.get(index).copied().unwrap_or(false);
        // The word a viewer who does not know the shape reads on hover. It is
        // the caller's, because what the control does is the caller's to know -
        // a play button that will pause is a word the bar cannot read off a
        // picture of a triangle.
        let tip = tips.get(index).map(String::as_str).unwrap_or("");
        let response = match control {
            Control::Icon(icon) => {
                icons::button_in(ui, *rect, self::control(index), *icon, false, muted, tip, rotated)
            }
            Control::Label(text) => {
                icons::label_in(ui, *rect, self::control(index), text, tip, rotated, muted)
            }
            Control::Progress(progress) => icons::label_in(
                ui,
                *rect,
                self::control(index),
                &progress_text(*progress),
                tip,
                rotated,
                muted,
            ),
            Control::Hangup => {
                icons::danger_in(ui, *rect, self::control(index), Icon::Hangup, tip, rotated)
            }
            // A stream with no audio track still has the switch's place, and
            // the place says what is missing: a speaker drawn in the dim colour
            // a greyed control wears. It is not the caller's `muted` flag that
            // makes it inert - that flag is for a control grey only for a while
            // - but the place itself, which never carries an action. So it
            // takes no press and no stop on the walk: a finger on it reaches
            // the picture under the strip, and the remote passes over it.
            Control::Silent => {
                let _ = icons::button_in(
                    ui,
                    *rect,
                    self::control(index),
                    Icon::Mute,
                    false,
                    true,
                    tip,
                    rotated,
                );
                continue;
            }
            // The strip is waiting for the box. The turning mark is the answer
            // to "was my press heard?", and the seat keeps the keys: no press,
            // no stop on the walk, nothing to activate beside it.
            Control::Waiting => {
                let _ = icons::waiting_in(ui, *rect, self::control(index), tip);
                continue;
            }
        };
        if !muted {
            nav.item(&response);
        }
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

    /// Bytes counted while the file is being sent.
    fn at(received: u64, total: Option<u64>) -> Progress {
        Progress { received, total, preparing: false }
    }

    /// A control is known by its place, and the seat is not one of them: the
    /// strip remembers the place the remote came from, and hands it back there.
    #[test]
    fn a_control_is_known_by_its_place() {
        assert_eq!(index_of(control(0)), Some(0));
        assert_eq!(index_of(control(7)), Some(7));
        assert_eq!(index_of(seat()), None, "the seat is not a control");
        assert_eq!(index_of(Id::new("somewhere else")), None);
        assert!(owns(seat()) && owns(control(11)), "and both are still the bar's");
    }

    /// A greyed control hands the focus on: to the next live one, round the end
    /// when it is the last, and to nothing at all when the row is all grey.
    #[test]
    fn a_greyed_control_hands_the_focus_on() {
        assert_eq!(next_live(&[true, true, false, true], 2), Some(3), "the next one");
        assert_eq!(next_live(&[true, true, false], 2), Some(0), "round the end");
        assert_eq!(next_live(&[true, false, false, true], 1), Some(3), "a run of grey");
        assert_eq!(next_live(&[false, false], 0), None, "nowhere to go");
        assert_eq!(next_live(&[], 0), None, "and no row at all");
    }

    fn portrait() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(1080.0, 1920.0))
    }

    fn inside(outer: Rect, inner: Rect) -> bool {
        outer.contains(inner.min) && outer.contains(inner.max)
    }

    fn placed(tile: Rect, rotated: bool, dock: Dock, widths: &[f32]) -> Layout {
        layout(tile, rotated, dock, EDGE, widths, &[], false)
    }

    /// The same, with a timeline row under the controls.
    fn playing(tile: Rect, dock: Dock, widths: &[f32]) -> Layout {
        layout(tile, false, dock, EDGE, widths, &[], true)
    }

    /// With a timeline the panel is deeper and wider, and its controls rise by
    /// one row: the strip grows under them rather than moving them to another
    /// edge, so a press that enters playback does not move what the finger was
    /// on.
    #[test]
    fn a_timeline_deepens_the_panel_and_raises_the_controls() {
        let widths = [icon_width(), icon_width()];
        let plain = placed(landscape(), false, Dock::Bottom, &widths);
        let full = playing(landscape(), Dock::Bottom, &widths);

        // The timeline and the gap above it, less the inset it replaces at the
        // docked edge: with a row under the controls the panel's own edge is the
        // row's, and the inset is only at the far end.
        let grows = TIMELINE + ROW_GAP - INNER;
        assert!((full.bar.height() - plain.bar.height() - grows).abs() < 0.5);
        // The edge it is docked to has not moved; only the far side has.
        assert!((full.bar.bottom() - plain.bar.bottom()).abs() < 0.5);
        assert!((plain.buttons[0].top() - full.buttons[0].top() - grows).abs() < 0.5);

        let row = full.timeline.expect("a timeline");
        assert!(row.top() >= full.buttons[0].bottom(), "the row is under the controls");
        assert!(inside(full.bar, row));
        assert!(plain.timeline.is_none());
        // Wider, because a timeline is read across the picture and the controls
        // are centred in whatever room it needs.
        assert!(full.bar.width() > plain.bar.width());
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

    /// A press on a control is that control's, the strip's padding is the
    /// strip's, and anything else is the picture's - the tap that opens and
    /// closes the bar is read from there.
    #[test]
    fn a_press_is_read_from_the_control_outwards() {
        let one = [icon_width()];
        let layout = placed(landscape(), false, Dock::Top, &one);
        let button = layout.buttons[0].center();
        assert_eq!(layout.hit(button), Hit::Button(0));
        // The padding between the strip's edge and the control is the strip's.
        let padding = Pos2::new(layout.bar.left() + 2.0, layout.bar.center().y);
        assert_eq!(layout.hit(padding), Hit::Inside);
        assert_eq!(layout.hit(Pos2::new(960.0, 540.0)), Hit::Outside);
        // The old bands are the picture's now: a press there is a tap on the
        // picture, which is what opens the strip.
        assert_eq!(layout.hit(Pos2::new(4.0, EDGE)), Hit::Outside);
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

    /// A bar waiting for an answer is not an idle bar.
    ///
    /// The box takes as long as it takes to answer a question about a
    /// recording, and the bar is where the answer lands: eight seconds of
    /// silence would take it down in the middle of the wait, and the viewer's
    /// cursor with it. So a question on its way holds the bar up - for a while
    /// rather than for ever - and the wall is asked to come back when the hold
    /// runs out, which is what the time left is for. The answer then hands the
    /// bar's own clock back, starting it again: a bar that had been waiting
    /// half a minute is not a bar the viewer has stopped using.
    #[test]
    fn a_held_bar_waits_for_its_answer() {
        let now = Instant::now();
        let mut bar = Bar::new();
        bar.show(Dock::Bottom, now);
        // Eight seconds of no answer to it, and it is on its way down.
        assert!(bar.remaining(now + IDLE - Duration::from_millis(100)).is_some());
        assert_eq!(bar.remaining(now + IDLE).unwrap(), Duration::ZERO);

        // A question on its way holds it up, and the time it has left is the
        // time the hold has left.
        bar.hold(now);
        assert!(bar.held(now + HOLD - Duration::from_secs(1)));
        assert!(bar.remaining(now).unwrap() > IDLE, "the hold is the longer clock");
        // And the wall is asked to come back when the hold runs out, which is
        // what the time left is for: the idle has long since run out, and the
        // bar is still up.
        assert_eq!(bar.remaining(now + HOLD - Duration::from_secs(1)).unwrap(), Duration::from_secs(1));
        // The hold is for a while, not for ever: a question the box never
        // answers does not keep the bar up for good.
        assert!(!bar.held(now + HOLD + Duration::from_secs(1)));

        // The answer hands the clock back, and the bar gets a new idle of its
        // own rather than the one it had before the question.
        bar.release(now + HOLD);
        assert!(!bar.held(now + HOLD + Duration::from_secs(1)));
        assert!(bar.remaining(now + HOLD + Duration::from_secs(1)).unwrap() >= Duration::from_secs(7));
        // And a bar that is put away is a bar with nothing to hold up.
        bar.hold(now + HOLD);
        bar.hide();
        assert!(!bar.held(now + HOLD + Duration::from_secs(1)));
        assert!(bar.remaining(now + HOLD + Duration::from_secs(1)).is_none());
    }

    /// The row is given to any tile with room for both of them, wherever the
    /// picture sits in it: a landscape camera fills a landscape tile, and a wall
    /// with no timeline at all is a recording that cannot be scrubbed. Turned,
    /// the depth runs along the width instead.
    #[test]
    fn a_timeline_is_given_to_a_tile_with_room_for_both_rows() {
        let five = [icon_width(); 5];
        assert!(fits_timeline(landscape(), false, EDGE, &five, &[]));
        assert!(fits_timeline(portrait(), false, EDGE, &five, &[]));
        assert!(fits_timeline(portrait(), true, EDGE, &five, &[]));
        assert!(fits_timeline(landscape(), true, EDGE, &five, &[]));

        // A strip of a tile has room for the controls and not for the row.
        let shallow = Rect::from_min_size(Pos2::ZERO, vec2(1920.0, 100.0));
        assert!(!fits_timeline(shallow, false, EDGE, &five, &[]));
        assert!(fits_timeline(shallow, true, EDGE, &five, &[]), "its width is the depth when turned");
        let narrow = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 1920.0));
        assert!(!fits_timeline(narrow, true, EDGE, &five, &[]));
        assert!(fits_timeline(narrow, false, EDGE, &five, &[]));
        // And the edge it keeps clear of the picture edge is part of the room.
        let exact = Rect::from_min_size(Pos2::ZERO, vec2(1920.0, EDGE + FULL));
        assert!(fits_timeline(exact, false, EDGE, &five, &[]));
        assert!(!fits_timeline(exact.shrink2(vec2(0.0, 1.0)), false, EDGE, &five, &[]), "a point short");

        // Wrapping makes the panel deeper, so a tile that holds one row of five
        // need not hold two.
        let eight = [icon_width(); 8];
        let narrow_tile = Rect::from_min_size(Pos2::ZERO, vec2(320.0, 150.0));
        assert!(fits_timeline(narrow_tile, false, EDGE, &five, &[]));
        assert!(!fits_timeline(narrow_tile, false, EDGE, &eight, &[]), "a second line needs 64 more");
    }

    /// How deep the panel is can be asked of the width alone, before there is a
    /// tile to lay the bar out in - and the answer is the one `layout` gives.
    /// That is what lets a caller give the strip its own band of the screen and
    /// place the picture above it in one pass rather than two: the band is
    /// measured from a width, and no height is consulted anywhere along the way.
    #[test]
    fn the_panels_depth_is_a_question_for_the_width_alone() {
        let three = [icon_width(); 3];
        // A recording on a phone: thirteen controls on the 363 points a 411-wide
        // picture leaves, broken into the three lines `tile_bar_break` names.
        let thirteen = [icon_width(); 13];
        let breaks = [1, 7];
        let phone = 411.0;

        assert_eq!(depth(1, false), 52.0, "one line");
        assert_eq!(depth(1, true), 109.0, "one line and a timeline");
        assert_eq!(depth(3, false), 146.0, "three lines");
        assert_eq!(depth(3, true), 203.0, "three lines and a timeline");

        assert_eq!(panel_depth(phone, &three, &[], false), 52.0);
        assert_eq!(panel_depth(phone, &three, &[], true), 109.0);
        assert_eq!(panel_depth(phone, &thirteen, &breaks, false), 146.0);
        assert_eq!(panel_depth(phone, &thirteen, &breaks, true), 203.0);

        // And the same answer the bar is laid out at, whatever height the tile
        // is given: this is a question for the width, and only the width.
        for height in [200.0, 800.0, 1920.0] {
            let tile = Rect::from_min_size(Pos2::ZERO, vec2(phone, height));
            let laid = layout(tile, false, Dock::Bottom, EDGE, &thirteen, &breaks, true);
            let asked = panel_depth(phone, &thirteen, &breaks, true);
            assert!((laid.bar.height() - asked).abs() < 0.01, "height {height}");
        }
    }

    /// The controls keep a little off the panel's own edge: at both ends when
    /// there is nothing under them, and at the far end only when a timeline is -
    /// the row is where the panel's edge is then.
    #[test]
    fn the_controls_keep_off_the_panels_edges() {
        let widths = [icon_width(); 2];
        // The inset, and the slack a control has in a line of its own.
        let room = INNER + (THICK - BUTTON_H) * 0.5;
        let plain = placed(landscape(), false, Dock::Bottom, &widths);
        assert!((plain.buttons[0].top() - plain.bar.top() - room).abs() < 0.5, "above");
        assert!((plain.bar.bottom() - plain.buttons[0].bottom() - room).abs() < 0.5, "below");

        let full = playing(landscape(), Dock::Bottom, &widths);
        assert!((full.buttons[0].top() - full.bar.top() - room).abs() < 0.5, "above");
        let row = full.timeline.expect("a timeline");
        assert!((full.bar.bottom() - row.bottom() - PAD).abs() < 0.5, "the row's own inset");
        assert!(row.top() > full.buttons[0].bottom(), "and a gap under the controls");
    }

    /// A line of controls is centred in the panel, not pushed against one end of
    /// it: a timeline makes the panel as wide as the picture, and controls at
    /// the left of that read as belonging to the left.
    #[test]
    fn a_line_of_controls_is_centred_in_the_panel() {
        let widths = [icon_width(); 3];
        let centred = playing(landscape(), Dock::Bottom, &widths);
        let first = centred.buttons[0];
        let last = centred.buttons[2];
        let left = first.left() - centred.bar.left();
        let right = centred.bar.right() - last.right();
        assert!((left - right).abs() < 1.0, "equal margins: {left} and {right}");

        // Each line is centred on its own, so two lines of different lengths
        // stack as a block rather than against a common edge.
        let widths = [icon_width(); 5];
        let two = layout(portrait(), false, Dock::Bottom, EDGE, &widths, &[3], true);
        for line in [&two.buttons[..3], &two.buttons[3..]] {
            let first = line[0];
            let last = line[line.len() - 1];
            let left = first.left() - two.bar.left();
            let right = two.bar.right() - last.right();
            assert!((left - right).abs() < 1.0, "line margins: {left} and {right}");
        }
    }

    /// A line of controls breaks where the next one would not fit, and the
    /// panel deepens by a row for each line it takes.
    #[test]
    fn a_narrow_picture_wraps_the_controls() {
        let widths = [icon_width(); 8];
        // A 1920 wide picture holds all eight on one line.
        let wide = layout(landscape(), false, Dock::Bottom, EDGE, &widths, &[], true);
        assert_eq!(wide.buttons.len(), 8);
        assert!(wide.buttons.iter().all(|rect| rect.center().y == wide.buttons[0].center().y));
        // A phone in portrait is 411 wide: 363 for the panel, which is six
        // controls a line - 316 - and not seven, which needs 368.
        let phone = Rect::from_min_size(Pos2::ZERO, vec2(411.0, 800.0));
        let narrow = layout(phone, false, Dock::Bottom, EDGE, &widths, &[], true);
        assert_eq!(narrow.buttons.len(), 8);
        let lines: Vec<f32> = {
            let mut seen: Vec<f32> = Vec::new();
            for rect in &narrow.buttons {
                if !seen.iter().any(|y| (*y - rect.center().y).abs() < 0.5) {
                    seen.push(rect.center().y);
                }
            }
            seen
        };
        assert_eq!(lines.len(), 2, "two lines on a phone");
        assert!(narrow.bar.height() > wide.bar.height(), "and a deeper panel");
    }

    /// The strip is walked by Left and Right whichever way it lies, and the
    /// quarter turn must not have given it a second scope: the two would then
    /// answer different pairs of arrows for the same row of controls.
    #[test]
    fn the_scope_does_not_turn_with_the_picture() {
        assert_eq!(scope(), "tile-bar-row");
        assert_ne!(seat(), control(0), "the seat is not a control");
        assert_ne!(control(0), control(1));
    }

    /// The seat sits in the bar's top-left corner: egui's geometric walk can
    /// only find the first button below it or to its right. All four arrows
    /// mean the same thing here, so the answer must not depend on geometry.
    /// A phone held upright asks for three lines on purpose: the way back to
    /// live alone, then the transport and the save, then the timeline's own
    /// keys. The tile is the whole 1x1 cell, so three rows fit in the letterbox
    /// black below the picture and the timeline keeps its height.
    #[test]
    fn three_breaks_put_a_portrait_recording_on_three_centred_lines() {
        let widths = [icon_width(); CONTROLS];
        let phone = Rect::from_min_size(Pos2::ZERO, vec2(720.0, 1350.0));
        let breaks = [1, 7];
        let laid = layout(phone, false, Dock::Bottom, EDGE, &widths, &breaks, true);
        let mut centres: Vec<f32> = Vec::new();
        for button in &laid.buttons {
            if !centres.iter().any(|y| (*y - button.center().y).abs() < 0.5) {
                centres.push(button.center().y);
            }
        }
        assert_eq!(centres.len(), 3, "three lines, not a greedy two");
        assert!((laid.buttons[0].center().y - centres[0]).abs() < 0.5);
        assert!((laid.buttons[1].center().y - centres[1]).abs() < 0.5);
        assert!((laid.buttons[7].center().y - centres[2]).abs() < 0.5);
        assert!(laid.timeline.is_some(), "the timeline was not squeezed out");
        assert!(fits_timeline(phone, false, EDGE, &widths, &breaks));
        for line in [&laid.buttons[..1], &laid.buttons[1..7], &laid.buttons[7..]] {
            let left = line[0].left() - laid.bar.left();
            let right = laid.bar.right() - line[line.len() - 1].right();
            assert!((left - right).abs() < 1.0, "each line is centred on its own");
        }
    }

    /// A progress word has four characters at most, because the button it goes
    /// in is a forty-point square and a chunked answer has no length to divide.
    #[test]
    fn a_progress_word_fits_its_square() {
        assert_eq!(progress_text(at(0, None)), "0%");
        assert_eq!(progress_text(at(0, Some(4800))), "0%", "a length but no byte yet");
        assert_eq!(progress_text(at(50, Some(100))), "50%");
        assert_eq!(progress_text(at(100, Some(100))), "100%");
        assert_eq!(progress_text(at(3 * 1024 * 1024, None)), "3.0M");
        assert_eq!(progress_text(at(12 * 1024 * 1024, None)), "12M");
    }

    /// The box cuts the file before it sends it, and on a long clip that wait
    /// is most of the download. The button has to say something during it, or
    /// the press looks like it did nothing.
    #[test]
    fn a_clip_being_cut_says_so_rather_than_counting_nothing() {
        let cutting = Progress { received: 0, total: None, preparing: true };
        assert_eq!(progress_text(cutting), "0%");
        assert_eq!(
            progress_text(Progress { preparing: true, ..at(9 * 1024 * 1024, None) }),
            "0%",
            "no byte has arrived while the box is cutting, whatever the count says"
        );
    }

    #[test]
    fn every_arrow_off_the_seat_names_the_first_button() {
        assert_eq!(first_arrow_target(true, Some(seat()), true), Some(control(0)));
        assert_eq!(first_arrow_target(true, Some(seat()), false), None, "not an arrow");
        assert_eq!(first_arrow_target(false, Some(seat()), true), None, "bar not open");
        assert_eq!(first_arrow_target(true, Some(control(2)), true), None, "already on a button");
        assert_eq!(first_arrow_target(true, None, true), None, "nothing focused");
        assert_eq!(first_arrow_target(true, Some(Id::new("toolbar")), true), None);
    }

    /// Down from a control still enters the recording's strip. Down from the
    /// seat does not - it is the first arrow onto the first button, and a
    /// strip that took it left the whole bar without a ring on any control.
    #[test]
    fn the_seats_down_is_not_the_strips_way_in() {
        assert!(!down_enters_strip(Some(seat())), "the seat is not a control");
        assert!(!down_enters_strip(None));
        assert!(!down_enters_strip(Some(Id::new("toolbar"))));
        assert!(down_enters_strip(Some(control(0))));
        assert!(down_enters_strip(Some(control(7))));
    }
}
