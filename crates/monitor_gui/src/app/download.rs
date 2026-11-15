//! Saving a recording: the state a download owns, and the task that runs it.
//!
//! A download does not belong to playback. [`super::playback`] is taken apart
//! by `leave_playback()`, but a clip the viewer asked for keeps arriving while
//! they look at the live wall, and the button has to remember it when they come
//! back - even if they come back on another camera. So the state sits on
//! [`XgViewApp`] beside the event channels, and `poll_events()` drains its
//! answers on every frame whether or not playback is open.
//!
//! There is deliberately no `Drop` here. On Android `quit()` ends the process
//! with `exit`, and a renderer that died ends it with `_exit`: neither runs a
//! destructor. What makes that safe is the file protocol in
//! `monitor_core::recording_download` - only a `.part` is ever written, and a
//! whole response is what renames it. A killed process leaves an orphan that
//! the next start sweeps away; it never leaves half an mp4.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use monitor_core::discovery::frigate::Segment;
use monitor_core::recording_download::{
    self as rules, ClipRequest, ClipWindow, DownloadedClip, Progress, DURATION_CHOICES,
    DEFAULT_DURATION,
};

use crate::dialogs::BackgroundEvent;
use crate::icons::{self, Icon};
use crate::nav::{Axis, Dir, Exit, ScopeDef};
use crate::{dialogs, theme, tilebar};
use super::{Answer, ToastKind, XgViewApp};

/// How long the download button goes on showing what the last one did.
///
/// The toast says where a finished clip went, but a toast expires. A viewer who
/// was on the live wall when the download ended still deserves to find out what
/// happened when they next open playback; the button's tip carries it briefly,
/// and then goes back to being a button.
const HINT_WINDOW: Duration = Duration::from_secs(6);

/// Whether the recording's bar is put back up this frame.
///
/// The save panel is drawn over the wall and takes the magnified bar down with
/// it, so the frame the panel goes away is the frame the bar comes back - and
/// that frame only. A bar put up whenever the panel was *away* stood again on
/// the next frame after Back put it down, and a recording's bar could not be
/// dismissed at all. Nothing is put back if the viewer has meanwhile left the
/// recording: `recording` is `playback.is_some()` and `magnified` is the
/// scheduler's own answer.
fn bar_returns_after(was_open: bool, is_open: bool, recording: bool, magnified: bool) -> bool {
    was_open && !is_open && recording && magnified
}

/// Where the focus goes back to when the panel closes, as a pure rule.
///
/// The button that opened it - but only when the focus was standing on that
/// button already, which is what a remote walking onto it leaves behind. A
/// pointer press moves no focus anywhere in this program, so a panel it opened
/// must not appear to move it either: the ring would come up on a button nobody
/// had aimed at, and stay lit for the ring's whole idle after the panel had
/// gone. See [`super::Modal::from`], which states the same rule for every window
/// here, and the toolbar's layout picker, which reads it off the press itself.
fn focus_returns_to(button: Option<egui::Id>, focused: Option<egui::Id>) -> Option<egui::Id> {
    button.filter(|id| focused == Some(*id))
}

/// What the last download answered, while it is still worth showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Last {
    Done { path: String, at: Instant },
    Failed { reason: String, at: Instant },
}

/// One download in flight.
struct Active {
    id: u64,
    /// The camera label at the moment the download was confirmed.
    camera: String,
    progress: Progress,
    /// Set on the way out. Checked between chunks and at every poll, so the
    /// task can still remove the export it made on the box and its own part.
    /// There is deliberately no abort handle: cancelling the future at its
    /// next await point would skip both of those.
    cancel: Arc<AtomicBool>,
}

/// Everything the wall knows about saving clips.
///
/// One download at a time: the button is muted while this is active, so there
/// is no queue to keep. `id` numbers the task so a late answer from one that
/// was cancelled cannot write its result over the next one's.
#[derive(Default)]
pub(super) struct DownloadState {
    next_id: u64,
    active: Option<Active>,
    last: Option<Last>,
}

impl DownloadState {
    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Reserves the number a task will answer with, before the task is spawned.
    pub(super) fn allocate_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub(super) fn start(&mut self, id: u64, camera: String, cancel: Arc<AtomicBool>) {
        self.active = Some(Active { id, camera, progress: Progress::default(), cancel });
        self.last = None;
    }

    fn current(&self, id: u64) -> bool {
        self.active.as_ref().is_some_and(|active| active.id == id)
    }

    /// What the button draws while a clip is arriving.
    pub(super) fn progress(&self) -> Option<Progress> {
        self.active.as_ref().map(|active| active.progress)
    }

