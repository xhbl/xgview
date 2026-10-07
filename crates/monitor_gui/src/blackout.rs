//! The blackout: a wall that blanks itself on a schedule.
//!
//! It exists to save the display at night without turning it off. The window
//! stays up and stays on screen, so the monitor is never asked to power down
//! and come back - waking is the part that is not reliable - and what stops is
//! the work: the scheduler parks every channel, which closes the camera sockets
//! and drops the decoders (see `monitor_core::scheduler`). All that is left to
//! draw is the local date and time in grey, breathing, wandering slowly across
//! a black wall: enough to show the machine is alive, not enough to light up a
//! dark room.
//!
//! Input brings the wall back. How long it stays back is the schedule's
//! `resume_after_minutes`: the wall blanks again after that many minutes with
//! nobody at it, or - at zero - not until the period ends.

use egui::{Align2, Color32, Context, FontId, Id, LayerId, Order, Vec2};

use monitor_core::config::Blackout;

/// i18n keys of the weekdays, in the order both the schedule's bits and
/// [`monitor_core::blackout::LocalTime::weekday`] are in: bit 0 is Monday.
pub const DAY_KEYS: [&str; 7] = [
    "settings-day-mon",
    "settings-day-tue",
    "settings-day-wed",
    "settings-day-thu",
    "settings-day-fri",
    "settings-day-sat",
    "settings-day-sun",
];

/// Where the mark may wander, as a fraction of the wall, so that all of it
/// stays on screen wherever it is.
const MARGIN: f32 = 0.06;

/// How fast the mark travels, as a fraction of the wall per second. Slow enough
/// that it is never something to watch.
const DRIFT: f32 = 0.030;

/// How close counts as arrived, in fractions of the wall.
const ARRIVAL: f32 = 0.010;

/// Seconds for one breath in and out, and the opacity it moves between.
const BREATH_SECONDS: f32 = 5.0;
const BREATH_LOW: f32 = 0.30;
const BREATH_HIGH: f32 = 0.62;

/// Sizes of the two lines in points, and half the distance between them. The
/// date is the smaller of the two: the time is what is glanced at.
const DATE_SIZE: f32 = 11.0;
const TIME_SIZE: f32 = 17.0;
const LINE_GAP: f32 = 12.0;

/// The grey both lines breathe in.
const TEXT_GREY: u8 = 160;

/// How far the pointer has to move before it counts as a viewer, in points.
/// A resting mouse reports a fraction of a point of jitter on some devices.
const POINTER_SLOP: f32 = 3.0;

/// Whether the wall is blank right now, and where the mark on it is.
pub struct BlackoutState {
    /// A period is in force, as of the last time the clock was consulted.
    scheduled: bool,
    /// When input last asked for the wall back, on egui's clock. Cleared when
    /// the period ends, so that the next one starts blank.
    interrupted_at: Option<f64>,
    /// When the schedule was last consulted, on egui's clock.
    checked_at: f64,
    /// Where the mark is, and where it is going, as fractions of the wall.
    mark: Vec2,
    target: Vec2,
    /// State for the wander's generator, which is six lines of arithmetic that
    /// are not worth a dependency.
    seed: u64,
}