    /// Whose clip it is, for a tip that has to name the camera.
    pub(super) fn camera(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.camera.as_str())
    }

    /// The last answer, while it is recent enough to still be news.
    pub(super) fn hint(&self) -> Option<&Last> {
        self.last.as_ref().filter(|last| {
            let at = match last {
                Last::Done { at, .. } | Last::Failed { at, .. } => at,
            };
            at.elapsed() < HINT_WINDOW
        })
    }

    /// A progress report, dropped when it belongs to a task no longer current.
    pub(super) fn note_progress(&mut self, id: u64, progress: Progress) -> bool {
        let Some(active) = self.active.as_mut().filter(|active| active.id == id) else {
            return false;
        };
        active.progress = progress;
        true
    }

    /// A whole clip, published under its final name.
    pub(super) fn finish_done(&mut self, id: u64, path: String) -> bool {
        self.finish(id, Last::Done { path, at: Instant::now() })
    }

    /// A download that failed, with the reason the toast and the tip show.
    pub(super) fn finish_failed(&mut self, id: u64, reason: String) -> bool {
        self.finish(id, Last::Failed { reason, at: Instant::now() })
    }

    fn finish(&mut self, id: u64, last: Last) -> bool {
        if !self.current(id) {
            return false;
        }
        self.active = None;
        self.last = Some(last);
        true
    }

    /// Stops a running download on the way out.
    ///
    /// Called from `quit()` and from `close_requested()`. Only the flag is set:
    /// aborting the task at its next await point would drop the future before
    /// it could delete the export it made on the box or remove its own `.part`.
    /// A process that stays up lets the task unwind and clean up. One ended
    /// outright cannot, and its orphaned part is swept at the next start;
    /// the box's export is then the one thing no client can make atomic with
    /// its own death.
    pub(super) fn cancel(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Where a saved clip goes, and the scratch directory its `.part` lives in.
///
/// On the desktop both are under the viewer's home; on Android the scratch
/// directory is the app's private files directory, and the public
/// `Download/xgview` folder is where the finished file is copied to. The probe
/// writes a file, so an unwritable home is reported before a request is made.
pub(super) fn download_dirs() -> Result<(PathBuf, PathBuf), String> {
    #[cfg(not(target_os = "android"))]
    {
        rules::desktop_dirs().map_err(|err| err.to_string())
    }
    #[cfg(target_os = "android")]
    {
        crate::android::download_dirs()
    }
}

/// Sweeps away `.part` files a process that was killed could not clean up.
pub(super) fn cleanup_on_start() {
    let Ok((_, parts)) = download_dirs() else {
        return;
    };
    let swept = rules::clean_stale_parts(&parts, rules::PART_LIFETIME);
    if swept > 0 {
        tracing::info!(target: "xgview::app", swept, "old download parts swept away");
    }
}

/// Gives a complete `.part` its public name.
fn publish(clip: &DownloadedClip, dir: &Path) -> Result<String, String> {
    #[cfg(not(target_os = "android"))]
    {
        rules::publish_desktop(&clip.temp, dir, &clip.name)
            .map(|path| path.display().to_string())
            .map_err(|err| err.to_string())
    }
    #[cfg(target_os = "android")]
    {
        // `dir` is the public folder's path, shown in the panel. On Android
        // the bytes reach it through the activity's MediaStore bridge, which
        // knows the finished name it made; a `std::fs` rename would land in a
        // folder the system cannot see.
        let _ = dir;
        crate::android::publish_download(&clip.temp, &clip.name)
    }
}

impl XgViewApp {
    /// Confirms one clip and lets it run on its own task.
    ///
    /// A second confirmation while one is in flight is refused here as well as
    /// at the button: the button is muted, but a key held down or a path around
    /// the focus walk must not start two tasks for the same `id`.
    pub(super) fn start_download(&mut self, request: ClipRequest, camera: String) {
        tracing::info!(target: "xgview::app", camera = %camera, name = %request.name, "saving a clip");
        if self.download.is_active() {
            tracing::info!(target: "xgview::app", "a download is already running");
            return;
        }
        // Android 9 cannot reach the public Downloads folder without a grant.
        // Asking for it at publish time would throw away a clip that had
        // already been cut and fetched, so the question comes first.
        #[cfg(target_os = "android")]
        if !crate::android::storage_granted() {
            crate::android::request_storage();
            self.flash(monitor_i18n::tr("download-no-storage"), ToastKind::Error);
            return;
        }
        let (dir, parts) = match download_dirs() {
            Ok(dirs) => dirs,
            Err(err) => {
                self.flash(
                    monitor_i18n::tr_args("toast-download-failed", &[("error", err.into())]),
                    ToastKind::Error,
                );
                return;
            }
        };
        let id = self.download.allocate_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let task_cancel = cancel.clone();
        let events = self.events_tx.clone();
        let task = self.handle.spawn(async move {
            let progress_events = events.clone();
            let result = rules::download_clip(&request, &parts, task_cancel, move |progress| {
                let _ = progress_events.send(BackgroundEvent::DownloadProgress { id, progress });
            })
            .await;
            let answer = match result {
                Ok(clip) => match publish(&clip, &dir) {
                    Ok(path) => {
                        // A desktop publish renames the part away. The Android
                        // bridge copies it into MediaStore instead, so without
                        // this a second full-size mp4 would sit in the app's
                        // own files after every successful save.
                        let _ = std::fs::remove_file(&clip.temp);
                        BackgroundEvent::DownloadDone { id, path }
                    }
                    Err(reason) => {
                        // Publishing is retryable - a storage grant withdrawn
                        // mid-download, a full disk, a provider that refused
                        // the row - while the bytes here are whole and the
                        // box's export has already been taken away. Keep the
                        // part rather than lose the only copy; the next
                        // start's sweep owns it after a day.
                        tracing::warn!(
                            target: "xgview::app",
                            path = %clip.temp.display(),
                            %reason,
                            "the whole clip was kept in the scratch directory"
                        );
                        BackgroundEvent::DownloadFailed { id, reason }
                    }
                },
                Err(err) => BackgroundEvent::DownloadFailed { id, reason: err.to_string() },
            };
            let _ = events.send(answer);
        });
        // Dropping the handle detaches the task: it keeps running, and on the
        // way out it still has the chance to remove the export and its part.
        drop(task);
        self.download.start(id, camera, cancel);
    }
}

/// The panel's navigation scopes, declared beside the panel they walk.
///
/// The lengths and the button under them are two scopes rather than one: the
/// three lengths are a row on every screen - a phone held upright has the width
/// for `[1 minute] [5 minutes] [10 minutes]`, and stacking them made the panel
/// longer than the choice was worth - and the walk along that row is Left /
/// Right. What joins the two is where Down goes, to the one button, and where Up
/// out of it goes, back to the lengths. A scope naming an exit that does not
/// exist is a dead key rather than a compile error, so the test at the foot of
/// this file walks them.
const LENGTHS: &str = "download-lengths";
const SAVE: &str = "download-save";
/// The same two scopes as a viewer holding a turned picture reads them.
///
/// A quarter turn clockwise puts the panel's own rows down the screen: what
/// was a row of lengths is a column, so Up / Down pick one, and what the viewer
/// calls Down - out of the lengths to the button under them - is a press to the
/// screen's left. `tilebar::scope` reads the same turn for the bar's controls.
const LENGTHS_TURNED: &str = "download-lengths-turned";
const SAVE_TURNED: &str = "download-save-turned";

/// Room under the last control. A level window's frame gives the button only a
/// few points before its edge, while the gap above it is a whole group space;
/// the turned window measures its own height and can give it the same. Both
/// read this one constant, so the button does not sit on the window's bottom
/// edge whichever way the picture is held. Only the theme's smallest gap is
/// needed: half a group space still reads as an empty row under the button.
const SAVE_FOOT: f32 = theme::space::XS;

/// Only a window with a save button has a last control to give room to: a hole
/// in the recording ends in small print and a jump, which have their own gap.
fn save_foot(save: Option<&String>) -> f32 {
    if save.is_some() { SAVE_FOOT } else { 0.0 }
}

pub(super) fn scopes() -> [(&'static str, ScopeDef); 4] {
    [
        (LENGTHS, ScopeDef::new(Axis::Row).scrolling().exit(Dir::Down, Exit::Scope(SAVE))),
        (SAVE, ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::Scope(LENGTHS))),
        (
            LENGTHS_TURNED,
            ScopeDef::new(Axis::Column).exit(Dir::Left, Exit::Scope(SAVE_TURNED)),
        ),
        (SAVE_TURNED, ScopeDef::new(Axis::Column).exit(Dir::Right, Exit::Scope(LENGTHS_TURNED))),
    ]
}

/// Everything the panel has to say this frame, worked out once.
///
/// Two ways draw it: `egui::Modal` on a level picture, and a hand-painted
/// window over one turned a quarter turn, because egui has no rotated layer.
/// What the two say must not drift, so the words, the interval and the name are
/// all made here and both renderers only place them.
struct View {
    camera: String,
    labels: Vec<String>,
    selected: usize,
    plan: ClipWindow,
    /// Small print, in the order it is read.
    notes: Vec<String>,
    /// The two ways out of a hole, and the moment each jumps to.
    jumps: Vec<(String, i64)>,
    /// The word on the button, absent when there is nothing to save.
    save: Option<String>,
    /// The name a confirmation would give the file.
    name: String,
}

impl View {
    fn of(choice: &DownloadChoice) -> Self {
        let seconds = DURATION_CHOICES[choice.choice.min(DURATION_CHOICES.len() - 1)];
        let plan = rules::clamp_window(&choice.stretches, choice.cursor, seconds);
        let labels = DURATION_CHOICES
            .iter()
            .map(|s| monitor_i18n::tr_args("download-minutes", &[("count", (s / 60).into())]))
            .collect();
        let (notes, jumps, save, name) = match plan {
            ClipWindow::Ready { from, to, shortened } => {
                let name = rules::clip_file_name(
                    &choice.camera,
                    &choice.frigate_camera,
                    from,
                    to - from,
                );
                let mut notes = vec![
                    monitor_i18n::tr_args(
                        "download-range",
                        &[("from", stamp(from).into()), ("to", stamp(to).into())],
                    ),
                    monitor_i18n::tr_args("download-actual", &[("seconds", (to - from).into())]),
                ];
                if shortened {
                    notes.push(monitor_i18n::tr("download-shortened"));
                }
                notes.push(monitor_i18n::tr_args("download-file", &[("name", name.clone().into())]));
                notes.push(monitor_i18n::tr_args(
                    "download-to",
                    &[("path", choice.dir.clone().into())],
                ));
                (notes, Vec::new(), Some(monitor_i18n::tr("download-confirm")), name)
            }
            ClipWindow::Gap { before, after } => {
                let mut jumps = Vec::new();
                if let Some(at) = before {
                    jumps.push((monitor_i18n::tr("download-jump-before"), at));
                }
                if let Some(at) = after {
                    jumps.push((monitor_i18n::tr("download-jump-after"), at));
                }
                (vec![monitor_i18n::tr("download-gap")], jumps, None, String::new())
            }
            ClipWindow::Empty => {
                (vec![monitor_i18n::tr("download-none")], Vec::new(), None, String::new())
            }
        };
        Self {
            camera: choice.camera.clone(),
            labels,
            selected: choice.choice,
            plan,
            notes,
            jumps,
            save,
            name,
        }
    }

    fn ready(&self) -> Option<(i64, i64)> {
        match self.plan {
            ClipWindow::Ready { from, to, .. } => Some((from, to)),
            _ => None,
        }
    }
}

/// What either renderer's controls answered this frame.
#[derive(Default)]
struct Actions {
    selected: Option<usize>,
    confirm: bool,
    close: bool,
    jump: Option<i64>,
}

/// The one answer to a press, whichever way the panel was drawn.
fn resolve(
    app: &mut XgViewApp,
    choice: &mut DownloadChoice,
    view: &View,
    actions: Actions,
) -> Answer {
    if let Some(index) = actions.selected {
        choice.choice = index;
    }
    if actions.confirm {
        tracing::info!(target: "xgview::app", ready = view.ready().is_some(), "the save panel was confirmed");
        if let Some((from, to)) = view.ready() {
            let camera = choice.camera.clone();
            app.start_download(
                ClipRequest {
                    base_url: choice.base_url.clone(),
                    camera: choice.frigate_camera.clone(),
                    from,
                    to,
                    credentials: choice.credentials.clone(),
                    name: view.name.clone(),
                },
                camera,
            );
            return Answer::CLOSE;
        }
    }
    if let Some(at) = actions.jump {
        app.seek(at);
        return Answer::CLOSE;
    }
    if actions.close {
        Answer::CLOSE
    } else {
        Answer::STAY
    }
}

/// A point of a turned picture's frame on the screen.
///
/// The picture's own frame swaps the screen's sides - its `x` runs down the
/// screen and its `y` runs left from the tile's right edge. `tilebar::to_screen`
/// puts the bar in the same frame, and `grid` the corners and the wording.
fn turned_at(tile: egui::Rect, p: egui::Pos2) -> egui::Pos2 {
    egui::pos2(tile.right() - p.y, tile.top() + p.x)
}

/// [`turned_at`] for a rectangle: a quarter turn keeps an axis-aligned
/// rectangle axis-aligned, so both corners are enough.
fn turned_rect(tile: egui::Rect, r: egui::Rect) -> egui::Rect {
    egui::Rect::from_two_pos(turned_at(tile, r.min), turned_at(tile, r.max))
}

/// The picture's own frame: the tile's sides, traded.
fn turned_frame(tile: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(tile.height(), tile.width()))
}

/// A panel centred in that frame, never bigger than the frame itself.
fn turned_panel_box(frame: egui::Rect, size: egui::Vec2) -> egui::Rect {
    let size = egui::vec2(size.x.min(frame.width()), size.y.min(frame.height()));
    egui::Rect::from_center_size(frame.center(), size)
}

/// Lines laid out in the panel's own frame; a turned line reads down the
/// screen, so the galley's width is still the width it is wrapped at.
fn turned_galley(
    ctx: &egui::Context,
    text: &str,
    font: &egui::FontId,
    color: egui::Color32,
    wrap: f32,
) -> std::sync::Arc<egui::Galley> {
    ctx.fonts(|fonts| fonts.layout(text.to_owned(), font.clone(), color, wrap))
}

/// One line of the turned panel. A galley's own `x` is the reading direction,
/// which on the screen is down, so the text starts at the local top-left and is
/// drawn with a quarter turn about it - exactly as `chrome::paint_toast_turned`
/// places a turned toast.
fn turned_text(
    ui: &egui::Ui,
    tile: egui::Rect,
    local: egui::Rect,
    galley: std::sync::Arc<egui::Galley>,
    color: egui::Color32,
) {
    let at = turned_at(
        tile,
        egui::pos2(
            local.min.x,
            local.min.y + (local.height() - galley.size().y) * 0.5,
        ),
    );
    ui.painter().add(
        egui::epaint::TextShape::new(at, galley, color)
            .with_angle(std::f32::consts::FRAC_PI_2),
    );
}