impl BlackoutState {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let mut state = Self {
            scheduled: false,
            interrupted_at: None,
            checked_at: f64::NEG_INFINITY,
            mark: Vec2::splat(0.5),
            target: Vec2::splat(0.5),
            seed,
        };
        state.pick_target();
        state
    }

    /// Whether the wall should be blank on this frame: inside a period, and
    /// past the wait that an interruption bought.
    pub fn is_on(&self, blackout: &Blackout, time: f64) -> bool {
        if !self.scheduled {
            return false;
        }
        let Some(interrupted) = self.interrupted_at else {
            return true;
        };
        // Zero means the interruption lasts the period out.
        let wait = f64::from(blackout.resume_after_minutes) * 60.0;
        wait > 0.0 && time - interrupted >= wait
    }

    /// Consults the schedule and reports whether the wall is blank now.
    ///
    /// At most once a second: reading the clock is cheap but not free, and the
    /// answer only ever changes on a minute boundary - thirty asks a second to
    /// be told the same thing is thirty too many. The wait after an
    /// interruption is applied every frame instead, so it comes back on time.
    pub fn evaluate(&mut self, blackout: &Blackout, time: f64) -> bool {
        if time - self.checked_at >= 1.0 {
            self.checked_at = time;
            let scheduled = monitor_core::blackout::active(blackout);
            if scheduled != self.scheduled {
                // Said out loud: this decides whether sockets are open and
                // whether the machine is held awake, and neither leaves a trace
                // of its own with nothing else running.
                tracing::info!(
                    target: "xgview::blackout",
                    scheduled,
                    minute_of_week = ?monitor_core::blackout::minute_of_week(),
                    periods = blackout.entries.len(),
                    "the blackout schedule"
                );
            }
            self.scheduled = scheduled;
            if !self.scheduled {
                // A period that has ended leaves nothing behind. Without this
                // the wall would stay visible through every later period.
                self.interrupted_at = None;
            }
        }
        self.is_on(blackout, time)
    }

    /// Reports whether the viewer asked for the wall back this frame.
    pub fn notice_input(&mut self, ctx: &Context, time: f64) -> bool {
        let why = ctx.input(|input| {
            let event = input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key { .. }
                        | egui::Event::Text(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::MouseWheel { .. }
                        | egui::Event::Zoom(_)
                )
            });
            if input.pointer.any_down() {
                "the pointer is down"
            } else if input.pointer.delta().length() > POINTER_SLOP {
                "the pointer moved"
            } else if input.raw_scroll_delta != Vec2::ZERO {
                "the wheel turned"
            } else if !input.keys_down.is_empty() {
                "a key is held"
            } else if event {
                "an event arrived"
            } else {
                ""
            }
        });
        if why.is_empty() {
            return false;
        }
        tracing::info!(target: "xgview::blackout", why, "input asked for the wall back");
        self.interrupted_at = Some(time);
        true
    }

    /// Moves the mark along, and picks somewhere else to go when it arrives.
    pub fn advance(&mut self, dt: f32) {
        let step = DRIFT * dt;
        let towards = self.target - self.mark;
        if towards.length() <= step.max(ARRIVAL) {
            self.mark = self.target;
            self.pick_target();
        } else {
            self.mark += towards.normalized() * step;
        }
        // Whatever the arithmetic has done, the mark stays inside the margin.
        let low = Vec2::splat(MARGIN);
        self.mark = self.mark.clamp(low, Vec2::splat(1.0 - MARGIN));
    }

    /// Paints the blank over everything, and the clock on it.
    ///
    /// It is drawn on its own layer above `Order::Foreground` rather than into
    /// the wall's panel, because the toolbar and the status bar are panels of
    /// the same window and have to go under it as well.
    pub fn draw(&self, ctx: &Context, time: f64) {
        let wall = ctx.screen_rect();
        let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("xgview-blackout")));
        painter.rect_filled(wall, egui::CornerRadius::ZERO, Color32::BLACK);

        // A build with no clock to read says nothing rather than guessing.
        let Some(now) = monitor_core::blackout::local_now() else {
            return;
        };
        let breath = (time as f32 * std::f32::consts::TAU / BREATH_SECONDS).sin() * 0.5 + 0.5;
        let alpha = BREATH_LOW + (BREATH_HIGH - BREATH_LOW) * breath;
        let colour = Color32::from_gray(TEXT_GREY).gamma_multiply(alpha);
        let at = wall.min + Vec2::new(self.mark.x * wall.width(), self.mark.y * wall.height());

        // The date and its weekday above, the time below, both centred on the
        // mark's position so the pair wanders as one.
        let weekday = monitor_i18n::tr(DAY_KEYS[usize::from(now.weekday.min(6))]);
        painter.text(
            at - Vec2::new(0.0, LINE_GAP),
            Align2::CENTER_CENTER,
            format!("{} {weekday}", monitor_core::blackout::format_date(&now)),
            FontId::proportional(DATE_SIZE),
            colour,
        );
        painter.text(
            at + Vec2::new(0.0, LINE_GAP),
            Align2::CENTER_CENTER,
            monitor_core::blackout::format_time(&now),
            FontId::proportional(TIME_SIZE),
            colour,
        );
    }

    fn pick_target(&mut self) {
        let span = 1.0 - 2.0 * MARGIN;
        self.target = Vec2::new(MARGIN + self.random() * span, MARGIN + self.random() * span);
    }

    /// xorshift64, scaled to `0.0 … 1.0`.
    fn random(&mut self) -> f32 {
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed = x;
        (x >> 40) as f32 / (1u32 << 24) as f32
    }
}

impl Default for BlackoutState {
    fn default() -> Self {
        Self::new()
    }
}