/// The panel on a level picture: an `egui::Modal`, whose backdrop and keyboard
/// behaviour are the same as every other window of this program.
fn draw_level(
    app: &mut XgViewApp,
    ctx: &egui::Context,
    width: f32,
    max_height: f32,
    view: &View,
) -> Actions {
    let mut actions = Actions::default();
    egui::Modal::new(egui::Id::new("xgview-download-panel")).show(ctx, |ui| {
        ui.set_width(width);
        // The cross stays outside the scroll area, where every other window
        // puts it: a body long enough to scroll must not carry the way out off
        // the top of the screen with it.
        let bar = dialogs::title_bar(
            ui,
            &monitor_i18n::tr("download-title"),
            true,
            &monitor_i18n::tr("dialog-close"),
        );
        if bar.close.is_some_and(|button| button.clicked()) {
            actions.close = true;
        }

        egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
            ui.label(egui::RichText::new(&view.camera).strong());
            ui.add_space(theme::space::M);

            app.nav.open(LENGTHS);
            ui.horizontal(|ui| {
                for (index, label) in view.labels.iter().enumerate() {
                    let button =
                        app.nav.tracked(ui.selectable_label(index == view.selected, label.clone()));
                    if index == view.selected {
                        app.download_panel.anchor = Some(button.id);
                    }
                    if button.clicked() {
                        actions.selected = Some(index);
                    }
                }
            });
            for text in &view.notes {
                note(ui, text.clone());
            }
            for (label, at) in &view.jumps {
                let button = app.nav.tracked(ui.button(label.clone()));
                if button.clicked() {
                    actions.jump = Some(*at);
                }
            }
            app.nav.close();
            ui.add_space(theme::space::M);

            // A button that cannot act is not a control: it takes no focus, so
            // it is registered only when it can be pressed. Its absence is what
            // leaves Down from the last length with nowhere to go, which is the
            // honest answer for a hole.
            if let Some(save_label) = &view.save {
                ui.horizontal(|ui| {
                    ui.add_space(XgViewApp::center_offset(ui, &[save_label]));
                    app.nav.open(SAVE);
                    let save = app.nav.tracked(ui.button(save_label.clone()));
                    // The panel opens on the save: the press that opened it was
                    // a request to save this recording, and the length still
                    // shows what it will save.
                    app.download_panel.anchor = Some(save.id);
                    app.nav.close();
                    if save.clicked() {
                        actions.confirm = true;
                    }
                });
                // Without this the button sits on the frame's own few points of
                // margin, while the jump buttons above it get a whole group
                // space. The turned window adds the same room to its height.
                ui.add_space(save_foot(view.save.as_ref()));
            }
        });
    });
    actions
}

/// A line of small print under the choices.
///
/// The answer to "what exactly will this save" is reference, not a heading: it
/// is read once, against the length chosen, and the same size as the text would
/// put it in competition with the choices above it. Every other window of this
/// program carries its asides this way - see the hints in `dialogs`.
fn note(ui: &mut egui::Ui, text: String) {
    ui.label(egui::RichText::new(text).small().color(theme::TEXT_DIM));
}

/// What the panel was opened on, frozen while the viewer chooses.
///
/// The address, the login and the recorded stretches are copies: a seek, a
/// page turn or a configuration edit while the panel is up must not change what
/// the confirmation will ask the box for. What is *not* frozen is the window:
/// [`rules::clamp_window`] is recomputed each frame, because choosing another
/// length changes where the window has to be slid to stay inside a stretch.
#[derive(Debug, Clone)]
pub(super) struct DownloadChoice {
    camera: String,
    frigate_camera: String,
    base_url: String,
    credentials: Option<(String, String)>,
    cursor: i64,
    stretches: Vec<Segment>,
    choice: usize,
    dir: String,
}

/// The wall clock as the panel spells it: `2026-11-14 16:10:30`.
fn stamp(at: i64) -> String {
    match monitor_core::blackout::local_at(at) {
        Some(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        ),
        None => at.to_string(),
    }
}

impl XgViewApp {
    /// Opens the panel on the moment under the cursor.
    ///
    /// An entirely empty timeline is answered with a toast instead: there is
    /// nothing to choose. A home that cannot be written to is answered here
    /// rather than after a request the box has already started cutting.
    /// `ctx` is read once, for where the focus stood when the panel was asked
    /// for - see the assignment of `from` below.
    pub(super) fn open_download_panel(&mut self, ctx: &egui::Context) {
        if self.download.is_active() {
            return;
        }
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let cursor = self.centre_of(playback);
        let source = playback.source.clone();
        let stretches = playback.stretches.clone();
        if matches!(
            rules::clamp_window(&stretches, cursor, DEFAULT_DURATION),
            ClipWindow::Empty
        ) {
            self.flash(monitor_i18n::tr("download-none"), ToastKind::Error);
            return;
        }
        let dir = match download_dirs() {
            Ok((dir, _)) => dir.display().to_string(),
            Err(err) => {
                self.flash(
                    monitor_i18n::tr_args("toast-download-failed", &[("error", err.into())]),
                    ToastKind::Error,
                );
                return;
            }
        };
        let choice = DURATION_CHOICES.iter().position(|s| *s == DEFAULT_DURATION).unwrap_or(0);
        // The name a person reads, not the identifier the box files the camera
        // under: the panel says whose recording this is, and the file lands in
        // Downloads to be found by a human. The session's own `camera` stays
        // the identifier, because every later answer is checked against it.
        let label = self
            .magnified_camera()
            .map(|camera| {
                if camera.name.trim().is_empty() {
                    camera.id.clone()
                } else {
                    camera.name.clone()
                }
            })
            .unwrap_or_else(|| source.camera.clone());
        tracing::info!(target: "xgview::app", camera = %label, cursor, "download panel opened");
        // The frame that opens the panel has not drawn a control yet, so the
        // anchor it left from a previous visit is not the one to focus now.
        // `hand_off_when` picks it up as soon as this frame's is known.
        self.download_panel.anchor = None;
        // Where the focus goes when the window closes: the button that opened
        // it, as every other window of this program does - but only when the
        // focus stood on that button already. Without this a remote that opened
        // the panel had nowhere to come back to, and the strip had to be armed
        // with Enter again before an arrow did anything; with it unconditional,
        // a panel opened with a finger lit the ring on a button no one had
        // aimed at. See `focus_returns_to`.
        self.download_panel.from =
            focus_returns_to(self.download_button(), ctx.memory(|memory| memory.focused()));
        self.download_panel.open(DownloadChoice {
            camera: label,
            frigate_camera: source.origin.camera.clone(),
            base_url: source.origin.base_url.clone(),
            credentials: source.credentials.clone(),
            cursor,
            stretches,
            choice,
            dir,
        });
    }

    /// Draws the panel that asks how much of the recording to save.
    ///
    /// The actual interval and the file name are shown *before* confirmation,
    /// because a window slid away from a hole is not the moment the viewer
    /// pointed at, and a short stretch is not the length they chose. Both ways
    /// of drawing it - a level `egui::Modal`, or one painted into a picture's
    /// turned frame - read the same [`View`], so neither can say something the
    /// other does not.
    ///
    /// The way out is the cross in the title bar and the Back key, as in every
    /// other window of this program - there is no Cancel button beside the
    /// answer. Back is answered in `handle_keys`, where a window this program
    /// opened takes it before the panel behind it does.
    pub(super) fn download_panel_window(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        let width = 420.0_f32.min(screen.width() - 48.0).max(260.0);
        let max_height = screen.height() * 0.8;
        // A picture turned a quarter turn takes its whole frame with it - the
        // bar, the corners, the toast. A level window over it would be the one
        // thing the viewer has to turn their head for, so this one is laid out
        // in the picture's frame and drawn turned. egui has no rotated layer;
        // see `draw_turned`.
        let turned = self.magnified_rotated();
        let tile = self.magnified_tile().unwrap_or(screen);
        self.modal_lifecycle(|app| &mut app.download_panel, |app, choice| {
            let view = View::of(choice);
            let actions = if turned {
                draw_turned(app, ctx, tile, &view)
            } else {
                draw_level(app, ctx, width, max_height, &view)
            };
            resolve(app, choice, &view, actions)
        });

        // A panel over the wall takes the magnified bar down - it would be in
        // the way of the form - and this is the one panel opened from a
        // recording, so the frame the panel goes away is where the bar comes
        // back. Without it the strip is gone the moment a save is confirmed,
        // and the recording's own controls have to be asked for again with a key
        // press while the clip is still arriving.
        //
        // The frame it *goes*, not every frame it is closed: a bar put up
        // whenever the panel was away stood again on the very next frame after
        // Back put it down, so a recording's bar could not be dismissed at all.
        // The panel is closed in `handle_keys`, before this runs, which is why
        // the answer is remembered from the end of the last frame rather than
        // asked now.
        let was_open = std::mem::replace(
            &mut self.download_panel_was_open,
            self.download_panel.is_open(),
        );
        if bar_returns_after(
            was_open,
            self.download_panel.is_open(),
            self.playback.is_some(),
            self.scheduler.is_zoomed(),
        ) {
            self.tile_bar.show(tilebar::Dock::Bottom, Instant::now());
        }
    }
}

/// The rows of the turned panel's body, in the order a viewer reads them.
///
/// The title is not one of them: it has a bar of its own at the panel's head,
/// laid out by [`turned_title_bar`].
enum TurnedRow {
    Camera,
    Lengths,
    Note(usize),
    Jump(usize),
    Save,
}

/// The panel's own frame over a turned picture: its height and the room around
/// it, in the picture's points.
const TURNED_EDGE: f32 = 20.0;
const TURNED_PAD: f32 = 14.0;
const TURNED_GAP: f32 = 8.0;
const TURNED_ROW_H: f32 = 30.0;

/// The turned window measures its own height: the title bar, its rule, the
/// padding around the body, the rows, and the room under the last control.
fn turned_panel_height(body_height: f32, stroke_width: f32, save: Option<&String>) -> f32 {
    dialogs::TITLE_BAR_H + stroke_width + 2.0 * TURNED_PAD + body_height + save_foot(save)
}

/// The turned panel's title bar, in the picture's own frame.
///
/// A level dialog's [`dialogs::title_bar`] is a strip across the whole window:
/// the words at one end and the cross at the other, with a rule under the
/// whole strip. Turned, that strip runs down the screen instead of across it,
/// but it is still the same bar: the words at its head, the cross at its foot,
/// and the rule between it and the body for its whole length. Its depth, the
/// inset, the cross and the edge around it are the level dialog's own
/// constants, so the two read as one bar seen a quarter turn apart.
struct TurnedTitleBar {
    /// Where the words are laid out; their `x` runs down the screen.
    text: egui::Rect,
    /// The close square, at the bar's far end - never beside the words.
    cross: egui::Rect,
    /// The rule under the whole bar, separating it from the body.
    rule: egui::Rect,
}

/// How far the words may run before they wrap, leaving the bar's far end to
/// the cross - the same room a level title leaves for it.
fn turned_title_wrap(panel_width: f32) -> f32 {
    (panel_width - dialogs::TITLE_INSET - dialogs::TITLE_BAR_H - TURNED_PAD).max(80.0)
}

/// Lays the turned title bar out. `title_width` is the words' galley width and
/// `rule_width` is the style's stroke - the same stroke a level title bar uses.
fn turned_title_bar(panel: egui::Rect, title_width: f32, rule_width: f32) -> TurnedTitleBar {
    let bar = egui::Rect::from_min_size(
        egui::pos2(panel.min.x, panel.min.y),
        egui::vec2(panel.width(), dialogs::TITLE_BAR_H),
    );
    let text_x = bar.min.x + dialogs::TITLE_INSET;
    let cross_x = bar.max.x - dialogs::TITLE_EDGE - dialogs::CLOSE_SIDE;
    TurnedTitleBar {
        text: egui::Rect::from_min_size(
            egui::pos2(text_x, bar.min.y),
            egui::vec2(title_width, bar.height()),
        ),
        cross: egui::Rect::from_min_size(
            egui::pos2(cross_x, bar.min.y + dialogs::TITLE_EDGE),
            egui::vec2(dialogs::CLOSE_SIDE, dialogs::CLOSE_SIDE),
        ),
        rule: egui::Rect::from_min_size(
            egui::pos2(bar.min.x, bar.max.y),
            egui::vec2(bar.width(), rule_width),
        ),
    }
}

/// The save panel painted into a picture's turned frame.
///
/// egui lays every widget out in the screen's axes and has no rotated layer, so
/// an `egui::Modal` here would stand level over a sideways picture. What egui
/// *can* still do is the modal itself: its backdrop, its layer and its presses.
/// This takes that layer and places each control by hand - a local rectangle in
/// the picture's frame, mapped to a screen rectangle by [`turned_rect`], given
/// to `ui.interact` so a finger and the keyboard find it, and painted with the
/// words turned a quarter turn. The bar and the toast over the same picture are
/// placed the same way; see `tilebar::to_screen` and `chrome::paint_toast_turned`.
fn draw_turned(
    app: &mut XgViewApp,
    ctx: &egui::Context,
    tile: egui::Rect,
    view: &View,
) -> Actions {
    let frame = turned_frame(tile);
    let width = 420.0_f32.min(frame.width() - 2.0 * TURNED_EDGE).max(240.0);
    let inner = width - 2.0 * TURNED_PAD;
    let style = ctx.style();
    let heading = style.text_styles[&egui::TextStyle::Heading].clone();
    let body = style.text_styles[&egui::TextStyle::Body].clone();
    let small = style.text_styles[&egui::TextStyle::Small].clone();
    let stroke = style.visuals.widgets.noninteractive.bg_stroke;

    // A turned line is as tall as its own galley, wrapped at the panel's inner
    // width. Measure every body row first: the panel cannot be centred until
    // its height is known, and a line of small print that wraps is two lines
    // tall.
    let mut rows: Vec<(TurnedRow, f32)> = vec![(
        TurnedRow::Camera,
        turned_galley(ctx, &view.camera, &body, theme::TEXT, inner).size().y.max(TURNED_ROW_H),
    )];
    rows.push((TurnedRow::Lengths, TURNED_ROW_H));
    for index in 0..view.notes.len() {
        let h = turned_galley(ctx, &view.notes[index], &small, theme::TEXT_DIM, inner).size().y;
        rows.push((TurnedRow::Note(index), h));
    }
    for index in 0..view.jumps.len() {
        rows.push((TurnedRow::Jump(index), TURNED_ROW_H));
    }
    if view.save.is_some() {
        rows.push((TurnedRow::Save, TURNED_ROW_H));
    }
    // The title is no longer a row: it has its own bar at the panel's head,
    // followed by the rule and then the body, exactly as in a level dialog.
    let body_height = rows.iter().map(|(_, h)| *h).sum::<f32>()
        + TURNED_GAP * rows.len().saturating_sub(1) as f32;
    let height = turned_panel_height(body_height, stroke.width, view.save.as_ref());
    let panel = turned_panel_box(frame, egui::vec2(width, height));
    let title = monitor_i18n::tr("download-title");
    let title_galley = turned_galley(
        ctx,
        &title,
        &heading,
        theme::TEXT,
        turned_title_wrap(panel.width()),
    );
    let title_bar = turned_title_bar(panel, title_galley.size().x, stroke.width);

    let mut actions = Actions::default();
    // The modal keeps its own backdrop and its place above the wall; the frame
    // is ours, so the stock one is taken away. Its content `Ui` is on the
    // modal's foreground layer with the screen as its clip rect, which is all
    // the hand-placed controls below need.
    egui::Modal::new(egui::Id::new("xgview-download-turned"))
        .frame(egui::Frame::NONE)
        .show(ctx, |ui| {
            let popup = egui::Frame::popup(ui.style());
            ui.painter().rect(
                turned_rect(tile, panel),
                popup.corner_radius,
                popup.fill,
                popup.stroke,
                egui::StrokeKind::Inside,
            );

            // The head: the words at one end, the cross at the other, and a
            // rule under the whole of it. A level dialog's title_bar is this
            // same bar seen without the turn, so both use its own stroke,
            // its inset and its cross.
            turned_text(ui, tile, title_bar.text, title_galley.clone(), theme::TEXT);
            let close = icons::button_in(
                ui,
                turned_rect(tile, title_bar.cross),
                egui::Id::new("xgview-download-turned-close"),
                Icon::Close,
                false,
                false,
                &monitor_i18n::tr("dialog-close"),
                true,
            );
            if close.clicked() {
                actions.close = true;
            }
            ui.painter().rect_filled(turned_rect(tile, title_bar.rule), 0.0, stroke.color);

            let mut y = title_bar.rule.max.y + TURNED_PAD;
            let row = |y: f32, h: f32| {
                egui::Rect::from_min_size(
                    egui::pos2(panel.min.x + TURNED_PAD, y),
                    egui::vec2(inner, h),
                )
            };
            for (kind, h) in &rows {
                let local = row(y, *h);
                match kind {
                    TurnedRow::Camera => turned_text(
                        ui,
                        tile,
                        local,
                        turned_galley(ctx, &view.camera, &body, theme::TEXT, inner),
                        theme::TEXT,
                    ),
                    TurnedRow::Lengths => {
                        let gap = theme::space::S;
                        let each = (inner - gap * (view.labels.len().saturating_sub(1) as f32))
                            / view.labels.len().max(1) as f32;
                        app.nav.open(LENGTHS_TURNED);
                        for (index, label) in view.labels.iter().enumerate() {
                            let at = egui::Rect::from_min_size(
                                egui::pos2(local.min.x + index as f32 * (each + gap), y),
                                egui::vec2(each, *h),
                            );
                            let button = app.nav.tracked(icons::selectable_label_in(
                                ui,
                                turned_rect(tile, at),
                                egui::Id::new(("xgview-download-turned-length", index)),
                                label,
                                "",
                                true,
                                false,
                                index == view.selected,
                            ));
                            if index == view.selected {
                                app.download_panel.anchor = Some(button.id);
                            }
                            if button.clicked() {
                                actions.selected = Some(index);
                            }
                        }
                        app.nav.close();
                    }
                    TurnedRow::Note(index) => turned_text(
                        ui,
                        tile,
                        local,
                        turned_galley(ctx, &view.notes[*index], &small, theme::TEXT_DIM, inner),
                        theme::TEXT_DIM,
                    ),
                    TurnedRow::Jump(index) => {
                        let (label, at) = &view.jumps[*index];
                        app.nav.open(LENGTHS_TURNED);
                        let button = app.nav.tracked(icons::selectable_label_in(
                            ui,
                            turned_rect(tile, local),
                            egui::Id::new(("xgview-download-turned-jump", *index)),
                            label,
                            "",
                            true,
                            false,
                            false,
                        ));
                        app.nav.close();
                        if button.clicked() {
                            actions.jump = Some(*at);
                        }
                    }
                    TurnedRow::Save => {
                        let label = view.save.as_deref().unwrap_or_default();
                        let side = inner.min(160.0);
                        let at = egui::Rect::from_min_size(
                            egui::pos2(local.min.x + (inner - side) * 0.5, y),
                            egui::vec2(side, *h),
                        );
                        app.nav.open(SAVE_TURNED);
                        let save = app.nav.tracked(icons::selectable_label_in(
                            ui,
                            turned_rect(tile, at),
                            egui::Id::new("xgview-download-turned-save"),
                            label,
                            "",
                            true,
                            false,
                            false,
                        ));
                        app.download_panel.anchor = Some(save.id);
                        app.nav.close();
                        if save.clicked() {
                            actions.confirm = true;
                        }
                    }
                }
                y += h + TURNED_GAP;
            }
        });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::Nav;
    use egui::Id;

    /// The panel as the walk sees it: three lengths, then the button under them.
    ///
    /// An exit naming a scope nothing declared is not a compile error - it is an
    /// arrow key that quietly does nothing, which on a television cannot be told
    /// from the panel being broken. So the shape is drawn here and walked.
    fn panel(lengths: &'static str, save: &'static str) -> (Nav, Vec<Id>, Id) {
        let lengths_ids: Vec<Id> = (0..3).map(|slot| Id::new(("length", slot))).collect();
        let save_id = Id::new("save");
        let mut nav = Nav::new(&scopes());
        let ctx = egui::Context::default();
        nav.begin();
        nav.open(lengths);
        for id in &lengths_ids {
            nav.item_id(*id);
        }
        nav.close();
        nav.open(save);
        nav.item_id(save_id);
        nav.close();
        nav.finish(&ctx);
        (nav, lengths_ids, save_id)
    }

    #[test]
    fn down_from_the_lengths_reaches_the_button_and_up_comes_back() {
        let (mut nav, ids, save_id) = panel(LENGTHS, SAVE);
        assert_eq!(nav.step(ids[2], Dir::Down), Some(save_id), "Down reaches the button");
        assert_eq!(nav.step(save_id, Dir::Up), Some(ids[0]), "Up comes back to the lengths");
    }

    /// The three lengths are one row on every screen, including a phone held
    /// upright: that is the arrows that pick one, and a vertical press is spent
    /// on the row rather than walking out of it.
    #[test]
    fn the_lengths_are_walked_left_and_right_whatever_the_screen() {
        let (mut nav, ids, save_id) = panel(LENGTHS, SAVE);
        assert_eq!(nav.step(ids[0], Dir::Right), Some(ids[1]));
        assert_eq!(nav.step(ids[1], Dir::Left), Some(ids[0]));
        assert_eq!(nav.step(ids[0], Dir::Up), None, "a row has no walk of its own upwards");
        assert_eq!(nav.step(ids[0], Dir::Down), Some(save_id), "and Down still leaves the row");
    }

    fn begun() -> (DownloadState, u64, Arc<AtomicBool>) {
        let mut state = DownloadState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let id = state.allocate_id();
        state.start(id, "门口".into(), cancel.clone());
        (state, id, cancel)
    }

    /// Leaving must not abort the task: it still owes the box a delete and
    /// the scratch directory its part. Only the flag is set.
    #[test]
    fn cancelling_tells_the_task_to_unwind_without_dropping_it() {
        let (mut state, _, cancel) = begun();
        assert!(!cancel.load(Ordering::Relaxed));
        state.cancel();
        assert!(cancel.load(Ordering::Relaxed), "the task is told to stop");
        assert!(!state.is_active(), "the button is free for the next press");
    }

    #[test]
    fn a_new_download_is_active_and_an_old_answer_is_not_current() {
        let (state, id, _) = begun();
        assert!(state.is_active());
        assert!(state.current(id));
        assert!(!state.current(id - 1));
    }

    #[test]
    fn progress_belongs_to_the_task_that_sent_it() {
        let (mut state, id, _) = begun();
        let progress = Progress { received: 2048, total: None, preparing: false };
        assert!(state.note_progress(id, progress));
        assert_eq!(state.progress(), Some(progress));
        assert!(!state.note_progress(id + 1, Progress { received: 9999, ..progress }));
        assert_eq!(state.progress(), Some(progress), "a stale answer changes nothing");
    }

    #[test]
    fn a_finished_download_leaves_a_short_lived_hint_and_frees_the_button() {
        let (mut state, id, _) = begun();
        assert!(state.finish_done(id, "/home/me/xgview/door.mp4".into()));
        assert!(!state.is_active());
        assert!(matches!(state.hint(), Some(Last::Done { .. })));
        assert!(!state.finish_failed(id, "too late".into()), "one terminal answer only");
    }

    #[test]
    fn a_failed_download_is_remembered_the_same_way() {
        let (mut state, id, _) = begun();
        assert!(state.finish_failed(id, "the box went quiet".into()));
        assert!(matches!(state.hint(), Some(Last::Failed { .. })));
        assert!(!state.is_active(), "a failure is not a download in flight");
    }

    #[test]
    fn cancelling_sets_the_flag_aborts_and_drops_the_state() {
        let (mut state, _, cancel) = begun();
        state.cancel();
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!state.is_active());
        assert!(!state.note_progress(1, Progress::default()), "no task answers a cancelled download");
        state.cancel();
        assert!(cancel.load(Ordering::Relaxed), "cancelling twice is harmless");
    }

    #[test]
    fn a_hint_expires_and_the_button_goes_back_to_idle() {
        let mut state = DownloadState::default();
        let expired = Instant::now() - HINT_WINDOW - Duration::from_secs(1);
        state.last = Some(Last::Done { path: "old.mp4".into(), at: expired });
        assert!(state.hint().is_none());
    }

    #[test]
    fn two_downloads_in_a_row_never_share_a_number() {
        let mut state = DownloadState::default();
        let first = state.allocate_id();
        let second = state.allocate_id();
        assert_ne!(first, second);
        assert!(!state.is_active(), "a number alone does not start anything");
    }

    /// The bar comes back when the panel goes, and never again: a rule that
    /// answered "the panel is not up" instead put the bar back on every frame,
    /// so Back could not dismiss a recording's controls any more.
    #[test]
    fn the_bar_comes_back_on_the_frame_the_panel_goes_away() {
        assert!(
            bar_returns_after(true, false, true, true),
            "the panel has just been closed over a recording"
        );
        assert!(
            !bar_returns_after(false, false, true, true),
            "closed last frame too: the bar is the viewer's to dismiss"
        );
        assert!(!bar_returns_after(true, true, true, true), "still up");
        assert!(!bar_returns_after(true, false, false, true), "left the recording");
        assert!(!bar_returns_after(true, false, true, false), "back on the wall");
    }

    /// A ring comes back to the save button only when a remote left it there.
    /// Handing the focus back unconditionally put it on a button a finger had
    /// pressed, where it stayed lit for the ring's whole idle after the panel
    /// had closed - and a pointer press is not aiming at anything.
    #[test]
    fn only_a_remote_gets_the_ring_back_on_the_save_button() {
        let button = Id::new(("control", 11));
        let seat = Id::new("seat");
        assert_eq!(
            focus_returns_to(Some(button), Some(button)),
            Some(button),
            "the remote walked onto the button, so it goes back there"
        );
        assert_eq!(
            focus_returns_to(Some(button), Some(seat)),
            None,
            "a pointer left the focus on the seat: the panel must not move it"
        );
        assert_eq!(focus_returns_to(Some(button), None), None, "nothing had the focus");
        assert_eq!(focus_returns_to(None, Some(button)), None, "this bar has no save button");
    }

    /// A window over a picture turned a quarter turn is laid out in the
    /// picture's own frame, whose sides are the screen's traded. Its own width
    /// therefore becomes height on the screen, and its centre does not move -
    /// the same mapping the bar and the toast are placed with.
    #[test]
    fn a_turned_panel_lies_across_the_picture() {
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 97.0), egui::vec2(360.0, 700.0));
        let frame = turned_frame(tile);
        assert_eq!(frame.size(), egui::vec2(700.0, 360.0), "the frame's sides trade");
        let panel = turned_panel_box(frame, egui::vec2(420.0, 260.0));
        let on_screen = turned_rect(tile, panel);
        assert_eq!(
            on_screen.size(),
            egui::vec2(260.0, 420.0),
            "what was wide now runs down the screen"
        );
        assert!((on_screen.center().x - tile.center().x).abs() < 0.01, "{on_screen:?}");
        assert!((on_screen.center().y - tile.center().y).abs() < 0.01, "{on_screen:?}");
        assert_eq!(
            turned_at(tile, panel.min),
            egui::pos2(on_screen.max.x, on_screen.min.y),
            "the panel's top-left is the screen's top-right corner"
        );
        let too_big = turned_panel_box(frame, egui::vec2(10000.0, 10000.0));
        assert!(too_big.width() <= frame.width() && too_big.height() <= frame.height());
    }

    /// The arrows over a turned picture walk the panel in the frame the viewer
    /// reads: Up / Down along the row of lengths, Left down to the button, Right
    /// back. A sideways press on the row is spent, not an exit.
    #[test]
    fn a_turned_panel_is_walked_in_the_pictures_frame() {
        let (mut nav, ids, save_id) = panel(LENGTHS_TURNED, SAVE_TURNED);
        assert_eq!(nav.step(ids[0], Dir::Down), Some(ids[1]), "the row runs down the screen");
        assert_eq!(nav.step(ids[1], Dir::Up), Some(ids[0]));
        assert_eq!(nav.step(ids[2], Dir::Left), Some(save_id), "the panel's Down is the screen's Left");
        assert_eq!(nav.step(save_id, Dir::Right), Some(ids[0]), "and its Up is Right");
        assert_eq!(nav.step(ids[0], Dir::Right), None, "a column has no sideways walk");
    }

    /// The turned title bar is the level dialog's bar seen a quarter turn
    /// apart: its depth, the inset before the words, the edge around the cross
    /// and the stroke under it are `dialogs::title_bar`'s own constants. The
    /// cross belongs at the bar's far end - the right of a level title, the
    /// foot of a turned one - not tucked in behind the words.
    #[test]
    fn a_turned_title_bar_puts_its_cross_at_the_far_end() {
        let panel = egui::Rect::from_min_size(egui::pos2(30.0, 40.0), egui::vec2(420.0, 300.0));
        let wrap = turned_title_wrap(panel.width());
        let rule_width = 1.0;
        let bar = turned_title_bar(panel, 190.0_f32.min(wrap), rule_width);

        assert_eq!(bar.text.min.y, panel.min.y, "the bar starts at the panel's own edge");
        assert_eq!(bar.text.height(), dialogs::TITLE_BAR_H, "the level dialog's depth");
        assert_eq!(bar.text.min.x - panel.min.x, dialogs::TITLE_INSET);
        assert_eq!(bar.cross.size(), egui::Vec2::splat(dialogs::CLOSE_SIDE));
        assert_eq!(bar.cross.min.y - panel.min.y, dialogs::TITLE_EDGE);
        assert_eq!(
            bar.cross.max.x,
            panel.max.x - dialogs::TITLE_EDGE,
            "the cross is at the far end, as at the right of a level title"
        );
        assert!(
            bar.cross.min.x > bar.text.max.x + TURNED_GAP,
            "it does not follow the words"
        );
        assert_eq!(bar.rule.min.y, panel.min.y + dialogs::TITLE_BAR_H, "under the bar");
        assert_eq!(bar.rule.height(), rule_width, "the level dialog's own stroke");
        assert_eq!(bar.rule.width(), panel.width(), "the rule runs the whole bar");

        // Even the longest wording stops before the cross: the wrap keeps the
        // bar's far end for it.
        let longest = turned_title_bar(panel, wrap, rule_width);
        assert!(longest.text.max.x < longest.cross.min.x);
    }

    /// On the screen the turned bar runs down the panel: the words at its
    /// head, the cross at its foot, and the rule between it and the body for
    /// the whole length - the level title bar rotated a quarter turn.
    #[test]
    fn a_turned_title_bar_is_the_level_bar_a_quarter_turn_apart() {
        let tile = egui::Rect::from_min_size(egui::pos2(0.0, 97.0), egui::vec2(360.0, 700.0));
        let panel = turned_panel_box(turned_frame(tile), egui::vec2(420.0, 300.0));
        let bar = turned_title_bar(panel, 190.0, 1.0);
        let cross = turned_rect(tile, bar.cross);
        let rule = turned_rect(tile, bar.rule);
        let whole = turned_rect(tile, panel);

        assert_eq!(rule.height(), whole.height(), "the rule runs the panel's whole length");
        assert!(rule.min.x < whole.max.x, "on the inside of the title's own column");
        assert!(cross.min.x >= rule.max.x, "the cross is on the title's side of the rule");
        assert!(
            cross.max.y > whole.max.y - dialogs::TITLE_BAR_H,
            "the cross sits at the foot of the turned bar"
        );
    }

    /// The last control has room under it in both renderers, from one constant:
    /// a level window adds it to the scroll area, a turned one to its measured
    /// height. A hole has no button, so neither window grows for it.
    #[test]
    fn a_save_button_does_not_sit_on_the_windows_edge() {
        let save = String::from("Download");
        assert_eq!(SAVE_FOOT, theme::space::XS, "one small gap, not a group space");
        assert!(SAVE_FOOT < theme::space::M / 2.0, "and less than half of one");
        assert_eq!(save_foot(Some(&save)), SAVE_FOOT);
        assert_eq!(save_foot(None), 0.0, "a hole ends in small print, not a button");

        let body = 180.0;
        let stroke = 1.0;
        let with = turned_panel_height(body, stroke, Some(&save));
        let without = turned_panel_height(body, stroke, None);
        assert_eq!(with - without, SAVE_FOOT, "the same room the level window adds");
        assert_eq!(
            without,
            dialogs::TITLE_BAR_H + stroke + 2.0 * TURNED_PAD + body,
            "and no other part of the window moved"
        );
    }
}
